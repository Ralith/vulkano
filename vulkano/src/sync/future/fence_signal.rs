// Copyright (c) 2017 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

use std::error::Error;
use std::mem;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::time::Duration;

use buffer::BufferAccess;
use command_buffer::submit::SubmitAnyBuilder;
use command_buffer::submit::SubmitCommandBufferBuilder;
use device::Device;
use device::DeviceOwned;
use device::Queue;
use image::ImageAccess;
use sync::AccessFlagBits;
use sync::Fence;
use sync::GpuFuture;
use sync::PipelineStages;

/// Builds a new fence signal future.
#[inline]
pub fn then_signal_fence<F>(future: F) -> FenceSignalFuture<F> where F: GpuFuture {
    let device = future.device().clone();

    assert!(future.queue().is_some());        // TODO: document

    let fence = Fence::new(device.clone()).unwrap();
    FenceSignalFuture {
        device: device,
        state: Mutex::new(FenceSignalFutureState::Pending(future, fence)),
    }
}

/// Represents a fence being signaled after a previous event.
#[must_use = "Dropping this object will immediately block the thread until the GPU has finished processing the submission"]
pub struct FenceSignalFuture<F> where F: GpuFuture {
    // Current state. See the docs of `FenceSignalFutureState`.
    state: Mutex<FenceSignalFutureState<F>>,
    // The device of the future.
    device: Arc<Device>,
}

// This future can be in three different states: pending (ie. newly-created), submitted (ie. the
// command that submits the fence has been submitted), or cleaned (ie. the previous future has
// been dropped).
enum FenceSignalFutureState<F> {
    // Newly-created. Not submitted yet.
    Pending(F, Fence),

    // Partially submitted to the queue. Only happens in situations where submitting requires two
    // steps, and when the first step succeeded while the second step failed.
    //
    // Note that if there's ever a submit operation that needs three steps we will need to rework
    // this code, as it was designed for two-step operations only.
    PartiallyFlushed(F, Fence),

    // Submitted to the queue.
    Flushed(F, Fence),

    // The submission is finished. The previous future and the fence have been cleaned.
    Cleaned,

    // A function panicked while the state was being modified. Should never happen.
    Poisonned,
}

impl<F> FenceSignalFuture<F> where F: GpuFuture {
    // Implementation of `cleanup_finished`, but takes a `&self` instead of a `&mut self`.
    // This is an external function so that we can also call it from an `Arc<FenceSignalFuture>`.
    #[inline]
    fn cleanup_finished_impl(&self) {
        let mut state = self.state.lock().unwrap();

        match *state {
            FenceSignalFutureState::Flushed(_, ref fence) => {
                match fence.wait(Duration::from_secs(0)) {
                    Ok(()) => (),
                    Err(_) => return,
                }
            },
            _ => return,
        };

        // This code can only be reached if we're already flushed and waiting on the fence
        // succeeded.
        *state = FenceSignalFutureState::Cleaned;
    }

