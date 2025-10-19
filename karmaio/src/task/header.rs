use super::{state::State, vtable::VTable};

pub(crate) struct Header {
    /// The current state of the task
    pub(super) state: State,
    /// Pointer to the task's vtable for type erased dispatcxh
    pub(super) vtable: &'static VTable,
}

impl Header {}
