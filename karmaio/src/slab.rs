//! Pre-allocated storage for a uniform data type.
//!
//! Adapted from the `slab` crate (https://github.com/tokio-rs/slab, MIT
//! license) so the runtime has no non-platform dependencies. `Slab` provides
//! pre-allocated storage for a single data type: in exchange for giving up
//! contiguous memory layout, it returns a key that identifies each stored
//! value, and slots are reused as values are removed.
//!
//! This module is internal to the runtime, used by the operation table and the
//! signal registry; it is not part of the public API.
//!
//! # Implementation
//!
//! The slab is backed by a `Vec` of slots, each either occupied or vacant. The
//! vacant slots form a free list: each vacant slot stores the index of the next
//! vacant slot, and `next` is the head of the list. Inserting pops the head, or
//! appends a fresh slot when the list is empty. Removing a value pushes its
//! slot back onto the head of the list, so keys are reused in LIFO order.

use std::{
    iter::{self, FusedIterator},
    mem, slice,
};

/// Pre-allocated storage for a uniform data type, indexed by reusable keys.
///
/// `insert` assigns the smallest recycled key (or the next sequential key when
/// nothing is free), `get` / `remove` look values up by key, and a key is never
/// handed out again for a different value while it is still in use.
pub(crate) struct Slab<T> {
    // The backing storage; a slot is either occupied with a value or vacant.
    entries: Vec<Entry<T>>,
    // Number of occupied slots currently in the slab.
    len: usize,
    // Head of the vacant-slot free list, or the length of `entries` when the
    // list is empty.
    next: usize,
}

impl<T> Clone for Slab<T>
where
    T: Clone,
{
    fn clone(&self) -> Self {
        Self {
            entries: self.entries.clone(),
            len: self.len,
            next: self.next,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.entries.clone_from(&source.entries);
        self.len = source.len;
        self.next = source.next;
    }
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Slab::new()
    }
}

// A slot in the slab. Vacant slots double as free-list links.
#[derive(Clone)]
enum Entry<T> {
    // Index of the next vacant slot, or `Slab::next` when this is the tail.
    Vacant(usize),
    Occupied(T),
}

