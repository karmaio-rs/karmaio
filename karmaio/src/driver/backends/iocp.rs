use std::{
    io::{Error, Result},
    os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle},
    task::{Context, Poll},
    time::Duration,
};

use slab::Slab;
use windows_sys::Win32::{
    Foundation::{ERROR_NOT_FOUND, HANDLE, INVALID_HANDLE_VALUE, RtlNtStatusToDosError, WAIT_TIMEOUT},
    System::IO::{CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, OVERLAPPED, OVERLAPPED_ENTRY},
};

use crate::driver::{
    Handle,
    backends::DriverBackend,
    ops::{Completion, Op, Operable, State, Submittable},
};

// Stable allocation passed to Windows for one overlapped operation.
//
// `raw` must stay first: completion entries give us back `*mut OVERLAPPED`,
// which we cast back to this header to recover the slab index.
#[repr(C)]
struct OverlappedHeader {
    raw: OVERLAPPED,
    index: usize,
}

// Owned OVERLAPPED state for a submitted IOCP operation.
//
// The memory behind this value is boxed so moving `Interest` between an op and
// the driver does not invalidate the pointer already handed to Windows.
pub(crate) struct Interest {
    overlapped: Box<OverlappedHeader>,
    handle: HANDLE,
}

impl Interest {
    // Creates zeroed overlapped state for an operation on `handle`.
    //
    // The driver fills the slab index after the operation has been accepted.
    pub(crate) fn new(handle: RawHandle) -> Self {
        Self {
            overlapped: Box::new(OverlappedHeader {
                raw: OVERLAPPED::default(),
                index: usize::MAX,
            }),
            handle: handle as HANDLE,
        }
    }

    // Returns the pointer to pass to Windows overlapped APIs.
    #[inline]
    pub(crate) fn as_mut_ptr(&mut self) -> *mut OVERLAPPED {
        &mut self.overlapped.raw
    }

    #[inline]
    fn as_ptr(&self) -> *const OVERLAPPED {
        &self.overlapped.raw
    }

    #[inline]
    fn handle(&self) -> HANDLE {
        self.handle
    }

    #[inline]
    fn set_index(&mut self, index: usize) {
        self.overlapped.index = index;
    }

    #[inline]
    unsafe fn index_from_raw(overlapped: *mut OVERLAPPED) -> usize {
        // SAFETY: `Interest::as_mut_ptr` returns a pointer to the first field of
        // `OverlappedHeader`, and that boxed header is kept alive in the slab
        // until the completion packet has been processed.
        unsafe { (*(overlapped as *mut OverlappedHeader)).index }
    }
}

pub(crate) enum Submission {
    Ready(Completion),
    Pending(Interest),
}

// A tracked operation slot in the driver slab.
//
// The slab index is the logical op id. Pending IOCP operations also keep an
// `Interest`, whose boxed OVERLAPPED stores this same index so completions can
// find the slot without looking at the completion key.
struct Slot {
    state: State,
    interest: Option<Interest>,
    data: Option<PendingData>,
}

// Type-erased owner for op data after the future is dropped.
//
// Windows still owns the OVERLAPPED until it posts the completion packet. The
// user no longer wants the result, but we must keep the op data alive because it
// may contain buffers or sockets referenced by the kernel operation.
struct PendingData {
    data: *mut (),
    drop: unsafe fn(*mut ()),
}

impl PendingData {
    fn new<T>(data: T) -> Self {
        unsafe fn drop_data<T>(data: *mut ()) {
            // SAFETY: `data` was produced by `Box::into_raw` for the same `T`
            // in `PendingData::new`.
            unsafe {
                drop(Box::from_raw(data as *mut T));
            }
        }

        Self {
            data: Box::into_raw(Box::new(data)) as *mut (),
            drop: drop_data::<T>,
        }
    }
}

impl Drop for PendingData {
    fn drop(&mut self) {
        // SAFETY: The drop function is monomorphized for the original type.
        unsafe {
            (self.drop)(self.data);
        }
    }
}

/// The completion port that receives kernel completion packets.
struct CompletionPort {
    port: OwnedHandle,
}

