use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
};

use crate::driver::Handle;
use crate::driver::backends::Operation;
use slab::Slab;

/// A generational identity for an operation stored by a backend.
///
/// The low half identifies a slab slot and the high half identifies the
/// generation of that slot. Reusing a slot therefore cannot make a late
/// kernel completion refer to a newer operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[repr(transparent)]
pub(crate) struct OpKey(usize);

impl OpKey {
    const INDEX_BITS: u32 = usize::BITS / 2;
    const INDEX_MASK: usize = (1usize << Self::INDEX_BITS) - 1;
    const GENERATION_MAX: usize = Self::INDEX_MASK - 1;

    pub(crate) const INVALID: Self = Self(0);

    #[inline]
    pub(crate) fn raw(self) -> usize {
        self.0
    }

    #[inline]
    pub(crate) fn as_u64(self) -> u64 {
        self.0 as u64
    }

    #[inline]
    fn from_components(slot: usize, generation: usize) -> Option<Self> {
        if slot >= Self::INDEX_MASK || !(1..=Self::GENERATION_MAX).contains(&generation) {
            return None;
        }

        // Slot zero is represented by one so that zero remains invalid.
        Some(Self((generation << Self::INDEX_BITS) | (slot + 1)))
    }

    #[inline]
    pub(crate) fn from_raw(raw: usize) -> Option<Self> {
        let slot = raw & Self::INDEX_MASK;
        let generation = raw >> Self::INDEX_BITS;
        if slot == 0 || generation == 0 || generation > Self::GENERATION_MAX {
            return None;
        }
        Some(Self(raw))
    }

    #[inline]
    fn components(self) -> Option<(usize, usize)> {
        let one_based_slot = self.0 & Self::INDEX_MASK;
        let generation = self.0 >> Self::INDEX_BITS;
        if one_based_slot == 0 || generation == 0 || generation > Self::GENERATION_MAX {
            return None;
        }
        Some((one_based_slot - 1, generation))
    }
}

/// A slab with generation tracking for backend operation identities.
pub(crate) struct OpTable<T> {
    entries: Slab<Option<T>>,
    generations: Vec<usize>,
    generation_limit: usize,
    active_len: usize,
}

impl<T> OpTable<T> {
    pub(crate) fn new(capacity: usize) -> io::Result<Self> {
        if capacity == 0 || capacity >= OpKey::INDEX_MASK {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "driver operation capacity is outside the representable key range",
            ));
        }

        Ok(Self {
            entries: Slab::with_capacity(capacity),
            generations: Vec::with_capacity(capacity),
            generation_limit: OpKey::GENERATION_MAX,
            active_len: 0,
        })
    }

    pub(crate) fn insert(&mut self, value: T) -> io::Result<OpKey> {
        self.insert_with_key(|_| value)
    }

    pub(crate) fn insert_with_key(&mut self, create: impl FnOnce(OpKey) -> T) -> io::Result<OpKey> {
        let slot = self.entries.vacant_key();
        if slot >= OpKey::INDEX_MASK {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "driver operation table exhausted its key range",
            ));
        }

        if self.generations.len() <= slot {
            self.generations.resize(slot + 1, 1);
        }

        let key = OpKey::from_components(slot, self.generations[slot])
            .ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "driver operation generation exhausted"))?;

        self.entries.vacant_entry().insert(Some(create(key)));
        self.active_len += 1;
        Ok(key)
    }

    #[cfg(test)]
    #[inline]
    fn get(&self, key: OpKey) -> Option<&T> {
        let (slot, generation) = key.components()?;
        (self.generations.get(slot)? == &generation)
            .then(|| self.entries.get(slot)?.as_ref())
            .flatten()
    }

    #[inline]
    pub(crate) fn get_mut(&mut self, key: OpKey) -> Option<&mut T> {
        let (slot, generation) = key.components()?;
        (self.generations.get(slot)? == &generation)
            .then(|| self.entries.get_mut(slot)?.as_mut())
            .flatten()
    }

    pub(crate) fn remove(&mut self, key: OpKey) -> Option<T> {
        let (slot, generation) = key.components()?;
        if self.generations.get(slot)? != &generation {
            return None;
        }

        let value = self.entries.get_mut(slot)?.take()?;
        self.active_len -= 1;

        if generation >= self.generation_limit {
            // Retire the slot rather than allowing generation wraparound.
            return Some(value);
        }

        self.generations[slot] = generation + 1;
        let retired = self.entries.remove(slot);
        debug_assert!(retired.is_none());
        Some(value)
    }

    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.active_len == 0
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (OpKey, &T)> {
        let generations = &self.generations;
        self.entries
            .iter()
            .filter_map(move |(slot, value)| OpKey::from_components(slot, generations[slot]).zip(value.as_ref()))
    }

    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = (OpKey, &mut T)> {
        let generations = &self.generations;
        self.entries
            .iter_mut()
            .filter_map(move |(slot, value)| OpKey::from_components(slot, generations[slot]).zip(value.as_mut()))
    }

    pub(crate) fn clear(&mut self) {
        let active: Vec<_> = self
            .entries
            .iter()
            .filter_map(|(slot, value)| value.as_ref().map(|_| (slot, self.generations[slot])))
            .collect();

        for (slot, generation) in active {
            let value = self.entries.get_mut(slot).and_then(Option::take);
            drop(value);

            if generation < self.generation_limit {
                self.generations[slot] = generation + 1;
                let retired = self.entries.remove(slot);
                debug_assert!(retired.is_none());
            }
        }
        self.active_len = 0;
    }
}

