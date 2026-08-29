//! Generational cancellation scopes owned by the driver.
//!
//! A [`ScopeId`] identifies one cancellation source. Tokens are copies of that
//! id. Recycling a slot bumps the generation so a dropped source cannot cancel
//! a later occupant. Attachments are stored both on the scope and on an
//! inverse `OpKey` map so completion does not scan every live source.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::ptr::NonNull;
use std::task::Waker;

use crate::driver::ops::OpKey;
use crate::slab::Slab;

/// Generational identity for a cancellation scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct ScopeId {
    table: u64,
    entry: u64,
}

struct ScopeFrame {
    id: ScopeId,
    parent: Option<NonNull<ScopeFrame>>,
}

scoped_thread_local!(static CURRENT_SCOPE: ScopeFrame);

/// Push `id` for the duration of `f`. Nested combinators form a linked stack.
pub(crate) fn with_scope<R>(id: ScopeId, f: impl FnOnce() -> R) -> R {
    let parent = CURRENT_SCOPE
        .is_set()
        .then(|| CURRENT_SCOPE.with(|current| NonNull::from(current)));
    let frame = ScopeFrame { id, parent };
    CURRENT_SCOPE.set(&frame, f)
}

/// Call `f` for every cancellation scope wrapping the current poll.
pub(crate) fn for_each_current_scope(mut f: impl FnMut(ScopeId)) {
    if CURRENT_SCOPE.is_set() {
        CURRENT_SCOPE.with(|current| {
            let mut frame = Some(NonNull::from(current));
            while let Some(pointer) = frame {
                // Safety: every frame is stack-owned by a currently active
                // `CURRENT_SCOPE.set` call. Parent frames outlive nested calls,
                // and the chain is only walked synchronously during that scope.
                let current = unsafe { pointer.as_ref() };
                f(current.id);
                frame = current.parent;
            }
        });
    }
}

std::thread_local! {
    // Cancellation sources and tokens are `!Send`, so identities only need to
    // be unique among runtimes created on this thread.
    static NEXT_TABLE_ID: Cell<u64> = const { Cell::new(1) };
}

fn next_table_id() -> u64 {
    NEXT_TABLE_ID.with(|next| {
        let id = next.get();
        next.set(id.checked_add(1).expect("cancellation scope table identity exhausted"));
        id
    })
}

impl ScopeId {
    fn from_parts(table: u64, slot: usize, generation: u32) -> Option<Self> {
        let slot = u32::try_from(slot).ok()?;
        if generation == 0 {
            return None;
        }
        Some(Self {
            table,
            entry: (u64::from(generation) << 32) | u64::from(slot),
        })
    }

    fn slot(self) -> usize {
        (self.entry as u32) as usize
    }

    fn generation(self) -> u32 {
        (self.entry >> 32) as u32
    }
}

#[cfg(test)]
pub(crate) fn current_scope_ids() -> Vec<ScopeId> {
    let mut ids = Vec::new();
    for_each_current_scope(|id| ids.push(id));
    ids
}

enum Scope {
    Active {
        ops: HashSet<OpKey>,
        waiters: HashMap<WaiterId, Waker>,
    },
    Cancelled,
}

/// Identity of one `CancellationToken::cancelled` registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub(crate) struct WaiterId(u64);

/// Result of polling a cancellation waiter.
pub(crate) enum SubscribeResult {
    Ready,
    Pending(WaiterId),
}

/// Subscription result plus a waker to drop after releasing the table.
pub(crate) struct ScopeSubscribe {
    pub result: SubscribeResult,
    pub deferred_drop: Option<Waker>,
}

/// Compact list of scopes attached to one operation.
enum AttachedScopes {
    Empty,
    One(ScopeId),
    Two(ScopeId, ScopeId),
    More(Vec<ScopeId>),
}

impl AttachedScopes {
    fn insert(&mut self, id: ScopeId) {
        if self.contains(id) {
            return;
        }
        *self = match std::mem::replace(self, Self::Empty) {
            Self::Empty => Self::One(id),
            Self::One(a) => Self::Two(a, id),
            Self::Two(a, b) => Self::More(vec![a, b, id]),
            Self::More(mut ids) => {
                ids.push(id);
                Self::More(ids)
            }
        };
    }