impl CompletionPort {
    fn new() -> Result<Self> {
        let handle = unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 0) };
        if handle.is_null() {
            return Err(Error::last_os_error());
        }

        Ok(Self {
            // SAFETY: `CreateIoCompletionPort` returned a valid owned handle.
            port: unsafe { OwnedHandle::from_raw_handle(handle as RawHandle) },
        })
    }

    fn add_handle(&self, handle: RawHandle, completion_key: usize) -> Result<()> {
        let result =
            unsafe { CreateIoCompletionPort(handle as HANDLE, self.port.as_raw_handle() as HANDLE, completion_key, 0) };

        if result.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn get_many(&self, entries: &mut Vec<OVERLAPPED_ENTRY>, timeout: Option<Duration>) -> Result<usize> {
        let mut count = 0;
        let result = unsafe {
            GetQueuedCompletionStatusEx(
                self.port.as_raw_handle() as HANDLE,
                entries.as_mut_ptr(),
                // Ensures that the writable capacity is always within bounds
                entries.capacity().min(u32::MAX as usize) as u32,
                &mut count,
                duration_millis(timeout),
                0,
            )
        };

        if result == 0 {
            let err = io::Error::last_os_error();

            if err.raw_os_error() == Some(WAIT_TIMEOUT as i32) {
                return Ok(0);
            }

            Err(err)
        } else {
            // Since the buffer is written to by the kernel, Vec does not know its new length
            // So we set length to the entries kernel reported to us
            unsafe {
                entries.set_len(count as usize);
            };
            Ok(count as usize)
        }
    }
}

pub(crate) struct IocpBackend {
    // Completion port shared by all handles registered with this backend.
    port: CompletionPort,
    // Per-op state keyed by slab index.
    ops: Slab<Slot>,
    // Reused batch buffer for completion packets.
    // Since this is a buffer that is filled by the kernel over ffi, we need to be follow some rules -
    // 1. We will not resize this vec after initialization
    // 2. We will have to manually manage the length of the buffer during -
    //    a. Reading completions from kernel
    //    b. Processing completions
    entries: Vec<OVERLAPPED_ENTRY>,
}

impl IocpBackend {
    pub(crate) fn new() -> Result<Self> {
        // TODO: Make this default configurable
        Ok(Self {
            port: CompletionPort::new()?,
            ops: Slab::with_capacity(1024),
            entries: vec![unsafe { std::mem::zeroed() }; 1024],
        })
    }

    // Associates a file or socket handle with this backend's completion port.
    //
    // Windows requires this before issuing overlapped I/O on the handle. The
    // completion key is intentionally separate from op identity; op identity is
    // recovered from the stable `OVERLAPPED` pointer.
    pub(crate) fn add_handle(&self, handle: RawHandle, completion_key: usize) -> Result<()> {
        self.port.add_handle(handle, completion_key)
    }
}

impl DriverBackend for IocpBackend {
    fn submit_op<T: Submittable>(&mut self, mut data: T, handle: Handle) -> Result<Op<T>> {
        let index = self.ops.insert(Slot {
            state: State::Submitted,
            interest: None,
            data: None,
        });

        match data.submit() {
            Submission::Ready(completion) => {
                // The operation finished before it needed IOCP. Store the
                // completion so the first poll resolves the future.
                self.ops[index].state = State::Completed(completion);
            }
            Submission::Pending(mut interest) => {
                // The kernel owns this OVERLAPPED until completion. Stamp the
                // slab index into the stable allocation before storing it.
                interest.set_index(index);
                self.ops[index].interest = Some(interest);
            }
        }

        Ok(Op::<T>::new(index, data, handle))
    }

    fn remove_op<T>(&mut self, op: &mut Op<T>) {
        let index = op.index();
        let Some(slot) = self.ops.get_mut(index) else {
            // Op already dropped or removed.
            return;
        };

        match &slot.state {
            State::Submitted | State::Waiting(..) => {
                // The future was dropped while the kernel may still complete
                // the operation. Ask Windows to cancel it, but keep the slot
                // and data alive until the completion packet arrives.
                if let Some(interest) = slot.interest.as_ref() {
                    let result = unsafe { CancelIoEx(interest.handle(), interest.as_ptr()) };

                    if result == 0 {
                        let err = io::Error::last_os_error();
                        debug_assert_eq!(
                            err.raw_os_error(),
                            Some(ERROR_NOT_FOUND as i32),
                            "CancelIoEx failed: {err}"
                        );
                    }
                }

                slot.data = op.take_data().map(PendingData::new);
                slot.state = State::Ignored(Box::new(()));
            }
            State::Completed(..) => {
                // The kernel already completed and no one will poll the
                // future, so the slot can be released immediately.
                self.ops.remove(index);
                let _ = op.take_data();
            }
            State::Ignored(..) => unreachable!("invalid operation state"),
            State::Ready => unreachable!("invalid operation state"),
        }
    }