#[cfg(test)]
mod key_tests {
    use super::OpTable;

    #[test]
    fn stale_keys_are_rejected_after_slot_reuse() {
        let mut table = OpTable::new(1).unwrap();
        let first = table.insert("first").unwrap();
        assert_eq!(table.remove(first), Some("first"));

        let second = table.insert("second").unwrap();
        assert_ne!(first, second);
        assert_eq!(table.get(first), None);
        assert_eq!(table.get(second), Some(&"second"));
    }
}

// Always available: every `SharedIoHandle<T>` path closes through the driver.
pub(crate) mod close;

// Filesystem ops (`feature = "fs"`).
#[cfg(feature = "fs")]
pub(crate) mod create_dir;
#[cfg(feature = "fs")]
pub(crate) mod hardlink;
#[cfg(feature = "fs")]
pub(crate) mod open;
#[cfg(feature = "fs")]
pub(crate) mod read_at;
#[cfg(feature = "fs")]
pub(crate) mod readv;
#[cfg(feature = "fs")]
pub(crate) mod rename;
#[cfg(feature = "fs")]
pub(crate) mod set_permissions;
#[cfg(feature = "fs")]
pub(crate) mod stat;
#[cfg(feature = "fs")]
pub(crate) mod symlink;
#[cfg(feature = "fs")]
pub(crate) mod sync;
#[cfg(feature = "fs")]
pub(crate) mod truncate;
#[cfg(feature = "fs")]
pub(crate) mod unlink;
#[cfg(feature = "fs")]
pub(crate) mod write_at;
#[cfg(feature = "fs")]
pub(crate) mod writev;

// Network ops (`feature = "net"`).
#[cfg(feature = "net")]
pub(crate) mod accept;
#[cfg(feature = "net")]
pub(crate) mod connect;
#[cfg(feature = "net")]
pub(crate) mod recv;
#[cfg(feature = "net")]
pub(crate) mod recv_from;
#[cfg(feature = "net")]
pub(crate) mod recvmsg;
#[cfg(feature = "net")]
pub(crate) mod send;
#[cfg(feature = "net")]
pub(crate) mod send_to;
#[cfg(feature = "net")]
pub(crate) mod sendmsg;

// Process / pipe stream ops (`feature = "process"`).
// Offset-less read/write are used by child stdio pipes (not seekable).
#[cfg(feature = "process")]
pub(crate) mod read;
#[cfg(all(feature = "process", target_os = "linux"))]
pub(crate) mod wait_process;
#[cfg(feature = "process")]
pub(crate) mod write;

/// Terminal result shared by the backend protocols.
///
/// The result intentionally contains only the operation result. Backend
/// completion metadata stays in the backend that owns it, which keeps the
/// common operation contract independent of io_uring CQE flags and kqueue or
/// IOCP details.
pub(crate) struct Completion {
    pub(crate) result: io::Result<u32>,
}

/// A typed one-shot operation future shared by all backends.
///
/// The future owns the logical operation payload. The selected backend owns
/// only the lifecycle slot and any platform-specific submission state, and
/// drives this future through the target-local `Operation` protocol.
pub(crate) struct Op<T: Operation + 'static> {
    driver: Handle,
    key: OpKey,
    data: Option<T>,
}

impl<T: Operation + 'static> Op<T> {
    pub(crate) fn new(key: OpKey, data: T, driver: Handle) -> Self {
        Self {
            driver,
            key,
            data: Some(data),
        }
    }

    pub(crate) fn key(&self) -> OpKey {
        self.key
    }

    pub(crate) fn take_data(&mut self) -> Option<T> {
        self.data.take()
    }

    #[allow(dead_code)]
    pub(crate) fn data_ref(&self) -> Option<&T> {
        self.data.as_ref()
    }

    #[allow(dead_code)]
    pub(crate) fn data_mut(&mut self) -> Option<&mut T> {
        self.data.as_mut()
    }
}

impl<T: Operation + Unpin + 'static> Future for Op<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.driver
            .upgrade()
            .expect("Not in runtime context")
            .poll_op(self.get_mut(), cx)
    }
}

impl<T: Operation + 'static> Drop for Op<T> {
    fn drop(&mut self) {
        if let Some(driver) = self.driver.upgrade() {
            driver.remove_op(self);
        }
    }
}

/// Send-able unit of work for the blocking thread pool.
///
/// Built inside a readiness backend's submission attempt when an operation
/// must run on the blocking pool.
/// Captures only `Send` state (paths, raw fds, flags) so the runtime thread can
/// keep non-`Send` op data (e.g. `SharedIoHandle`) while the syscall runs off-thread.
/// Used on kqueue Unix targets / Windows; io_uring handles equivalent work in-kernel.
#[allow(dead_code)]
pub(crate) struct BlockingJob {
    work: Box<dyn FnOnce() -> Completion + Send + 'static>,
}

#[allow(dead_code)] // Used on macOS / Windows; unused on pure io_uring Linux builds.
impl BlockingJob {
    pub(crate) fn new(work: impl FnOnce() -> Completion + Send + 'static) -> Self {
        Self { work: Box::new(work) }
    }

    pub(crate) fn run(self) -> Completion {
        (self.work)()
    }
}
