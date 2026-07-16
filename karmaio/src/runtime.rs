use crate::task::Task;

pub mod blocking;
pub mod local;

pub use crate::task::{JoinError, JoinHandle};
pub use blocking::{BlockingPool, BlockingPoolHandle};
pub use local::{Runtime, spawn_blocking, spawn_local};

pub(crate) trait Schedule: Sized + 'static {
    /// Schedule the task
    fn schedule(&self, task: Task<Self>);

    /// Schedule the task to run in the near future, yielding the thread to
    /// other tasks.
    fn yield_now(&self, task: Task<Self>) {
        self.schedule(task);
    }

    /// Polling the task resulted in a panic. Should the runtime shutdown?
    fn unhandled_panic(&self) {
        // By default, do nothing
    }
}