    // Implementation of `flush`. You must lock the state and pass the mutex guard here.
    fn flush_impl(&self, state: &mut MutexGuard<FenceSignalFutureState<F>>)
                  -> Result<(), Box<Error>>
    {
        unsafe {
            // In this function we temporarily replace the current state with `Poisonned` at the
            // beginning, and we take care to always put back a value into `state` before
            // returning (even in case of error).
            let old_state = mem::replace(&mut **state, FenceSignalFutureState::Poisonned);

            let (previous, fence, partially_flushed) = match old_state {
                FenceSignalFutureState::Pending(prev, fence) => {
                    (prev, fence, false)
                },
                FenceSignalFutureState::PartiallyFlushed(prev, fence) => {
                    (prev, fence, true)
                },
                other => {
                    // We were already flushed in the past, or we're already poisonned. Don't do
                    // anything.
                    **state = other;
                    return Ok(());
                },
            };

            // TODO: meh for unwrap
            let queue = previous.queue().unwrap().clone();

            // There are three possible outcomes for the flush operation: success, partial success
            // in which case `result` will contain `Err(OutcomeErr::Partial)`, or total failure
            // in which case `result` will contain `Err(OutcomeErr::Full)`.
            enum OutcomeErr<E> { Partial(E), Full(E) }
            let result = match try!(previous.build_submission()) {
                SubmitAnyBuilder::Empty => {
                    debug_assert!(!partially_flushed);
                    let mut b = SubmitCommandBufferBuilder::new();
                    b.set_fence_signal(&fence);
                    b.submit(&queue).map_err(|err| OutcomeErr::Full(err.into()))
                },
                SubmitAnyBuilder::SemaphoresWait(sem) => {
                    debug_assert!(!partially_flushed);
                    let b: SubmitCommandBufferBuilder = sem.into();
                    debug_assert!(!b.has_fence());
                    b.submit(&queue).map_err(|err| OutcomeErr::Full(err.into()))
                },
                SubmitAnyBuilder::CommandBuffer(mut cb_builder) => {
                    debug_assert!(!partially_flushed);
                    // The assert below could technically be a debug assertion as it is part of the
                    // safety contract of the trait. However it is easy to get this wrong if you
                    // write a custom implementation, and if so the consequences would be
                    // disastrous and hard to debug. Therefore we prefer to just use a regular
                    // assertion.
                    assert!(!cb_builder.has_fence());
                    cb_builder.set_fence_signal(&fence);
                    cb_builder.submit(&queue).map_err(|err| OutcomeErr::Full(err.into()))
                },
                SubmitAnyBuilder::QueuePresent(present) => {
                    let intermediary_result = if partially_flushed {
                        Ok(())
                    } else {
                        present.submit(&queue)
                    };
                    match intermediary_result {
                        Ok(()) => {
                            let mut b = SubmitCommandBufferBuilder::new();
                            b.set_fence_signal(&fence);
                            b.submit(&queue).map_err(|err| OutcomeErr::Partial(err.into()))
                        },
                        Err(err) => {
                            Err(OutcomeErr::Full(err.into()))
                        }
                    }
                },
            };

            // Restore the state before returning.
            match result {
                Ok(()) => {
                    **state = FenceSignalFutureState::Flushed(previous, fence);
                    Ok(())
                },
                Err(OutcomeErr::Partial(err)) => {
                    **state = FenceSignalFutureState::PartiallyFlushed(previous, fence);
                    Err(err)
                },
                Err(OutcomeErr::Full(err)) => {
                    **state = FenceSignalFutureState::Pending(previous, fence);
                    Err(err)
                },
            }
        }
    }
}

impl<F> FenceSignalFutureState<F> {
    #[inline]
    fn get_prev(&self) -> Option<&F> {
        match *self {
            FenceSignalFutureState::Pending(ref prev, _) => Some(prev),
            FenceSignalFutureState::PartiallyFlushed(ref prev, _) => Some(prev),
            FenceSignalFutureState::Flushed(ref prev, _) => Some(prev),
            FenceSignalFutureState::Cleaned => None,
            FenceSignalFutureState::Poisonned => None,
        }
    }
}

unsafe impl<F> GpuFuture for FenceSignalFuture<F> where F: GpuFuture {
    #[inline]
    fn cleanup_finished(&mut self) {
        self.cleanup_finished_impl()
    }

    #[inline]
    unsafe fn build_submission(&self) -> Result<SubmitAnyBuilder, Box<Error>> {
        let mut state = self.state.lock().unwrap();
        try!(self.flush_impl(&mut state));

        match *state {
            FenceSignalFutureState::Flushed(_, ref fence) => {
                try!(fence.wait(Duration::from_secs(600)));     // TODO: arbitrary timeout?
            },
            FenceSignalFutureState::Cleaned | FenceSignalFutureState::Poisonned => (),
            FenceSignalFutureState::Pending(_, _)  => unreachable!(),
            FenceSignalFutureState::PartiallyFlushed(_, _) => unreachable!(),
        }

        Ok(SubmitAnyBuilder::Empty)
    }

    #[inline]
    fn flush(&self) -> Result<(), Box<Error>> {
        let mut state = self.state.lock().unwrap();
        self.flush_impl(&mut state)
    }