    fn contains(&self, id: ScopeId) -> bool {
        match self {
            Self::Empty => false,
            Self::One(a) => *a == id,
            Self::Two(a, b) => *a == id || *b == id,
            Self::More(ids) => ids.contains(&id),
        }
    }

    fn iter(&self) -> impl Iterator<Item = ScopeId> {
        match self {
            Self::Empty => AttachedIter::Zero,
            Self::One(a) => AttachedIter::One(*a),
            Self::Two(a, b) => AttachedIter::Two(*a, *b, 0),
            Self::More(ids) => AttachedIter::More(ids.iter()),
        }
    }
}

enum AttachedIter<'a> {
    Zero,
    One(ScopeId),
    Two(ScopeId, ScopeId, u8),
    More(std::slice::Iter<'a, ScopeId>),
}

impl Iterator for AttachedIter<'_> {
    type Item = ScopeId;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Zero => None,
            Self::One(id) => {
                let id = *id;
                *self = Self::Zero;
                Some(id)
            }
            Self::Two(a, b, index) => {
                let id = match *index {
                    0 => *a,
                    1 => *b,
                    _ => return None,
                };
                *index += 1;
                Some(id)
            }
            Self::More(iter) => iter.next().copied(),
        }
    }
}

/// Result of attaching an operation to a scope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AttachResult {
    /// The operation is registered with an active scope.
    Attached,
    /// The scope is already cancelled or no longer exists.
    Cancelled,
}

/// Ops and waiters snapshotted by [`ScopeTable::cancel`].
pub(crate) struct ScopeCancel {
    pub ops: Vec<OpKey>,
    pub waiters: Vec<Waker>,
}

/// Driver-owned registry of cancellation scopes.
pub(crate) struct ScopeTable {
    id: u64,
    entries: Slab<Scope>,
    generations: Vec<u32>,
    attachments: HashMap<OpKey, AttachedScopes>,
    next_waiter: u64,
}

impl ScopeTable {
    pub(crate) fn new() -> Self {
        Self {
            id: next_table_id(),
            entries: Slab::new(),
            generations: Vec::new(),
            attachments: HashMap::new(),
            next_waiter: 1,
        }
    }

    pub(crate) fn insert(&mut self) -> ScopeId {
        let slot = self.entries.vacant_key();
        if self.generations.len() <= slot {
            self.generations.resize(slot + 1, 1);
        }
        let generation = self.generations[slot];
        let id = ScopeId::from_parts(self.id, slot, generation).expect("scope generation exhausted");
        self.entries.insert(Scope::Active {
            ops: HashSet::new(),
            waiters: HashMap::new(),
        });
        id
    }

    /// Remove a scope. A missing or stale id is a no-op.
    ///
    /// If the scope was still `Active`, it is cancelled first so the caller
    /// can issue platform cancels for the snapshotted ops.
    pub(crate) fn remove(&mut self, id: ScopeId) -> ScopeCancel {
        let pending = self.cancel(id);
        if self.get(id).is_none() {
            return pending;
        }

        let slot = id.slot();
        let generation = id.generation();
        if generation < u32::MAX {
            let _ = self.entries.try_remove(slot);
            self.generations[slot] = generation + 1;
        }
        pending
    }

    pub(crate) fn is_cancelled(&self, id: ScopeId) -> bool {
        match self.get(id) {
            Some(Scope::Cancelled) | None => true,
            Some(Scope::Active { .. }) => false,
        }
    }

    /// Mark the scope cancelled and snapshot its ops and waiters.
    ///
    /// Idempotent: a second call returns empty vectors. Does not issue
    /// platform cancellation; the driver does that from the snapshot.
    pub(crate) fn cancel(&mut self, id: ScopeId) -> ScopeCancel {
        let Some(scope) = self.get_mut(id) else {
            return ScopeCancel {
                ops: Vec::new(),
                waiters: Vec::new(),
            };
        };
        match std::mem::replace(scope, Scope::Cancelled) {
            Scope::Active { ops, waiters } => ScopeCancel {
                ops: ops.into_iter().collect(),
                waiters: waiters.into_values().collect(),
            },
            Scope::Cancelled => ScopeCancel {
                ops: Vec::new(),
                waiters: Vec::new(),
            },
        }
    }

