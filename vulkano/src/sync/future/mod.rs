// Copyright (c) 2016 The vulkano developers
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
use command_buffer::CommandBuffer;
use command_buffer::CommandBufferExecFuture;
use command_buffer::submit::SubmitAnyBuilder;
use device::DeviceOwned;
use device::Queue;
use image::ImageAccess;
use swapchain::Swapchain;
use swapchain::PresentFuture;
use sync::AccessFlagBits;
use sync::PipelineStages;

pub use self::dummy::DummyFuture;
pub use self::fence_signal::FenceSignalFuture;
pub use self::join::JoinFuture;
pub use self::semaphore_signal::SemaphoreSignalFuture;

mod dummy;
mod fence_signal;
mod join;
mod semaphore_signal;

/// Represents an event that will happen on the GPU in the future.
///
/// See the documentation of the `sync` module for explanations about futures.
// TODO: consider switching all methods to take `&mut self` for optimization purposes
pub unsafe trait GpuFuture: DeviceOwned {
    /// If possible, checks whether the submission has finished. If so, gives up ownership of the
    /// resources used by these submissions.
    ///
    /// It is highly recommended to call `cleanup_finished` from time to time. Doing so will
    /// prevent memory usage from increasing over time, and will also destroy the locks on 
    /// resources used by the GPU.
    fn cleanup_finished(&mut self);

    /// Builds a submission that, if submitted, makes sure that the event represented by this
    /// `GpuFuture` will happen, and possibly contains extra elements (eg. a semaphore wait or an
    /// event wait) that makes the dependency with subsequent operations work.
    ///
    /// It is the responsibility of the caller to ensure that the submission is going to be
    /// submitted only once. However keep in mind that this function can perfectly be called
    /// multiple times (as long as the returned object is only submitted once).
    ///
    /// It is however the responsibility of the implementation to not return the same submission
    /// from multiple different future objects. For example if you implement `GpuFuture` on
    /// `Arc<Foo>` then `build_submission()` must always return `SubmitAnyBuilder::Empty`,
    /// otherwise it would be possible for the user to clone the `Arc` and make the same
    /// submission be submitted multiple times.
    ///
    /// Once the caller has submitted the submission and has determined that the GPU has finished
    /// executing it, it should call `signal_finished`. Failure to do so will incur a large runtime
    /// overhead, as the future will have to block to make sure that it is finished.
    // TODO: better error type
    unsafe fn build_submission(&self) -> Result<SubmitAnyBuilder, Box<Error>>;

    /// Flushes the future and submits to the GPU the actions that will permit this future to
    /// occur.
    ///
    /// The implementation must remember that it was flushed. If the function is called multiple
    /// times, only the first time must result in a flush.
    // TODO: better error type
    fn flush(&self) -> Result<(), Box<Error>>;

    /// Sets the future to its "complete" state, meaning that it can safely be destroyed.
    ///
    /// This must only be done if you called `build_submission()`, submitted the returned
    /// submission, and determined that it was finished.
    unsafe fn signal_finished(&self);

    /// Returns the queue that triggers the event. Returns `None` if unknown or irrelevant.
    ///
    /// If this function returns `None` and `queue_change_allowed` returns `false`, then a panic
    /// is likely to occur if you use this future. This is only a problem if you implement
    /// the `GpuFuture` trait yourself for a type outside of vulkano.
    fn queue(&self) -> Option<&Arc<Queue>>;

    /// Returns `true` if elements submitted after this future can be submitted to a different
    /// queue than the other returned by `queue()`.
    fn queue_change_allowed(&self) -> bool;

    /// Checks whether submitting something after this future grants access (exclusive or shared,
    /// depending on the parameter) to the given buffer on the given queue.
    ///
    /// If the access is granted, returns the pipeline stage and access flags of the latest usage
    /// of this resource, or `None` if irrelevant.
    ///
    /// > **Note**: Returning `Ok` means "access granted", while returning `Err` means
    /// > "don't know". Therefore returning `Err` is never unsafe.
    fn check_buffer_access(&self, buffer: &BufferAccess, exclusive: bool, queue: &Queue)
                           -> Result<Option<(PipelineStages, AccessFlagBits)>, ()>;

    /// Checks whether submitting something after this future grants access (exclusive or shared,
    /// depending on the parameter) to the given image on the given queue.
    ///
    /// If the access is granted, returns the pipeline stage and access flags of the latest usage
    /// of this resource, or `None` if irrelevant.
    ///
    /// > **Note**: Returning `Ok` means "access granted", while returning `Err` means
    /// > "don't know". Therefore returning `Err` is never unsafe.
    ///
    /// > **Note**: Keep in mind that changing the layout of an image also requires exclusive
    /// > access.
    fn check_image_access(&self, image: &ImageAccess, exclusive: bool, queue: &Queue)
                         -> Result<Option<(PipelineStages, AccessFlagBits)>, ()>;

