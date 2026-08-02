//! Generational operation identities shared by all platform backends.
//!
//! [`OpKey`] / [`OpTable`] track live operations. [`CompletionKey`] decodes the
//! reserved wake/cancel control tokens so they are never confused with op keys.

use std::io;

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
    // Keep the highest generation unused so ordinary keys never overlap the
    // reserved all-ones completion-control values in [`CompletionKey`].
    const GENERATION_MAX: usize = Self::INDEX_MASK - 1;

    /// Largest number of operation slots representable by this token.
    pub(crate) const MAX_CAPACITY: usize = Self::INDEX_MASK;

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
    pub(crate) fn slot_is_representable(slot: usize) -> bool {
        slot < Self::MAX_CAPACITY
    }

    #[inline]
    fn from_components(slot: usize, generation: usize) -> Option<Self> {
        if !Self::slot_is_representable(slot) || !(1..=Self::GENERATION_MAX).contains(&generation) {
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

/// Non-operation completion identifiers used by platform backends.
///
/// These are decoded in one place so an operation table never receives a wake
/// or cancellation acknowledgement as if it were an op key.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // Control packets are backend-specific.
pub(crate) enum CompletionKey {
    Operation(OpKey),
    Wake,
    Cancel,
    WakeCancel,
}

#[allow(dead_code)] // Control packets are backend-specific.
impl CompletionKey {
    const WAKE_RAW: usize = usize::MAX;
    const CANCEL_RAW: usize = usize::MAX - 1;
    const WAKE_CANCEL_RAW: usize = usize::MAX - 2;

    #[inline]
    pub(crate) const fn wake_raw() -> usize {
        Self::WAKE_RAW
    }

    #[inline]
    pub(crate) const fn cancel_raw() -> usize {
        Self::CANCEL_RAW
    }

    #[inline]
    pub(crate) const fn wake_cancel_raw() -> usize {
        Self::WAKE_CANCEL_RAW
    }

    #[inline]
    pub(crate) fn decode(raw: u64) -> Option<Self> {
        let raw = usize::try_from(raw).ok()?;

        match raw {
            Self::WAKE_RAW => Some(Self::Wake),
            Self::CANCEL_RAW => Some(Self::Cancel),
            Self::WAKE_CANCEL_RAW => Some(Self::WakeCancel),
            _ => OpKey::from_raw(raw).map(Self::Operation),
        }
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
        if capacity == 0 || capacity > OpKey::MAX_CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "driver operation capacity is outside the representable key range",
            ));
        }

        Self::new_with_generation_limit(capacity, OpKey::GENERATION_MAX)
    }

    fn new_with_generation_limit(capacity: usize, generation_limit: usize) -> io::Result<Self> {
        if !(1..=OpKey::GENERATION_MAX).contains(&generation_limit) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "driver operation generation limit is outside the representable range",
            ));
        }

        Ok(Self {
            entries: Slab::with_capacity(capacity),
            generations: Vec::with_capacity(capacity),
            generation_limit,
            active_len: 0,
        })
    }

    #[cfg(test)]
    fn new_for_test(capacity: usize, generation_limit: usize) -> io::Result<Self> {
        Self::new_with_generation_limit(capacity, generation_limit)
    }

    pub(crate) fn insert(&mut self, value: T) -> io::Result<OpKey> {
        self.insert_with_key(|_| value)
    }

    pub(crate) fn insert_with_key(&mut self, create: impl FnOnce(OpKey) -> T) -> io::Result<OpKey> {
        let slot = self.entries.vacant_key();
        if !OpKey::slot_is_representable(slot) {
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
    use super::{CompletionKey, OpKey, OpTable};

    #[test]
    fn stale_keys_are_rejected_after_slot_reuse() {
        let mut table = OpTable::new(1).unwrap();
        let first = table.insert("first").unwrap();
        assert_eq!(table.remove(first), Some("first"));

        let second = table.insert("second").unwrap();
        assert_ne!(first, second);
        assert_eq!(table.get(first), None);
        assert_eq!(table.get_mut(first), None);
        assert_eq!(table.remove(first), None);
        assert_eq!(table.get(second), Some(&"second"));
    }

    #[test]
    fn insertion_factory_receives_the_stored_key() {
        let mut table = OpTable::new(1).unwrap();
        let key = table.insert_with_key(|key| key).unwrap();
        assert_eq!(table.get(key), Some(&key));
    }

    #[test]
    fn completion_key_reserves_control_values() {
        assert_eq!(
            CompletionKey::decode(CompletionKey::wake_raw() as u64),
            Some(CompletionKey::Wake)
        );
        assert_eq!(
            CompletionKey::decode(CompletionKey::cancel_raw() as u64),
            Some(CompletionKey::Cancel)
        );
        assert_eq!(
            CompletionKey::decode(CompletionKey::wake_cancel_raw() as u64),
            Some(CompletionKey::WakeCancel)
        );

        // Reserved control values must never decode as ordinary operation keys.
        assert_eq!(OpKey::from_raw(CompletionKey::wake_raw()), None);
        assert_eq!(OpKey::from_raw(CompletionKey::cancel_raw()), None);
        assert_eq!(OpKey::from_raw(CompletionKey::wake_cancel_raw()), None);

        let key = OpTable::new(1).unwrap().insert(()).unwrap();
        assert_eq!(
            CompletionKey::decode(key.as_u64()),
            Some(CompletionKey::Operation(key))
        );
        assert_eq!(CompletionKey::decode(0), None);
        assert_eq!(CompletionKey::decode(OpKey::INVALID.as_u64()), None);
    }

    #[test]
    fn capacity_must_fit_token_encoding() {
        assert!(OpTable::<()>::new(0).is_err());
        assert!(OpTable::<()>::new(OpKey::MAX_CAPACITY.saturating_add(1)).is_err());
        assert!(OpKey::slot_is_representable(OpKey::MAX_CAPACITY - 1));
        assert!(!OpKey::slot_is_representable(OpKey::MAX_CAPACITY));
    }

    #[test]
    fn invalid_key_lookup_is_safe() {
        let mut table = OpTable::new(1).unwrap();
        let key = table.insert("active").unwrap();

        assert_eq!(table.get(OpKey::INVALID), None);
        assert_eq!(table.get_mut(OpKey::INVALID), None);
        assert_eq!(table.remove(OpKey::INVALID), None);
        assert_eq!(table.get(key), Some(&"active"));
    }

    #[test]
    fn exhausted_generation_retires_slot_instead_of_wrapping() {
        let mut table = OpTable::new_for_test(1, 2).unwrap();
        let first = table.insert("first").unwrap();
        assert_eq!(table.remove(first), Some("first"));

        let second = table.insert("second").unwrap();
        assert_eq!(
            first.components().map(|(slot, _)| slot),
            second.components().map(|(slot, _)| slot)
        );
        assert_eq!(table.remove(second), Some("second"));

        let third = table.insert("third").unwrap();
        assert_ne!(
            first.components().map(|(slot, _)| slot),
            third.components().map(|(slot, _)| slot)
        );
        assert_eq!(table.get(first), None);
        assert_eq!(table.get(second), None);
    }

    #[test]
    fn clear_invalidates_active_keys_and_retires_exhausted_slots() {
        let mut table = OpTable::new_for_test(1, 2).unwrap();
        let first = table.insert("first").unwrap();

        table.clear();
        assert!(table.is_empty());
        assert_eq!(table.get(first), None);

        let second = table.insert("second").unwrap();
        assert_ne!(first, second);
        table.clear();

        let third = table.insert("third").unwrap();
        assert_ne!(
            second.components().map(|(slot, _)| slot),
            third.components().map(|(slot, _)| slot)
        );
        assert_eq!(table.get(second), None);
    }
}