    fn poll_op<T: Operable>(&mut self, op: &mut Op<T>, cx: &mut Context<'_>) -> Poll<T::Result> {
        let state = &mut self.ops.get_mut(op.index()).expect("invalid internal state").state;

        match state {
            State::Submitted => {
                // First poll after submission: remember this task's waker and
                // let the IOCP completion path wake it.
                *state = State::Waiting(cx.waker().clone());
                Poll::Pending
            }
            State::Waiting(waker) => {
                // The task moved or was re-polled with a different waker.
                // Replace it so completion wakes the current task.
                if !waker.will_wake(cx.waker()) {
                    *state = State::Waiting(cx.waker().clone());
                }
                Poll::Pending
            }
            State::Completed(_) => match self.ops.remove(op.index()).state {
                // Completion was already dispatched. Consume op data exactly
                // once and let the op-specific code decode the CQ result.
                State::Completed(completion) => Poll::Ready(op.take_data().unwrap().complete(completion)),
                _ => unreachable!("invalid operation"),
            },
            // The op has been ignored/cancelled by the caller. It should not be polled again
            State::Ignored(..) => {
                unreachable!("invalid operation")
            }
            // This state is only set in poll based reactors, not completion reactors
            State::Ready => {
                unreachable!("invalid operation")
            }
        }
    }

    fn submit(&mut self) -> Result<()> {
        // IOCP operations are submitted by the individual overlapped syscalls.
        Ok(())
    }

    fn wait(&mut self) -> Result<usize> {
        let num_entries = self.port.get_many(&mut self.entries, None)?;
        Ok(num_entries)
    }

    fn wait_with_duration(&mut self, duration: Duration) -> Result<usize> {
        let num_entries = self.port.get_many(&mut self.entries, Some(duration))?;
        Ok(num_entries)
    }

    fn dispatch_completions(&mut self) {
        for entry in &self.entries {
            let overlapped = entry.lpOverlapped;

            if overlapped.is_null() {
                continue;
            }

            let index = unsafe { Interest::index_from_raw(overlapped) };
            let status = (entry.Internal as u32) as i32;
            let result = if status >= 0 {
                Ok(entry.dwNumberOfBytesTransferred)
            } else {
                let err = unsafe { RtlNtStatusToDosError(status) };
                Err(io::Error::from_raw_os_error(err as i32))
            };

            if let Some(slot) = self.ops.get_mut(index) {
                // Once the packet is dequeued, Windows no longer references
                // the OVERLAPPED allocation.
                slot.interest = None;

                // `State::complete` handles both waking a live waiter and
                // reporting whether an ignored op can now be dropped.
                let should_drop = slot.state.complete(Completion { result, flags: 0 });

                if should_drop {
                    self.ops.remove(index);
                }
            }
        }

        // All entires have been processed, so we clear the vec for the next round
        // Note: This does not deallocate the vec, so we still have the existing capacity
        self.entries.clear();
    }
}

impl Drop for IocpBackend {
    fn drop(&mut self) {
        for (_, slot) in self.ops.iter_mut() {
            match slot.state {
                State::Submitted | State::Waiting(..) => {
                    // In-flight ops must be canceled and then drained from the
                    // completion port before their OVERLAPPED memory can go.
                    if let Some(interest) = slot.interest.as_ref() {
                        let _ = unsafe { CancelIoEx(interest.handle(), interest.as_ptr()) };
                        slot.state = State::Ignored(Box::new(()));
                    }
                }
                State::Ignored(..) => {
                    // Already canceled by the future drop path; the drain loop
                    // below will wait for its completion packet.
                }
                State::Completed(..) => {
                    // Completion has already been dispatched, so no kernel
                    // reference remains.
                }
                State::Ready => unreachable!("invalid operation state"),
            }
        }

        while self
            .ops
            .iter()
            .any(|(_, slot)| matches!(slot.state, State::Ignored(..)))
        {
            if self.wait().is_err() {
                continue;
            }

            self.dispatch_completions();
        }

        self.ops.clear();
    }
}

#[inline]
fn duration_millis(duration: Option<Duration>) -> u32 {
    match duration {
        Some(duration) => {
            let millis = duration
                .checked_add(Duration::from_nanos(999_999))
                .unwrap_or(duration)
                .as_millis();

            millis.min(u32::MAX as u128) as u32
        }
        None => u32::MAX,
    }
}