    pub(crate) fn attach(&mut self, id: ScopeId, key: OpKey) -> AttachResult {
        match self.get_mut(id) {
            Some(Scope::Active { ops, .. }) => {
                ops.insert(key);
            }
            Some(Scope::Cancelled) | None => return AttachResult::Cancelled,
        }
        self.attachments.entry(key).or_insert(AttachedScopes::Empty).insert(id);
        AttachResult::Attached
    }

    /// Remove an operation from every scope that registered it.
    pub(crate) fn detach(&mut self, key: OpKey) {
        let Some(attached) = self.attachments.remove(&key) else {
            return;
        };
        for id in attached.iter() {
            if let Some(Scope::Active { ops, .. }) = self.get_mut(id) {
                ops.remove(&key);
            }
        }
    }

    /// Register or update a waker for `CancellationToken::cancelled`.
    pub(crate) fn subscribe(&mut self, id: ScopeId, registration: Option<WaiterId>, waker: Waker) -> ScopeSubscribe {
        if let Some(registration) = registration {
            match self.get_mut(id) {
                Some(Scope::Cancelled) | None => {
                    return ScopeSubscribe {
                        result: SubscribeResult::Ready,
                        deferred_drop: Some(waker),
                    };
                }
                Some(Scope::Active { waiters, .. }) => {
                    if let Some(registered) = waiters.get_mut(&registration) {
                        let replaced = if registered.will_wake(&waker) {
                            waker
                        } else {
                            std::mem::replace(registered, waker)
                        };
                        return ScopeSubscribe {
                            result: SubscribeResult::Pending(registration),
                            deferred_drop: Some(replaced),
                        };
                    }
                }
            }
        }

        let registration = WaiterId(self.next_waiter);
        self.next_waiter = self
            .next_waiter
            .checked_add(1)
            .expect("cancellation waiter identity exhausted");
        match self.get_mut(id) {
            Some(Scope::Cancelled) | None => ScopeSubscribe {
                result: SubscribeResult::Ready,
                deferred_drop: Some(waker),
            },
            Some(Scope::Active { waiters, .. }) => {
                waiters.insert(registration, waker);
                ScopeSubscribe {
                    result: SubscribeResult::Pending(registration),
                    deferred_drop: None,
                }
            }
        }
    }

    /// Remove a dropped `CancellationToken::cancelled` waiter.
    pub(crate) fn unsubscribe(&mut self, id: ScopeId, registration: WaiterId) -> Option<Waker> {
        if let Some(Scope::Active { waiters, .. }) = self.get_mut(id) {
            waiters.remove(&registration)
        } else {
            None
        }
    }

    #[cfg(test)]
    pub(crate) fn waiter_count(&self, id: ScopeId) -> usize {
        match self.get(id) {
            Some(Scope::Active { waiters, .. }) => waiters.len(),
            Some(Scope::Cancelled) | None => 0,
        }
    }

    fn get(&self, id: ScopeId) -> Option<&Scope> {
        if id.table != self.id {
            return None;
        }
        let slot = id.slot();
        let generation = id.generation();
        if self.generations.get(slot) != Some(&generation) {
            return None;
        }
        self.entries.get(slot)
    }