impl<T> Slab<T> {
    /// Construct a new, empty slab. Does not allocate.
    pub(crate) const fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 0,
            len: 0,
        }
    }

    /// Construct a new, empty slab with capacity for `capacity` values without
    /// reallocating.
    pub(crate) fn with_capacity(capacity: usize) -> Slab<T> {
        Slab {
            entries: Vec::with_capacity(capacity),
            next: 0,
            len: 0,
        }
    }

    /// Return the number of values the slab can store without reallocating.
    #[inline]
    pub(crate) fn capacity(&self) -> usize {
        self.entries.capacity()
    }

    /// Clear the slab of all values.
    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.len = 0;
        self.next = 0;
    }

    /// Return the number of stored values.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Return `true` if there are no values stored in the slab.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Return an iterator over the stored `(key, value)` pairs.
    ///
    /// The iterator walks every slot, vacant or not, so iterating a slab whose
    /// capacity far exceeds its length is not efficient.
    pub(crate) fn iter(&self) -> Iter<'_, T> {
        Iter {
            entries: self.entries.iter().enumerate(),
            len: self.len,
        }
    }

    /// Return an iterator that allows modifying each stored value.
    pub(crate) fn iter_mut(&mut self) -> IterMut<'_, T> {
        IterMut {
            entries: self.entries.iter_mut().enumerate(),
            len: self.len,
        }
    }

    /// Return a reference to the value associated with `key`, if any.
    #[inline]
    pub(crate) fn get(&self, key: usize) -> Option<&T> {
        match self.entries.get(key) {
            Some(Entry::Occupied(val)) => Some(val),
            _ => None,
        }
    }

    /// Return a mutable reference to the value associated with `key`, if any.
    #[inline]
    pub(crate) fn get_mut(&mut self, key: usize) -> Option<&mut T> {
        match self.entries.get_mut(key) {
            Some(&mut Entry::Occupied(ref mut val)) => Some(val),
            _ => None,
        }
    }

    /// Insert a value into the slab, returning the key assigned to it.
    ///
    /// The returned key can later be used to retrieve or remove the value.
    /// Additional capacity is allocated if needed.
    pub(crate) fn insert(&mut self, val: T) -> usize {
        let key = self.next;

        self.insert_at(key, val);

        key
    }

    /// Return the key of the next vacant entry, without requiring mutable
    /// access. Equivalent to `slab.vacant_entry().key()`.
    #[inline]
    pub(crate) fn vacant_key(&self) -> usize {
        self.next
    }

    /// Return a handle to a vacant entry, allowing the value to be created
    /// with knowledge of the key it will be assigned.
    pub(crate) fn vacant_entry(&mut self) -> VacantEntry<'_, T> {
        VacantEntry {
            key: self.next,
            slab: self,
        }
    }

    // Insert `val` at `key`, which must be the head of the free list (or the
    // end of the backing storage), and keep the free list consistent.
    fn insert_at(&mut self, key: usize, val: T) {
        self.len += 1;

        if key == self.entries.len() {
            self.entries.push(Entry::Occupied(val));
            self.next = key + 1;
        } else {
            self.next = match self.entries.get(key) {
                Some(&Entry::Vacant(next)) => next,
                _ => unreachable!(),
            };
            self.entries[key] = Entry::Occupied(val);
        }
    }

    /// Remove and return the value associated with `key`, if any. The key is
    /// released and may be associated with a future stored value.
    pub(crate) fn try_remove(&mut self, key: usize) -> Option<T> {
        if let Some(entry) = self.entries.get_mut(key)
            && let Entry::Occupied(_) = entry
        {
            // Replace the slot with a vacant entry pointing at the current
            // free-list head, then push `key` onto the head.
            let val = match mem::replace(entry, Entry::Vacant(self.next)) {
                Entry::Occupied(val) => val,
                Entry::Vacant(_) => unreachable!(),
            };

            self.len -= 1;
            self.next = key;
            return val.into();
        }
        None
    }

    /// Remove and return the value associated with `key`.
    ///
    /// # Panics
    ///
    /// Panics if `key` has no value associated with it.
    #[track_caller]
    pub(crate) fn remove(&mut self, key: usize) -> T {
        self.try_remove(key).expect("invalid key")
    }

    /// Return `true` if a value is associated with `key`.
    #[inline]
    pub(crate) fn contains(&self, key: usize) -> bool {
        matches!(self.entries.get(key), Some(&Entry::Occupied(_)))
    }
}

/// A handle to a vacant entry in a [`Slab`], allowing a value to be created
/// with knowledge of the key it will be assigned.
pub(crate) struct VacantEntry<'a, T> {
    slab: &'a mut Slab<T>,
    key: usize,
}

impl<'a, T> VacantEntry<'a, T> {
    /// Return the key associated with this entry.
    #[inline]
    pub(crate) fn key(&self) -> usize {
        self.key
    }

    /// Insert a value into the entry's slot, returning a mutable reference to
    /// the stored value.
    pub(crate) fn insert(self, val: T) -> &'a mut T {
        self.slab.insert_at(self.key, val);

        match self.slab.entries.get_mut(self.key) {
            Some(&mut Entry::Occupied(ref mut val)) => val,
            _ => unreachable!(),
        }
    }
}

/// An iterator over the values stored in a [`Slab`].
pub(crate) struct Iter<'a, T> {
    entries: iter::Enumerate<slice::Iter<'a, Entry<T>>>,
    len: usize,
}

impl<'a, T> Iterator for Iter<'a, T> {
    type Item = (usize, &'a T);

    fn next(&mut self) -> Option<Self::Item> {
        for (key, entry) in &mut self.entries {
            if let Entry::Occupied(ref val) = *entry {
                self.len -= 1;
                return Some((key, val));
            }
        }

        debug_assert_eq!(self.len, 0);
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<'a, T> DoubleEndedIterator for Iter<'a, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        while let Some((key, entry)) = self.entries.next_back() {
            if let Entry::Occupied(ref val) = *entry {
                self.len -= 1;
                return Some((key, val));
            }
        }

        debug_assert_eq!(self.len, 0);
        None
    }
}

impl<T> ExactSizeIterator for Iter<'_, T> {
    fn len(&self) -> usize {
        self.len
    }
}

impl<T> FusedIterator for Iter<'_, T> {}

