use std::{cell::UnsafeCell, collections::VecDeque};

use crate::{runtime::Schedule, task::Task};

pub(crate) struct LocalTaskQueue<S: Schedule> {
    queue: UnsafeCell<VecDeque<Task<S>>>,
}

impl<S: Schedule> Default for LocalTaskQueue<S> {
    fn default() -> Self {
        Self::new()
    }
}

impl<S: Schedule> Drop for LocalTaskQueue<S> {
    fn drop(&mut self) {
        unsafe {
            let queue = &mut *self.queue.get();
            while let Some(_task) = queue.pop_front() {}
        }
    }
}

impl<S: Schedule> LocalTaskQueue<S> {
    pub(crate) fn new() -> Self {
        const DEFAULT_TASK_QUEUE_SIZE: usize = 1024;
        Self::new_with_capacity(DEFAULT_TASK_QUEUE_SIZE)
    }

    pub(crate) fn new_with_capacity(capacity: usize) -> Self {
        Self {
            queue: UnsafeCell::new(VecDeque::with_capacity(capacity)),
        }
    }

    pub(crate) fn push_back(&self, item: Task<S>) {
        // SAFETY:
        // Exclusive mutable access because:
        // - The mutable reference is created and used immediately within this scope.
        // - `LocalTaskQueue` is `!Sync`, so no other threads can access it concurrently.
        let queue = unsafe { &mut *self.queue.get() };
        queue.push_back(item);
    }

    pub(crate) fn pop_front(&self) -> Option<Task<S>> {
        // SAFETY:
        // Exclusive mutable access because:
        // - The mutable reference is created and used immediately within this scope.
        // - `LocalTaskQueue` is `!Sync`, so no other threads can access it concurrently.
        let queue = unsafe { &mut *self.queue.get() };
        queue.pop_front()
    }

    pub(crate) fn is_empty(&self) -> bool {
        // SAFETY:
        // Exclusive mutable access because:
        // - The mutable reference is created and used immediately within this scope.
        // - `LocalTaskQueue` is `!Sync`, so no other threads can access it concurrently.
        let queue = unsafe { &mut *self.queue.get() };
        queue.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        // SAFETY:
        // Exclusive mutable access because:
        // - The mutable reference is created and used immediately within this scope.
        // - `LocalTaskQueue` is `!Sync`, so no other threads can access it concurrently.
        let queue = unsafe { &mut *self.queue.get() };
        queue.len()
    }
}