    #[inline]
    unsafe fn signal_finished(&self) {
        let state = self.state.lock().unwrap();
        match *state {
            FenceSignalFutureState::Flushed(ref prev, _) => {
                prev.signal_finished();
            },
            FenceSignalFutureState::Cleaned | FenceSignalFutureState::Poisonned => (),
            _ => unreachable!(),
        }
    }

    #[inline]
    fn queue_change_allowed(&self) -> bool {
        true
    }

    #[inline]
    fn queue(&self) -> Option<&Arc<Queue>> {
        // FIXME: reimplement correctly ; either find a solution or change the API to take &mut self
        None
    }

    #[inline]
    fn check_buffer_access(&self, buffer: &BufferAccess, exclusive: bool, queue: &Queue)
                           -> Result<Option<(PipelineStages, AccessFlagBits)>, ()> {
        let state = self.state.lock().unwrap();
        if let Some(previous) = state.get_prev() {
            previous.check_buffer_access(buffer, exclusive, queue)
        } else {
            Err(())
        }
    }

    #[inline]
    fn check_image_access(&self, image: &ImageAccess, exclusive: bool, queue: &Queue)
                          -> Result<Option<(PipelineStages, AccessFlagBits)>, ()> {
        let state = self.state.lock().unwrap();
        if let Some(previous) = state.get_prev() {
            previous.check_image_access(image, exclusive, queue)
        } else {
            Err(())
        }
    }
}

unsafe impl<F> DeviceOwned for FenceSignalFuture<F> where F: GpuFuture {
    #[inline]
    fn device(&self) -> &Arc<Device> {
        &self.device
    }
}

impl<F> Drop for FenceSignalFuture<F> where F: GpuFuture {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap();

        // We ignore any possible error while submitting for now. Problems are handled below.
        let _ = self.flush_impl(&mut state);

        match mem::replace(&mut *state, FenceSignalFutureState::Cleaned) {
            FenceSignalFutureState::Flushed(previous, fence) => {
                // This is a normal situation. Submitting worked.
                // TODO: arbitrary timeout?
                // TODO: handle errors?
                fence.wait(Duration::from_secs(600)).unwrap();
                unsafe { previous.signal_finished(); }
            },
            FenceSignalFutureState::Cleaned => {
                // Also a normal situation. The user called `cleanup_finished()` before dropping.
            },
            FenceSignalFutureState::Poisonned => {
                // The previous future was already dropped and blocked the current queue.
            },
            FenceSignalFutureState::Pending(_, _) |
            FenceSignalFutureState::PartiallyFlushed(_, _) => {
                // Flushing produced an error. There's nothing more we can do except drop the 
                // previous future and let it block the current queue.
            },
        }
    }
}

unsafe impl<F> GpuFuture for Arc<FenceSignalFuture<F>> where F: GpuFuture {
    #[inline]
    fn cleanup_finished(&mut self) {
        self.cleanup_finished_impl()
    }

    #[inline]
    unsafe fn build_submission(&self) -> Result<SubmitAnyBuilder, Box<Error>> {
        // Note that this is sound because we always return `SubmitAnyBuilder::Empty`. See the
        // documentation of `build_submission`.
        (**self).build_submission()
    }

    #[inline]
    fn flush(&self) -> Result<(), Box<Error>> {
        (**self).flush()
    }

    #[inline]
    unsafe fn signal_finished(&self) {
        (**self).signal_finished()
    }

    #[inline]
    fn queue_change_allowed(&self) -> bool {
        (**self).queue_change_allowed()
    }

    #[inline]
    fn queue(&self) -> Option<&Arc<Queue>> {
        (**self).queue()
    }

    #[inline]
    fn check_buffer_access(&self, buffer: &BufferAccess, exclusive: bool, queue: &Queue)
                          -> Result<Option<(PipelineStages, AccessFlagBits)>, ()>
    {
        (**self).check_buffer_access(buffer, exclusive, queue)
    }

    #[inline]
    fn check_image_access(&self, image: &ImageAccess, exclusive: bool, queue: &Queue)
                          -> Result<Option<(PipelineStages, AccessFlagBits)>, ()>
    {
        (**self).check_image_access(image, exclusive, queue)
    }
}
