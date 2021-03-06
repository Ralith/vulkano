// Copyright (c) 2017 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::error::Error;
use std::sync::Arc;

use buffer::BufferAccess;
use command_buffer::submit::SubmitAnyBuilder;
use device::Device;
use device::DeviceOwned;
use device::Queue;
use image::ImageAccess;
use sync::AccessFlagBits;
use sync::GpuFuture;
use sync::PipelineStages;

use VulkanObject;

/// Joins two futures together.
// TODO: handle errors
#[inline]
pub fn join<F, S>(first: F, second: S) -> JoinFuture<F, S>
    where F: GpuFuture, S: GpuFuture
{
    assert_eq!(first.device().internal_object(), second.device().internal_object());

    if !first.queue_change_allowed() && !second.queue_change_allowed() {
        assert!(first.queue().unwrap().is_same(second.queue().unwrap()));
    }

    JoinFuture {
        first: first,
        second: second,
    }
}

/// Two futures joined into one.
#[must_use]
pub struct JoinFuture<A, B> {
    first: A,
    second: B,
}

unsafe impl<A, B> DeviceOwned for JoinFuture<A, B> where A: DeviceOwned, B: DeviceOwned {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        let device = self.first.device();
        debug_assert_eq!(self.second.device().internal_object(), device.internal_object());
        device
    }
}

unsafe impl<A, B> GpuFuture for JoinFuture<A, B> where A: GpuFuture, B: GpuFuture {
    #[inline]
    fn cleanup_finished(&mut self) {
        self.first.cleanup_finished();
        self.second.cleanup_finished();
    }

    #[inline]
    fn flush(&self) -> Result<(), Box<Error>> {
        // Since each future remembers whether it has been flushed, there's no safety issue here
        // if we call this function multiple times.
        try!(self.first.flush());
        try!(self.second.flush());
        Ok(())
    }

    #[inline]
    unsafe fn build_submission(&self) -> Result<SubmitAnyBuilder, Box<Error>> {
        let first = try!(self.first.build_submission());
        let second = try!(self.second.build_submission());

        Ok(match (first, second) {
            (SubmitAnyBuilder::Empty, b) => b,
            (a, SubmitAnyBuilder::Empty) => a,
            (SubmitAnyBuilder::SemaphoresWait(mut a), SubmitAnyBuilder::SemaphoresWait(b)) => {
                a.merge(b);
                SubmitAnyBuilder::SemaphoresWait(a)
            },
            (SubmitAnyBuilder::SemaphoresWait(a), SubmitAnyBuilder::CommandBuffer(b)) => {
                try!(b.submit(&self.second.queue().clone().unwrap()));
                SubmitAnyBuilder::SemaphoresWait(a)
            },
            (SubmitAnyBuilder::CommandBuffer(a), SubmitAnyBuilder::SemaphoresWait(b)) => {
                try!(a.submit(&self.first.queue().clone().unwrap()));
                SubmitAnyBuilder::SemaphoresWait(b)
            },
            (SubmitAnyBuilder::SemaphoresWait(a), SubmitAnyBuilder::QueuePresent(b)) => {
                try!(b.submit(&self.second.queue().clone().unwrap()));
                SubmitAnyBuilder::SemaphoresWait(a)
            },
            (SubmitAnyBuilder::QueuePresent(a), SubmitAnyBuilder::SemaphoresWait(b)) => {
                try!(a.submit(&self.first.queue().clone().unwrap()));
                SubmitAnyBuilder::SemaphoresWait(b)
            },
            (SubmitAnyBuilder::CommandBuffer(a), SubmitAnyBuilder::CommandBuffer(b)) => {
                // TODO: we may want to add debug asserts here
                let new = a.merge(b);
                SubmitAnyBuilder::CommandBuffer(new)
            },
            (SubmitAnyBuilder::QueuePresent(a), SubmitAnyBuilder::QueuePresent(b)) => {
                try!(a.submit(&self.first.queue().clone().unwrap()));
                try!(b.submit(&self.second.queue().clone().unwrap()));
                SubmitAnyBuilder::Empty
            },
            (SubmitAnyBuilder::CommandBuffer(a), SubmitAnyBuilder::QueuePresent(b)) => {
                unimplemented!()
            },
            (SubmitAnyBuilder::QueuePresent(a), SubmitAnyBuilder::CommandBuffer(b)) => {
                unimplemented!()
            },
        })
    }

    #[inline]
    unsafe fn signal_finished(&self) {
        self.first.signal_finished();
        self.second.signal_finished();
    }

    #[inline]
    fn queue_change_allowed(&self) -> bool {
        self.first.queue_change_allowed() && self.second.queue_change_allowed()
    }

    #[inline]
    fn queue(&self) -> Option<&Arc<Queue>> {
        match (self.first.queue(), self.second.queue()) {
            (Some(q1), Some(q2)) => if q1.is_same(&q2) {
                Some(q1)
            } else if self.first.queue_change_allowed() {
                Some(q2)
            } else if self.second.queue_change_allowed() {
                Some(q1)
            } else {
                None
            },
            (Some(q), None) => Some(q),
            (None, Some(q)) => Some(q),
            (None, None) => None,
        }
    }

    #[inline]
    fn check_buffer_access(&self, buffer: &BufferAccess, exclusive: bool, queue: &Queue)
                           -> Result<Option<(PipelineStages, AccessFlagBits)>, ()>
    {
        let first = self.first.check_buffer_access(buffer, exclusive, queue);
        let second = self.second.check_buffer_access(buffer, exclusive, queue);
        debug_assert!(!exclusive || !(first.is_ok() && second.is_ok()), "Two futures gave \
                                                                         exclusive access to the \
                                                                         same resource");
        match (first, second) {
            (Ok(v), Err(_)) | (Err(_), Ok(v)) => Ok(v),
            (Err(()), Err(())) => Err(()),
            (Ok(None), Ok(None)) => Ok(None),
            (Ok(Some(a)), Ok(None)) | (Ok(None), Ok(Some(a))) => Ok(Some(a)),
            (Ok(Some((a1, a2))), Ok(Some((b1, b2)))) => {
                Ok(Some((a1 | b1, a2 | b2)))
            },
        }
    }

    #[inline]
    fn check_image_access(&self, image: &ImageAccess, exclusive: bool, queue: &Queue)
                          -> Result<Option<(PipelineStages, AccessFlagBits)>, ()>
    {
        let first = self.first.check_image_access(image, exclusive, queue);
        let second = self.second.check_image_access(image, exclusive, queue);
        debug_assert!(!exclusive || !(first.is_ok() && second.is_ok()), "Two futures gave \
                                                                         exclusive access to the \
                                                                         same resource");
        match (first, second) {
            (Ok(v), Err(_)) | (Err(_), Ok(v)) => Ok(v),
            (Err(()), Err(())) => Err(()),
            (Ok(None), Ok(None)) => Ok(None),
            (Ok(Some(a)), Ok(None)) | (Ok(None), Ok(Some(a))) => Ok(Some(a)),
            (Ok(Some((a1, a2))), Ok(Some((b1, b2)))) => {
                Ok(Some((a1 | b1, a2 | b2)))
            },
        }
    }
}
