use crate::{
    runtime::{
        Schedule,
        local::{CURRENT_SCHEDULER, queue::LocalTaskQueue},
    },
    task::Task,
};

#[derive(Default)]
pub(crate) struct Scheduler {
    pub(crate) tasks: LocalTaskQueue<ScheduleHandle>,
}

#[derive(Debug)]
pub(crate) struct ScheduleHandle;

impl Schedule for ScheduleHandle {
    fn schedule(&self, task: Task<Self>) {
        CURRENT_SCHEDULER.with(|sch| {
            sch.tasks.push_back(task);
        });
    }

    fn yield_now(&self, task: Task<Self>) {
        CURRENT_SCHEDULER.with(|sch| {
            sch.tasks.push_back(task);
        });
    }
}