    /// Joins this future with another one, representing the moment when both events have happened.
    // TODO: handle errors
    fn join<F>(self, other: F) -> JoinFuture<Self, F>
        where Self: Sized, F: GpuFuture
    {
        join::join(self, other)
    }

    /// Executes a command buffer after this future.
    ///
    /// > **Note**: This is just a shortcut function. The actual implementation is in the
    /// > `CommandBuffer` trait.
    #[inline]
    fn then_execute<Cb>(self, queue: Arc<Queue>, command_buffer: Cb)
                        -> CommandBufferExecFuture<Self, Cb>
        where Self: Sized, Cb: CommandBuffer + 'static
    {
        command_buffer.execute_after(self, queue)
    }

    /// Executes a command buffer after this future, on the same queue as the future.
    ///
    /// > **Note**: This is just a shortcut function. The actual implementation is in the
    /// > `CommandBuffer` trait.
    #[inline]
    fn then_execute_same_queue<Cb>(self, command_buffer: Cb) -> CommandBufferExecFuture<Self, Cb>
        where Self: Sized, Cb: CommandBuffer + 'static
    {
        let queue = self.queue().unwrap().clone();
        command_buffer.execute_after(self, queue)
    }

    /// Signals a semaphore after this future. Returns another future that represents the signal.
    ///
    /// Call this function when you want to execute some operations on a queue and want to see the
    /// result on another queue.
    #[inline]
    fn then_signal_semaphore(self) -> SemaphoreSignalFuture<Self> where Self: Sized {
        semaphore_signal::then_signal_semaphore(self)
    }

    /// Signals a semaphore after this future and flushes it. Returns another future that
    /// represents the moment when the semaphore is signalled.
    ///
    /// This is a just a shortcut for `then_signal_semaphore()` followed with `flush()`.
    ///
    /// When you want to execute some operations A on a queue and some operations B on another
    /// queue that need to see the results of A, it can be a good idea to submit A as soon as
    /// possible while you're preparing B.
    ///
    /// If you ran A and B on the same queue, you would have to decide between submitting A then
    /// B, or A and B simultaneously. Both approaches have their trade-offs. But if A and B are
    /// on two different queues, then you would need two submits anyway and it is always
    /// advantageous to submit A as soon as possible.
    #[inline]
    fn then_signal_semaphore_and_flush(self) -> Result<SemaphoreSignalFuture<Self>, Box<Error>>
        where Self: Sized
    {
        let f = self.then_signal_semaphore();
        f.flush()?;
        Ok(f)
    }

    /// Signals a fence after this future. Returns another future that represents the signal.
    ///
    /// > **Note**: More often than not you want to immediately flush the future after calling this
    /// > function. If so, consider using `then_signal_fence_and_flush`.
    #[inline]
    fn then_signal_fence(self) -> FenceSignalFuture<Self> where Self: Sized {
        fence_signal::then_signal_fence(self)
    }

    /// Signals a fence after this future. Returns another future that represents the signal.
    ///
    /// This is a just a shortcut for `then_signal_fence()` followed with `flush()`.
    #[inline]
    fn then_signal_fence_and_flush(self) -> Result<FenceSignalFuture<Self>, Box<Error>>
        where Self: Sized
    {
        let f = self.then_signal_fence();
        f.flush()?;
        Ok(f)
    }

    /// Presents a swapchain image after this future.
    ///
    /// You should only ever do this indirectly after a `SwapchainAcquireFuture` of the same image,
    /// otherwise an error will occur when flushing.
    ///
    /// > **Note**: This is just a shortcut for the `Swapchain::present()` function.
    #[inline]
    fn then_swapchain_present(self, queue: Arc<Queue>, swapchain: Arc<Swapchain>,
                              image_index: usize) -> PresentFuture<Self>
        where Self: Sized
    {
        Swapchain::present(swapchain, self, queue, image_index)
    }
}

unsafe impl<F: ?Sized> GpuFuture for Box<F> where F: GpuFuture {
    #[inline]
    fn cleanup_finished(&mut self) {
        (**self).cleanup_finished()
    }

    #[inline]
    unsafe fn build_submission(&self) -> Result<SubmitAnyBuilder, Box<Error>> {
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