    fn get_mut(&mut self, id: ScopeId) -> Option<&mut Scope> {
        if id.table != self.id {
            return None;
        }
        let slot = id.slot();
        let generation = id.generation();
        if self.generations.get(slot) != Some(&generation) {
            return None;
        }
        self.entries.get_mut(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::op_table::OpTable;

    fn fake_key(label: &'static str) -> OpKey {
        // Each test gets its own table so keys only need to be distinct within
        // one case. The table is leaked into the key value, not retained.
        let mut table = OpTable::new(4).unwrap();
        table.insert(label).unwrap()
    }

    #[test]
    fn cancel_is_idempotent_and_sticky() {
        let mut table = ScopeTable::new();
        let id = table.insert();
        assert!(!table.is_cancelled(id));

        let first = table.cancel(id);
        assert!(first.ops.is_empty());
        assert!(table.is_cancelled(id));

        let second = table.cancel(id);
        assert!(second.ops.is_empty());
        assert!(second.waiters.is_empty());
        assert!(table.is_cancelled(id));
    }

    #[test]
    fn attach_then_cancel_snapshots_the_op() {
        let mut table = ScopeTable::new();
        let id = table.insert();
        let key = fake_key("op");

        assert_eq!(table.attach(id, key), AttachResult::Attached);
        let cancelled = table.cancel(id);
        assert_eq!(cancelled.ops, vec![key]);
        assert_eq!(table.attach(id, fake_key("later")), AttachResult::Cancelled);
    }

    #[test]
    fn detach_removes_the_op_from_an_active_scope() {
        let mut table = ScopeTable::new();
        let id = table.insert();
        let key = fake_key("op");

        assert_eq!(table.attach(id, key), AttachResult::Attached);
        table.detach(key);
        assert!(table.cancel(id).ops.is_empty());
    }

    #[test]
    fn detach_after_cancel_is_a_no_op_on_the_empty_set() {
        let mut table = ScopeTable::new();
        let id = table.insert();
        let key = fake_key("op");
        table.attach(id, key);
        let _ = table.cancel(id);
        table.detach(key);
        assert!(table.is_cancelled(id));
    }

    #[test]
    fn nested_scopes_both_track_the_same_op() {
        let mut table = ScopeTable::new();
        let a = table.insert();
        let b = table.insert();
        let key = fake_key("op");

        assert_eq!(table.attach(a, key), AttachResult::Attached);
        assert_eq!(table.attach(b, key), AttachResult::Attached);

        let cancelled_a = table.cancel(a);
        assert_eq!(cancelled_a.ops, vec![key]);
        // The inverse map still lists both until detach; `b` still has the key.
        let cancelled_b = table.cancel(b);
        assert_eq!(cancelled_b.ops, vec![key]);
    }

    #[test]
    fn dropped_scope_generation_does_not_alias_a_reuse() {
        let mut table = ScopeTable::new();
        let first = table.insert();
        let _ = table.remove(first);

        let second = table.insert();
        assert_ne!(first, second);
        assert!(table.is_cancelled(first));
        assert!(!table.is_cancelled(second));
        assert_eq!(table.attach(first, fake_key("stale")), AttachResult::Cancelled);
        assert_eq!(table.attach(second, fake_key("fresh")), AttachResult::Attached);
    }

    #[test]
    fn missing_scope_is_cancelled() {
        let mut table = ScopeTable::new();
        let mut other = ScopeTable::new();
        let id = other.insert();
        assert!(table.is_cancelled(id));
        assert!(matches!(
            table.subscribe(id, None, std::task::Waker::noop().clone()).result,
            SubscribeResult::Ready
        ));
    }

    #[test]
    fn subscribe_on_cancelled_scope_completes_immediately() {
        let mut table = ScopeTable::new();
        let id = table.insert();
        let _ = table.cancel(id);
        assert!(matches!(
            table.subscribe(id, None, std::task::Waker::noop().clone()).result,
            SubscribeResult::Ready
        ));
    }

    #[test]
    fn unsubscribe_releases_waiter() {
        let mut table = ScopeTable::new();
        let id = table.insert();
        let registration = match table.subscribe(id, None, std::task::Waker::noop().clone()).result {
            SubscribeResult::Pending(registration) => registration,
            SubscribeResult::Ready => panic!("active scope completed a waiter"),
        };
        assert_eq!(table.waiter_count(id), 1);

        drop(table.unsubscribe(id, registration));
        assert_eq!(table.waiter_count(id), 0);
        assert!(table.cancel(id).waiters.is_empty());
    }
}