/// A mutable iterator over the values stored in a [`Slab`].
pub(crate) struct IterMut<'a, T> {
    entries: iter::Enumerate<slice::IterMut<'a, Entry<T>>>,
    len: usize,
}

impl<'a, T> Iterator for IterMut<'a, T> {
    type Item = (usize, &'a mut T);

    fn next(&mut self) -> Option<Self::Item> {
        for (key, entry) in &mut self.entries {
            if let Entry::Occupied(ref mut val) = *entry {
                self.len -= 1;
                return Some((key, val));
            }
        }

        debug_assert_eq!(self.len, 0);
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }
}

impl<'a, T> DoubleEndedIterator for IterMut<'a, T> {
    fn next_back(&mut self) -> Option<Self::Item> {
        while let Some((key, entry)) = self.entries.next_back() {
            if let Entry::Occupied(ref mut val) = *entry {
                self.len -= 1;
                return Some((key, val));
            }
        }

        debug_assert_eq!(self.len, 0);
        None
    }
}

impl<T> ExactSizeIterator for IterMut<'_, T> {
    fn len(&self) -> usize {
        self.len
    }
}

impl<T> FusedIterator for IterMut<'_, T> {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_get_and_iter() {
        let mut slab = Slab::new();
        assert!(slab.is_empty());
        assert_eq!(slab.vacant_key(), 0);

        let a = slab.insert("a");
        let b = slab.insert("b");
        assert_eq!(slab.len(), 2);
        assert_eq!(slab.get(a), Some(&"a"));
        assert_eq!(slab.get_mut(b), Some(&mut "b"));
        assert_eq!(slab.get(99), None);
        assert!(slab.contains(a));
        assert!(!slab.contains(99));
        assert_eq!(slab.iter().collect::<Vec<_>>(), vec![(a, &"a"), (b, &"b")]);
    }

    #[test]
    fn removed_keys_are_reused_in_lifo_order() {
        let mut slab = Slab::new();
        let first = slab.insert("first");
        let second = slab.insert("second");
        slab.remove(first);
        slab.remove(second);

        let third = slab.insert("third");
        assert_eq!(third, second);
        let fourth = slab.insert("fourth");
        assert_eq!(fourth, first);

        assert_eq!(slab.get(third), Some(&"third"));
        assert_eq!(slab.get(fourth), Some(&"fourth"));
    }

    #[test]
    fn vacant_entry_inserts_at_its_key() {
        let mut slab = Slab::new();
        let key = {
            let entry = slab.vacant_entry();
            let key = entry.key();
            entry.insert(key);
            key
        };

        assert_eq!(slab.get(key), Some(&key));
    }

    #[test]
    fn iter_mut_modifies_stored_values() {
        let mut slab = Slab::new();
        let a = slab.insert(1);
        let b = slab.insert(2);

        for (_, val) in slab.iter_mut() {
            *val += 10;
        }

        assert_eq!(slab.get(a), Some(&11));
        assert_eq!(slab.get(b), Some(&12));
    }

    #[test]
    fn try_remove_reports_missing_keys() {
        let mut slab = Slab::new();
        let key = slab.insert("value");
        assert_eq!(slab.try_remove(key), Some("value"));
        assert_eq!(slab.try_remove(key), None);
        assert!(!slab.contains(key));
        assert!(slab.is_empty());
    }

    #[test]
    fn clear_drops_all_values() {
        let mut slab = Slab::new();
        slab.insert("a");
        slab.insert("b");
        slab.clear();

        assert!(slab.is_empty());
        assert_eq!(slab.vacant_key(), 0);
    }

    #[test]
    fn clone_copies_slots_and_key_reuse_order() {
        let mut slab = Slab::new();
        let a = slab.insert("a");
        slab.insert("b");
        slab.remove(a);

        let cloned = slab.clone();
        assert_eq!(cloned.len(), 1);
        assert_eq!(cloned.vacant_key(), 0);
        assert_eq!(cloned.iter().next().map(|(_, val)| *val), Some("b"));
    }

    #[test]
    fn with_capacity_preallocates() {
        let slab: Slab<i32> = Slab::with_capacity(10);
        assert_eq!(slab.capacity(), 10);
        assert!(slab.is_empty());
    }
}
