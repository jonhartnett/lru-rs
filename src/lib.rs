// MIT License

// Copyright (c) 2016 Jerome Froelich

// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:

// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! An implementation of a LRU cache. The cache supports `get`, `get_mut`, `put`,
//! and `pop` operations, all of which are O(1). This crate was heavily influenced
//! by the [LRU Cache implementation in an earlier version of Rust's std::collections crate](https://doc.rust-lang.org/0.12.0/std/collections/lru_cache/struct.LruCache.html).
//!
//! ## Example
//!
//! ```rust
//! extern crate lru;
//!
//! use lru::LruCache;
//!
//! fn main() {
//!         let mut cache = LruCache::new(2);
//!         cache.put("apple", 3);
//!         cache.put("banana", 2);
//!
//!         assert_eq!(*cache.get(&"apple").unwrap(), 3);
//!         assert_eq!(*cache.get(&"banana").unwrap(), 2);
//!         assert!(cache.get(&"pear").is_none());
//!
//!         assert_eq!(cache.put("banana", 4), Some(2));
//!         assert_eq!(cache.put("pear", 5), None);
//!
//!         assert_eq!(*cache.get(&"pear").unwrap(), 5);
//!         assert_eq!(*cache.get(&"banana").unwrap(), 4);
//!         assert!(cache.get(&"apple").is_none());
//!
//!         {
//!             let v = cache.get_mut(&"banana").unwrap();
//!             *v = 6;
//!         }
//!
//!         assert_eq!(*cache.get(&"banana").unwrap(), 6);
//! }
//! ```

#![no_std]

#[cfg(feature = "hashbrown")]
extern crate hashbrown;

#[cfg(test)]
extern crate scoped_threadpool;

use alloc::borrow::Borrow;
use alloc::boxed::Box;
use core::fmt;
use core::fmt::{Debug, Formatter};
use core::hash::{BuildHasher, Hash, Hasher};
use core::iter::FusedIterator;
use core::marker::PhantomData;
use core::mem::{self, replace};
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicUsize, Ordering};
use core::usize;
#[cfg(not(feature = "no_std"))]
use std::borrow::ToOwned;

#[cfg(any(test, not(feature = "no_std")))]
extern crate std;

#[cfg(feature = "hashbrown")]
use hashbrown::HashSet;
#[cfg(not(feature = "hashbrown"))]
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

extern crate alloc;

// This type exists to allow a "blanket" Borrow impl for KeyRef without conflicting with the
//  stdlib blanket impl
#[repr(transparent)]
struct KeyWrapper<K: ?Sized>(K);

impl<K: ?Sized> KeyWrapper<K> {
    fn from_ref(key: &K) -> &Self {
        // safety: KeyWrapper is transparent, so casting the ref like this is allowable
        unsafe { &*(key as *const K as *const KeyWrapper<K>) }
    }
}

impl<K: ?Sized + Hash> Hash for KeyWrapper<K> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.hash(state)
    }
}

impl<K: ?Sized + PartialEq> PartialEq for KeyWrapper<K> {
    fn eq(&self, other: &Self) -> bool {
        self.0.eq(&other.0)
    }
}

impl<K: ?Sized + Eq> Eq for KeyWrapper<K> {}

// Struct used to hold a key value pair. Also contains references to previous and next entries
// so we can maintain the entries in a linked list ordered by their use.
struct LruEntry<K, V> {
    key: mem::MaybeUninit<K>,
    val: mem::MaybeUninit<V>,
    prev: *mut LruEntry<K, V>,
    next: *mut LruEntry<K, V>,
}

impl<K, V> LruEntry<K, V> {
    fn new(key: K, val: V) -> Self {
        LruEntry {
            key: mem::MaybeUninit::new(key),
            val: mem::MaybeUninit::new(val),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }

    fn new_sigil() -> Self {
        LruEntry {
            key: mem::MaybeUninit::uninit(),
            val: mem::MaybeUninit::uninit(),
            prev: ptr::null_mut(),
            next: ptr::null_mut(),
        }
    }
}

#[cfg(feature = "hashbrown")]
pub type DefaultHasher = hashbrown::hash_map::DefaultHashBuilder;
#[cfg(not(feature = "hashbrown"))]
pub type DefaultHasher = std::collections::hash_map::RandomState;

// Struct used to wrap entries to compare/hash by key
struct EntryWrapper<K, V>(NonNull<LruEntry<K, V>>);

impl<K, V> EntryWrapper<K, V> {
    fn key(&self) -> &K {
        unsafe { self.0.as_ref().key.assume_init_ref() }
    }
}

impl<K: Hash, V> Hash for EntryWrapper<K, V> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.key().hash(state)
    }
}

impl<K: PartialEq, V> PartialEq for EntryWrapper<K, V> {
    fn eq(&self, other: &EntryWrapper<K, V>) -> bool {
        self.key().eq(other.key())
    }
}

impl<K: Eq, V> Eq for EntryWrapper<K, V> {}

impl<K, V, Q> Borrow<KeyWrapper<Q>> for EntryWrapper<K, V>
where
    K: Borrow<Q>,
    Q: ?Sized,
{
    fn borrow(&self) -> &KeyWrapper<Q> {
        KeyWrapper::from_ref(self.key().borrow())
    }
}

/// Specifies how new entries should be added to `LruCache`s. Used as a return by `Limiter::on_add`.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum AddBehavior {
    /// Accepts the new entry by adding it to the list without evicting any existing entries.
    Accept,
    /// Accepts the new entry by evicting the LRU entry. If the new element is about to make the
    /// cache oversized, this should be preferred over `Accept` because allows the cache
    /// implementation to reuse memory from the LRU entry.
    /// Note: `Evict` is only relevant for `Limiter::on_add` calls. For `Limiter::on_update` calls,
    /// it behaves exactly like `Accept`.
    Evict,
    /// Rejects the new entry because it is too big for the cache's capacity.
    /// Note: `Limiter::on_remove` will not be called for the entry if it is rejected. Therefore, a
    /// rejected `Limiter::on_add` should leave the limiter's internal state unchanged.
    Reject,
}

/// A trait for implementing limiters, which constrain the maximum cache of an `LruCache`. Limiters
/// may limit the number of elements, the total memory usage of the elements, or other metrics.
///
/// # Example
///
/// ```
/// // This example implements a simple sum limiter
///
/// use std::cell::RefCell;
/// use std::sync::atomic::{AtomicUsize, Ordering};
/// use lru::{AddBehavior, Limiter, LruCache};
///
/// /// A limiter for the sum of the cache values
/// struct SumLimited {
///     limit: usize,
///     current: RefCell<usize>
/// }
///
/// impl<K, S> Limiter<K, usize, S> for SumLimited {
///     fn is_oversized(&self, cache: &LruCache<K, usize, impl Limiter<K, usize, S>, S>) -> bool {
///         *self.current.borrow() > self.limit
///     }
///
///     fn on_add(&self, cache: &LruCache<K, usize, impl Limiter<K, usize, S>, S>, key: &K, value: &usize) -> AddBehavior {
///         if *value > self.limit {
///             AddBehavior::Reject
///         } else {
///             let mut current = self.current.borrow_mut();
///             *current += value;
///             if *current > self.limit {
///                 AddBehavior::Evict
///             } else {
///                 AddBehavior::Accept
///             }
///         }
///     }
///
///     fn on_update(&self, cache: &LruCache<K, usize, impl Limiter<K, usize, S>, S>, old_key: &K, old_value: &usize, new_key: Option<&K>, new_value: Option<&usize>) -> AddBehavior {
///         let mut current = self.current.borrow_mut();
///         let mut next = *current;
///         if let Some(new_value) = new_value {
///             next -= old_value;
///             next += new_value;
///         }
///         if next > self.limit {
///             AddBehavior::Reject
///         } else {
///             *current = next;
///             AddBehavior::Accept
///         }
///     }
///
///     fn on_remove(&self, cache: &LruCache<K, usize, impl Limiter<K, usize, S>, S>, key: &K, value: &usize) {
///         *self.current.borrow_mut() -= value;
///     }
/// }
/// ```
pub trait Limiter<K, V, S> {
    /// Returns true if the cache is currently too big.
    fn is_oversized(&self, cache: &LruCache<K, V, impl Limiter<K, V, S>, S>) -> bool;

    /// Called when a new element is added to the cache to update any aggregations used by the
    /// limiter and to specify what to do with the element. See `AddBehavior` for the meanings of
    /// the different return values.
    fn on_add(
        &self,
        cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        key: &K,
        value: &V,
    ) -> AddBehavior;

    /// Called when an element is updated in the cache update any aggregations used by the
    /// limiter and to specify what to do with the element. See `AddBehavior` for the meanings of
    /// the different return values.
    ///
    /// Note that if the key xor the value is being updated, the other will be `None`.
    #[allow(unused_variables)]
    fn on_update(
        &self,
        cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        old_key: &K,
        old_value: &V,
        new_key: Option<&K>,
        new_value: Option<&V>,
    ) -> AddBehavior;

    /// Called when an element is removed from the cache to update any aggregations used by the
    /// limiter. Note that this function is only called if the element was successfully added in
    /// the first place (i.e. if the on_add call for the element returned `AddBehavior::Accept` or
    /// `AddBehavior::Evict`)
    #[allow(unused_variables)]
    fn on_remove(&self, cache: &LruCache<K, V, impl Limiter<K, V, S>, S>, key: &K, value: &V) {}
}

/// A `Limiter` which does not limit the max size of the cache.
#[derive(Debug, Copy, Clone)]
pub struct Unlimited;

impl<K, V, S> Limiter<K, V, S> for Unlimited {
    fn is_oversized(&self, _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>) -> bool {
        false
    }

    fn on_add(
        &self,
        _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        _key: &K,
        _value: &V,
    ) -> AddBehavior {
        AddBehavior::Accept
    }

    fn on_update(
        &self,
        _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        _old_key: &K,
        _old_value: &V,
        _new_key: Option<&K>,
        _new_value: Option<&V>,
    ) -> AddBehavior {
        AddBehavior::Accept
    }
}

/// A `Limiter` which limits the max len of the cache.
#[derive(Debug, Copy, Clone)]
pub struct SizeLimited(usize);

impl SizeLimited {
    /// Creates a new `SizeLimited` with the given limit
    pub fn new(limit: usize) -> Self {
        Self(limit)
    }

    /// Gets the current limit
    pub fn limit(&self) -> usize {
        self.0
    }

    /// Sets the size limit
    pub fn set_limit(&mut self, limit: usize) {
        self.0 = limit;
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> Limiter<K, V, S> for SizeLimited {
    fn is_oversized(&self, cache: &LruCache<K, V, impl Limiter<K, V, S>, S>) -> bool {
        cache.len() > self.0
    }

    fn on_add(
        &self,
        cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        _key: &K,
        _value: &V,
    ) -> AddBehavior {
        if self.0 == 0 {
            AddBehavior::Reject
        } else if cache.len() >= self.0 {
            AddBehavior::Evict
        } else {
            AddBehavior::Accept
        }
    }

    fn on_update(
        &self,
        _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        _old_key: &K,
        _old_value: &V,
        _new_key: Option<&K>,
        _new_value: Option<&V>,
    ) -> AddBehavior {
        // updates don't change length. Accept unconditionally
        AddBehavior::Accept
    }
}

/// A trait for implementing arbitrary cost functions for `CostLimited`. Note that you can create a
/// `CostFn` from a pair of closures, so it is often not necessary to implement this trait directly.
///
/// It is a logic error for the cost of a key or value to change while it is stored in an `LruCache`
/// (similar to changing the hash of a key while it is in a `HashMap`). The behavior resulting from
/// such a logic error is not specified, but will be encapsulated to the `LruCache` that observed
/// the logic error and not result in undefined behavior. This could include panics, incorrect
/// results, aborts, memory leaks, and non-termination.
///
/// # Example
///
/// ```
/// use lru::{CostLimited, LruCache};
/// let limiter = CostLimited::with_func(100, (
///     |key: &String| key.len(),
///     |value: &usize| *value
/// ));
/// let mut cache = LruCache::with_limiter(limiter);
/// cache.put("a".to_string(), 90);  // costs 91
/// cache.put("b".to_string(), 10);  // costs 11, evicts "a"
/// assert_eq!(cache.get("a"), None)
/// ```
pub trait CostFn<K, V> {
    /// Gets the cost of a key in the cache
    fn key_cost(&self, key: &K) -> usize;
    /// Gets the cost of a value in the cache
    fn value_cost(&self, value: &V) -> usize;
}

impl<K, V, F: Fn(&K) -> usize, G: Fn(&V) -> usize> CostFn<K, V> for (F, G) {
    fn key_cost(&self, key: &K) -> usize {
        (self.0)(key)
    }

    fn value_cost(&self, value: &V) -> usize {
        (self.1)(value)
    }
}

/// A `Limiter` which limits the max "cost" of the cache based on an arbitrary cost function
#[derive(Debug)]
pub struct CostLimited<F> {
    limit: usize,
    current: AtomicUsize,
    cost_func: F,
}

impl<F: Default> CostLimited<F> {
    /// Creates a new `CostLimited` with the given limit and the default value of the cost function
    pub fn new(limit: usize) -> Self {
        Self::with_func(limit, F::default())
    }
}

impl<F> CostLimited<F> {
    /// The maximum limit allowed by `CostLimited`
    pub const MAX_LIMIT: usize = usize::MAX / 2;

    /// Creates a new `CostLimited` with the given limit and cost function
    pub fn with_func(limit: usize, cost_func: F) -> Self {
        let mut this = Self {
            limit: 0,
            current: AtomicUsize::new(0),
            cost_func,
        };
        this.set_limit(limit);
        this
    }

    /// Gets the current limit
    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Sets the limit
    pub fn set_limit(&mut self, limit: usize) {
        if limit > Self::MAX_LIMIT {
            panic!("Limit ({}) cannot exceed {}", limit, Self::MAX_LIMIT);
        }
        self.limit = limit;
    }

    /// Gets the current total cost of the cache.
    pub fn current(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    /// Gets a ref to the cost function
    pub fn cost_func(&self) -> &F {
        &self.cost_func
    }

    /// Gets a mutable ref to the cost function
    pub fn cost_func_mut(&mut self) -> &mut F {
        &mut self.cost_func
    }

    /// Consumes the `CostLimited` and returns the inner cost function
    pub fn into_cost_func(self) -> F {
        self.cost_func
    }

    fn add_cost(current: usize, cost: usize) -> usize {
        current.checked_add(cost).expect(
            "Cost overflowed. This shouldn't be possible because of the MAX_LIMIT. This is a bug",
        )
    }

    fn sub_cost(current: usize, cost: usize) -> usize {
        current
            .checked_sub(cost)
            .expect("Key or value cost changed between insertion and removal")
    }

    fn update_cost(&self, mut func: impl FnMut(usize) -> usize) -> AddBehavior {
        let mut prev = self.current.load(Ordering::Acquire);
        let next = loop {
            let next = func(prev);
            let res =
                self.current
                    .compare_exchange_weak(prev, next, Ordering::AcqRel, Ordering::Acquire);
            match res {
                Ok(_) => break next,
                Err(new_prev) => prev = new_prev,
            }
        };
        if next > self.limit {
            AddBehavior::Evict
        } else {
            AddBehavior::Accept
        }
    }
}

impl<K, V, S, F: CostFn<K, V>> Limiter<K, V, S> for CostLimited<F> {
    fn is_oversized(&self, _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>) -> bool {
        self.current() > self.limit
    }

    fn on_add(
        &self,
        _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        key: &K,
        value: &V,
    ) -> AddBehavior {
        let cost = self.cost_func.key_cost(key) + self.cost_func.value_cost(value);
        if cost > self.limit {
            return AddBehavior::Reject;
        }
        self.update_cost(|current| Self::add_cost(current, cost))
    }

    fn on_update(
        &self,
        _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
        old_key: &K,
        old_value: &V,
        new_key: Option<&K>,
        new_value: Option<&V>,
    ) -> AddBehavior {
        let mut prev_cost = 0;
        let mut next_cost = 0;
        if let Some(new_key) = new_key {
            prev_cost += self.cost_func.key_cost(old_key);
            next_cost += self.cost_func.key_cost(new_key);
        }
        if let Some(new_value) = new_value {
            prev_cost += self.cost_func.value_cost(old_value);
            next_cost += self.cost_func.value_cost(new_value);
        }
        self.update_cost(|current| Self::add_cost(Self::sub_cost(current, prev_cost), next_cost))
    }

    fn on_remove(&self, _cache: &LruCache<K, V, impl Limiter<K, V, S>, S>, key: &K, value: &V) {
        let cost = self.cost_func.key_cost(key) + self.cost_func.value_cost(value);
        self.update_cost(|current| Self::sub_cost(current, cost));
    }
}

/// A trait for implementing "keys" into an LruCache entry. Used to customize how to get a ref for
/// lookup. Note that implementing this trait only allows entry lookup. To support insertion as
/// well, see `InsertionKey`.
//noinspection RsSelfConvention
pub trait Key {
    /// Type of the ref used for lookup.
    type Key: ?Sized + Hash + Eq;

    /// Gets this key as a ref.
    fn as_ref(this: &Self) -> &Self::Key;
}

/// A trait for implementing keys which support insertion (by conversion into the "real" key type).
//noinspection RsSelfConvention
pub trait InsertionKey<K>: Key {
    /// Converts this key into the "real" key type.
    fn into_owned(this: Self) -> K;
}

/// A wrapper for entry lookup via owned key. Allows efficient insertion without cloning.
#[derive(Hash, Eq, PartialEq)]
pub struct OwnedKey<K>(pub K);

impl<K: Hash + Eq> Key for OwnedKey<K> {
    type Key = K;

    fn as_ref(this: &Self) -> &Self::Key {
        &this.0
    }
}

impl<K: Hash + Eq> InsertionKey<K> for OwnedKey<K> {
    fn into_owned(this: Self) -> K {
        this.0
    }
}

impl<K: Debug> Debug for OwnedKey<K> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A wrapper for entry lookup via borrowed ref. Allows efficient lookup without cloning.
#[derive(Hash, Eq, PartialEq)]
pub struct BorrowedKey<'a, Q: ?Sized>(pub &'a Q);

impl<'a, Q: ?Sized + Hash + Eq> Key for BorrowedKey<'a, Q> {
    type Key = Q;

    fn as_ref(this: &Self) -> &Self::Key {
        this.0
    }
}

#[cfg(not(feature = "no_std"))]
impl<'a, K: Borrow<Q>, Q: ?Sized + Hash + Eq + ToOwned<Owned = K>> InsertionKey<K>
    for BorrowedKey<'a, Q>
{
    fn into_owned(this: Self) -> Q::Owned {
        this.0.to_owned()
    }
}

impl<'a, Q: ?Sized + Debug> Debug for BorrowedKey<'a, Q> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// Used to store either the OccupiedEntry's creation key or the evicted entry, since these two
//  cannot coexist
enum OccupiedExtra<K, V, Q> {
    Key(Option<Q>),
    Evicted(Option<(K, V)>),
}

/// A view into an occupied entry in an `LruCache`. It is part of the `Entry` enum.
pub struct OccupiedEntry<
    'a,
    K: Hash + Eq,
    V,
    Q = OwnedKey<K>,
    L: Limiter<K, V, S> = SizeLimited,
    S: BuildHasher = DefaultHasher,
> {
    cache: &'a mut LruCache<K, V, L, S>,
    node: NonNull<LruEntry<K, V>>,
    extra: OccupiedExtra<K, V, Q>,
}

impl<'a, K: Hash + Eq, V, Q, L: Limiter<K, V, S>, S: BuildHasher> OccupiedEntry<'a, K, V, Q, L, S> {
    /// Gets a reference to the key in the entry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry(1).or_insert("a");
    /// assert_eq!(cache.entry(1).key(), &1);
    /// ```
    pub fn key(&self) -> &K {
        unsafe { self.node.as_ref().key.assume_init_ref() }
    }

    fn key_mut(&mut self) -> &mut K {
        unsafe { self.node.as_mut().key.assume_init_mut() }
    }

    /// Gets a reference to the value in the entry. Unlike `get` does not update the LRU list so the
    /// key's position will be unchanged.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry(1).or_insert("a");
    /// if let Entry::Occupied(entry) = cache.entry(1) {
    ///     assert_eq!(entry.peek(), &"a");
    /// };
    /// ```
    pub fn peek(&self) -> &V {
        unsafe { self.node.as_ref().val.assume_init_ref() }
    }

    /// Gets a mutable reference to the value in the entry. Unlike `get_mut` does not update the LRU
    /// list so the key's position will be unchanged.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_insert(1);
    /// if let Entry::Occupied(mut entry) = cache.entry("a") {
    ///     assert_eq!(entry.peek(), &1);
    ///     *entry.peek_mut() *= 2;
    ///     assert_eq!(entry.peek(), &2);
    /// };
    /// ```
    pub fn peek_mut(&mut self) -> &mut V {
        unsafe { self.node.as_mut().val.assume_init_mut() }
    }

    /// Converts the `OccupiedEntry` into a mutable reference to the value in the entry with a
    /// lifetime bound to the map itself. Unlike `into_mut` does not update the LRU list so the
    /// key's position will be unchanged.
    ///
    /// If you need multiple references to the `OccupiedEntry`, see `peek_mut`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_insert(1);
    /// if let Entry::Occupied(mut entry) = cache.entry("a") {
    ///     *entry.into_peek() *= 2;
    /// }
    /// assert_eq!(cache.get(&"a"), Some(&2));
    /// ```
    pub fn into_peek(mut self) -> &'a mut V {
        unsafe { self.node.as_mut().val.assume_init_mut() }
    }

    /// Gets a reference to the value in the entry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry(1).or_insert("a");
    /// if let Entry::Occupied(mut entry) = cache.entry(1) {
    ///     assert_eq!(entry.get(), &"a");
    /// };
    /// ```
    pub fn get(&mut self) -> &V {
        self.promote();
        self.peek()
    }

    /// Gets a mutable reference to the value in the entry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_insert(1);
    /// if let Entry::Occupied(mut entry) = cache.entry("a") {
    ///     assert_eq!(entry.get(), &1);
    ///     *entry.get_mut() *= 2;
    ///     assert_eq!(entry.get(), &2);
    /// };
    /// ```
    pub fn get_mut(&mut self) -> &mut V {
        self.promote();
        self.peek_mut()
    }

    /// Converts the `OccupiedEntry` into a mutable reference to the value in the entry with a
    /// lifetime bound to the map itself.
    ///
    /// If you need multiple references to the `OccupiedEntry`, see `get_mut`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_insert(1);
    /// if let Entry::Occupied(mut entry) = cache.entry("a") {
    ///     *entry.into_mut() *= 2;
    /// }
    /// assert_eq!(cache.get(&"a"), Some(&2));
    /// ```
    pub fn into_mut(mut self) -> &'a mut V {
        self.promote();
        self.into_peek()
    }

    /// Marks this entry's key as the most recently used one.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    /// cache.get(&1);
    /// cache.get(&2);
    ///
    /// // If we do `pop_lru` now, we would pop 3.
    /// // assert_eq!(cache.pop_lru(), Some((3, "c")));
    ///
    /// // By promoting 3, we make sure it isn't popped.
    /// if let Entry::Occupied(mut entry) = cache.entry(3) {
    ///     entry.promote();
    /// }
    /// assert_eq!(cache.pop_lru(), Some((1, "a")));
    /// ```
    pub fn promote(&mut self) {
        self.cache.detach(self.node.as_ptr());
        self.cache.attach(self.node.as_ptr());
    }

    /// Marks this entry's key as the least recently used one.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    /// cache.get(&1);
    /// cache.get(&2);
    ///
    /// // If we do `pop_lru` now, we would pop 3.
    /// // assert_eq!(cache.pop_lru(), Some((3, "c")));
    ///
    /// // By demoting 1 and 2, we make sure those are popped first.
    /// if let Entry::Occupied(mut entry) = cache.entry(2) {
    ///     entry.demote();
    /// }
    /// if let Entry::Occupied(mut entry) = cache.entry(1) {
    ///     entry.demote();
    /// }
    /// assert_eq!(cache.pop_lru(), Some((1, "a")));
    /// assert_eq!(cache.pop_lru(), Some((2, "b")));
    /// ```
    pub fn demote(&mut self) {
        self.cache.detach(self.node.as_ptr());
        self.cache.attach_last(self.node.as_ptr());
    }

    fn replace_node(mut self, node: NonNull<LruEntry<K, V>>) -> Result<Self, Self> {
        let root = unsafe { self.cache.root.unwrap_unchecked() };
        if node == root {
            Err(self)
        } else {
            self.node = node;
            // invalidate any key/evictions
            self.extra = OccupiedExtra::Key(None);
            Ok(self)
        }
    }

    /// Gets the next (less recently used) entry in the cache.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    ///
    /// if let Entry::Occupied(entry) = cache.entry(2) {
    ///     let entry = entry.next().unwrap();
    ///     assert_eq!(entry.key(), &1);
    /// };
    /// ```
    pub fn next(self) -> Result<Self, Self> {
        let node = unsafe { NonNull::new_unchecked(self.node.as_ref().next) };
        self.replace_node(node)
    }

    /// Gets the previous (more recently used) entry in the cache.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    ///
    /// if let Entry::Occupied(entry) = cache.entry(2) {
    ///     let entry = entry.prev().unwrap();
    ///     assert_eq!(entry.key(), &3);
    /// };
    /// ```
    pub fn prev(self) -> Result<Self, Self> {
        let node = unsafe { NonNull::new_unchecked(self.node.as_ref().prev) };
        self.replace_node(node)
    }

    /// Sets the value of the entry, and returns the entry’s old value.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    ///
    /// if let Entry::Occupied(mut entry) = cache.entry(1) {
    ///     assert_eq!(entry.insert("b"), "a");
    ///     assert_eq!(entry.get(), &"b");
    /// };
    /// ```
    pub fn insert(&mut self, value: V) -> V {
        self.try_insert(value)
            .unwrap_or_else(|_| panic!("Cache does not have sufficient capacity"))
    }

    /// Trys to set the value of the entry, and returns the entry’s old value. If the new entry is
    /// rejected by the limiter, returns the rejected value as an `Result::Err`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    ///
    /// if let Entry::Occupied(mut entry) = cache.entry(1) {
    ///     assert_eq!(entry.try_insert("b"), Ok("a"));
    ///     assert_eq!(entry.get(), &"b");
    /// };
    /// ```
    pub fn try_insert(&mut self, value: V) -> Result<V, V> {
        let behavior =
            self.cache
                .limiter
                .on_update(self.cache, self.key(), self.peek(), None, Some(&value));
        if behavior == AddBehavior::Reject {
            return Err(value);
        }
        Ok(replace(self.get_mut(), value))
    }

    /// Takes the value out of the entry, and returns it.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    ///
    /// if let Entry::Occupied(mut entry) = cache.entry(1) {
    ///     assert_eq!(entry.remove(), "a");
    /// }
    /// assert!(!cache.contains(&1));
    /// ```
    pub fn remove(self) -> V {
        self.remove_entry().1
    }

    fn remove_node(mut self) -> NonNull<LruEntry<K, V>> {
        let key = unsafe { self.node.as_ref().key.assume_init_ref() };
        // note: we can't use self.key() here because the compiler doesn't know that it doesn't
        //  overlap with self.cache
        let removed = self.cache.map.remove(KeyWrapper::from_ref(key));
        debug_assert!(removed);
        self.cache.detach(self.node.as_ptr());
        self.cache
            .limiter
            .on_remove(self.cache, self.key(), self.peek());
        // prevent automatic evictions by setting the extra to Key
        self.extra = OccupiedExtra::Key(None);
        self.node
    }

    /// Takes the key and value out of the entry, and returns them.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    ///
    /// if let Entry::Occupied(mut entry) = cache.entry(1) {
    ///     assert_eq!(entry.remove_entry(), (1, "a"));
    /// }
    /// assert!(!cache.contains(&1));
    /// ```
    pub fn remove_entry(self) -> (K, V) {
        let node = self.remove_node();
        let LruEntry { key, val, .. } = unsafe { *Box::from_raw(node.as_ptr()) };
        let key = unsafe { key.assume_init() };
        let value = unsafe { val.assume_init() };
        (key, value)
    }

    /// Takes the entry evicted by this entry's insertion, if any. A return value of `None` means
    /// that this entry was not created by insertion, did not evict another entry, or was already
    /// taken.
    ///
    /// Any evicted entries which remain untaken when the entry is dropped will be dropped.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    ///
    /// let mut entry = cache.entry(3).insert("c");
    /// assert_eq!(entry.take_evicted(), Some((1, "a")));
    /// assert_eq!(entry.take_evicted(), None);
    /// ```
    pub fn take_evicted(&mut self) -> Option<(K, V)> {
        match &mut self.extra {
            OccupiedExtra::Key(_) => return None,
            OccupiedExtra::Evicted(evicted) => {
                if let Some(evicted) = evicted.take() {
                    return Some(evicted);
                }
            }
        }
        #[allow(clippy::never_loop)]
        'fuse: loop {
            if self.cache.limiter.is_oversized(self.cache) {
                let mut other = match self.cache.entry_lru() {
                    // limiter is reporting oversized on an empty cache, bail out
                    None => break 'fuse,
                    Some(other) => other,
                };
                if other.node == self.node {
                    // tried to evict ourself! never allow that, just move to next entry
                    other = match other.next() {
                        Ok(other) => other,
                        // no other entries left, just bail out
                        Err(_) => break 'fuse,
                    }
                }
                return Some(other.remove_entry());
            }
            break 'fuse;
        }
        // switch to the key extra so we behave like a fused iterator
        self.extra = OccupiedExtra::Key(None);
        None
    }
}

impl<'a, K: Hash + Eq, V, Q: InsertionKey<K>, L: Limiter<K, V, S>, S: BuildHasher>
    OccupiedEntry<'a, K, V, Q, L, S>
{
    /// Replaces the key in the hash map with the key used to create this entry. Panics if the
    /// key was already consumed by insertion.
    ///
    /// # Example
    ///
    /// ```
    /// use std::rc::Rc;
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// let str1 = Rc::new("abc".to_string());
    /// let str2 = Rc::new("abc".to_string());
    ///
    /// cache.put(str1.clone(), 1);
    ///
    /// assert_eq!(Rc::strong_count(&str1), 2);
    /// assert_eq!(Rc::strong_count(&str2), 1);
    /// if let Entry::Occupied(mut entry) = cache.entry(str2.clone()) {
    ///     entry.replace_key();
    ///     assert_eq!(Rc::strong_count(&str1), 1);
    ///     assert_eq!(Rc::strong_count(&str2), 2);
    /// };
    /// ```
    pub fn replace_key(self) -> K {
        self.try_replace_key()
            .unwrap_or_else(|_| panic!("Cache does not have sufficient capacity"))
    }

    fn get_key_for_replace(&mut self) -> K {
        let key = match &mut self.extra {
            OccupiedExtra::Key(key) => key.take(),
            OccupiedExtra::Evicted(_) => None,
        };
        let key = key.expect("Key was already consumed by insertion");
        Q::into_owned(key)
    }

    /// Tries to replace the key in the cache with the key used to create this entry. Panics if the
    /// key was already consumed by insertion. If the limiter rejects the update, returns the
    /// rejected key.
    ///
    /// # Example
    ///
    /// ```
    /// use std::rc::Rc;
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// let str1 = Rc::new("abc".to_string());
    /// let str2 = Rc::new("abc".to_string());
    ///
    /// cache.put(str1.clone(), 1);
    ///
    /// assert_eq!(Rc::strong_count(&str1), 2);
    /// assert_eq!(Rc::strong_count(&str2), 1);
    /// if let Entry::Occupied(mut entry) = cache.entry(str2.clone()) {
    ///     entry.try_replace_key().unwrap();
    ///     assert_eq!(Rc::strong_count(&str1), 1);
    ///     assert_eq!(Rc::strong_count(&str2), 2);
    /// };
    /// ```
    pub fn try_replace_key(mut self) -> Result<K, K> {
        let key = self.get_key_for_replace();
        let behavior =
            self.cache
                .limiter
                .on_update(self.cache, self.key(), self.peek(), Some(&key), None);
        if behavior == AddBehavior::Reject {
            return Err(key);
        }
        Ok(replace(self.key_mut(), key))
    }

    /// Replaces the entry, returning the old key and value. The new key in the hash map will be
    /// the key used to create this entry. Panics if the key was already consumed by insertion.
    ///
    /// # Example
    ///
    /// ```
    /// use std::rc::Rc;
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// let str1 = Rc::new("abc".to_string());
    /// let str2 = Rc::new("abc".to_string());
    ///
    /// cache.put(str1.clone(), 1);
    ///
    /// assert_eq!(Rc::strong_count(&str1), 2);
    /// assert_eq!(Rc::strong_count(&str2), 1);
    /// if let Entry::Occupied(mut entry) = cache.entry(str2.clone()) {
    ///     entry.replace_entry(5);
    ///     assert_eq!(Rc::strong_count(&str1), 1);
    ///     assert_eq!(Rc::strong_count(&str2), 2);
    /// }
    /// assert_eq!(cache.get(&str1), Some(&5));
    /// ```
    pub fn replace_entry(self, value: V) -> (K, V) {
        self.try_replace_entry(value)
            .unwrap_or_else(|_| panic!("Cache does not have sufficient capacity"))
    }

    /// Tries to replace the entry, returning the old key and value. The new key in the hash map
    /// will be the key used to create this entry. Panics if the key was already consumed by
    /// insertion. If the limiter rejects the update, returns the rejected entry.
    ///
    /// # Example
    ///
    /// ```
    /// use std::rc::Rc;
    /// use lru::{Entry, LruCache};
    /// let mut cache = LruCache::new(3);
    ///
    /// let str1 = Rc::new("abc".to_string());
    /// let str2 = Rc::new("abc".to_string());
    ///
    /// cache.put(str1.clone(), 1);
    ///
    /// assert_eq!(Rc::strong_count(&str1), 2);
    /// assert_eq!(Rc::strong_count(&str2), 1);
    /// if let Entry::Occupied(mut entry) = cache.entry(str2.clone()) {
    ///     entry.try_replace_entry(5).unwrap();
    ///     assert_eq!(Rc::strong_count(&str1), 1);
    ///     assert_eq!(Rc::strong_count(&str2), 2);
    /// }
    /// assert_eq!(cache.get(&str1), Some(&5));
    /// ```
    pub fn try_replace_entry(mut self, value: V) -> Result<(K, V), (K, V)> {
        let key = self.get_key_for_replace();
        let behavior = self.cache.limiter.on_update(
            self.cache,
            self.key(),
            self.peek(),
            Some(&key),
            Some(&value),
        );
        if behavior == AddBehavior::Reject {
            return Err((key, value));
        }
        let key = replace(self.key_mut(), key);
        let value = replace(self.get_mut(), value);
        Ok((key, value))
    }
}

impl<'a, K: Hash + Eq, V, Q, L: Limiter<K, V, S>, S: BuildHasher> Drop
    for OccupiedEntry<'a, K, V, Q, L, S>
{
    fn drop(&mut self) {
        while self.take_evicted().is_some() {}
    }
}

impl<'a, K: Hash + Eq + Debug, V: Debug, Q, L: Limiter<K, V, S>, S: BuildHasher> Debug
    for OccupiedEntry<'a, K, V, Q, L, S>
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("OccupiedEntry")
            .field("key", self.key())
            .field("value", self.peek())
            .finish()
    }
}

/// A view into a vacant entry in an `LruCache`. It is part of the `Entry` enum.
pub struct VacantEntry<'a, K, V, Q = OwnedKey<K>, L = SizeLimited, S = DefaultHasher> {
    cache: &'a mut LruCache<K, V, L, S>,
    key: Q,
}

impl<'a, K, V, Q: Key, L, S> VacantEntry<'a, K, V, Q, L, S> {
    /// Gets a reference to the key that would be used when inserting a value through the
    /// VacantEntry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::<u8, u8>::new(2);
    ///
    /// assert_eq!(cache.entry(1).key(), &1);
    /// ```
    pub fn key(&self) -> &Q::Key {
        Q::as_ref(&self.key)
    }

    /// Take ownership of the key.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, Entry, OwnedKey};
    /// let mut cache = LruCache::<u8, u8>::new(2);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(1) {
    ///     assert_eq!(entry.into_key(), OwnedKey(1));
    /// };
    /// ```
    pub fn into_key(self) -> Q {
        self.key
    }
}

impl<'a, K: Hash + Eq, V, Q: InsertionKey<K>, L: Limiter<K, V, S>, S: BuildHasher>
    VacantEntry<'a, K, V, Q, L, S>
{
    /// Sets the value of the entry with the `VacantEntry`’s key, and returns a mutable reference to
    /// it.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, Entry};
    /// let mut cache = LruCache::new(2);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(1) {
    ///     entry.insert("a");
    /// }
    /// assert_eq!(cache.get(&1), Some(&"a"));
    /// ```
    pub fn insert(self, value: V) -> &'a mut V {
        self.insert_entry(value).into_mut()
    }

    /// Sets the value of the entry with the `VacantEntry`’s key, and returns an `OccupiedEntry`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, Entry};
    /// let mut cache = LruCache::new(2);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(1) {
    ///     entry.insert_entry("a");
    /// }
    /// assert_eq!(cache.get(&1), Some(&"a"));
    /// ```
    pub fn insert_entry(self, value: V) -> OccupiedEntry<'a, K, V, Q, L, S> {
        self.try_insert_entry(value)
            .unwrap_or_else(|_| panic!("Cache does not have sufficient capacity"))
    }

    /// Trys to set the value of the entry with the `VacantEntry`’s key, and returns a mutable
    /// reference to it. If insertion fails because the cache has zero capacity, returns the entry
    /// which could not be inserted as an Err.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, Entry};
    /// let mut cache = LruCache::new(2);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(1) {
    ///     let res = entry.try_insert("a");
    ///     assert!(res.is_ok());
    /// }
    /// assert_eq!(cache.get(&1), Some(&"a"));
    ///
    /// cache.resize(0);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(2) {
    ///     let res = entry.try_insert("b");
    ///     assert_eq!(res, Err((2, "b")));
    /// };
    /// ```
    pub fn try_insert(self, value: V) -> Result<&'a mut V, (K, V)> {
        Ok(self.try_insert_entry(value)?.into_mut())
    }

    /// Trys to set the value of the entry with the `VacantEntry`’s key, and returns an
    /// `OccupiedEntry`. If insertion fails because the cache has zero capacity, returns the entry
    /// which could not be inserted as an Err.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, Entry};
    /// let mut cache = LruCache::new(2);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(3) {
    ///     let res = entry.try_insert_entry("c");
    ///     assert!(res.is_ok());
    /// }
    /// assert_eq!(cache.get(&3), Some(&"c"));
    ///
    /// cache.resize(0);
    ///
    /// if let Entry::Vacant(entry) = cache.entry(4) {
    ///     let res = entry.try_insert_entry("d");
    ///     assert_eq!(res.unwrap_err(), (4, "d"));
    /// };
    /// ```
    #[allow(clippy::type_complexity)]
    pub fn try_insert_entry(self, value: V) -> Result<OccupiedEntry<'a, K, V, Q, L, S>, (K, V)> {
        let key = Q::into_owned(self.key);
        let (node, evicted) = {
            let behavior = self.cache.limiter.on_add(self.cache, &key, &value);
            match behavior {
                AddBehavior::Reject => return Err((key, value)),
                AddBehavior::Evict if !self.cache.is_empty() => {
                    // if the cache is full, remove the last entry so we can use it for the new key
                    let entry = unsafe { self.cache.entry_lru().unwrap_unchecked() };
                    let mut node = entry.remove_node();
                    let key = replace(unsafe { node.as_mut().key.assume_init_mut() }, key);
                    let value = replace(unsafe { node.as_mut().val.assume_init_mut() }, value);
                    let evicted = Some((key, value));
                    (node, evicted)
                }
                _ => {
                    let node = unsafe {
                        NonNull::new_unchecked(Box::into_raw(Box::new(LruEntry::new(key, value))))
                    };
                    if self.cache.is_empty() {
                        self.cache.alloc_root();
                    }
                    (node, None)
                }
            }
        };
        self.cache.attach(node.as_ptr());
        self.cache.map.insert(EntryWrapper(node));
        Ok(OccupiedEntry {
            cache: self.cache,
            node,
            extra: OccupiedExtra::Evicted(evicted),
        })
    }
}

impl<'a, K, V, Q: Key, L, S> Debug for VacantEntry<'a, K, V, Q, L, S>
where
    Q::Key: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_struct("VacantEntry")
            .field("key", &self.key())
            .finish()
    }
}

/// A view into a single entry in a map, which may either be vacant or occupied.
///
/// This `enum` is constructed from the `LruCache::entry`/`LruCache::entry_ref` methods on
/// `LruCache`.
pub enum Entry<
    'a,
    K: Hash + Eq,
    V,
    Q = OwnedKey<K>,
    L: Limiter<K, V, S> = SizeLimited,
    S: BuildHasher = DefaultHasher,
> {
    /// An occupied entry.
    Occupied(OccupiedEntry<'a, K, V, Q, L, S>),
    /// A vacant entry.
    Vacant(VacantEntry<'a, K, V, Q, L, S>),
}

impl<'a, K: Hash + Eq + Borrow<Q::Key>, V, Q: Key, L: Limiter<K, V, S>, S: BuildHasher>
    Entry<'a, K, V, Q, L, S>
{
    /// Returns a reference to this entry's key.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::<u8, u8>::new(2);
    ///
    /// assert_eq!(cache.entry(1).key(), &1);
    /// ```
    pub fn key(&self) -> &Q::Key {
        match self {
            Entry::Occupied(entry) => entry.key().borrow(),
            Entry::Vacant(entry) => entry.key(),
        }
    }
}

impl<'a, K: Hash + Eq, V, Q: InsertionKey<K>, L: Limiter<K, V, S>, S: BuildHasher>
    Entry<'a, K, V, Q, L, S>
{
    /// Sets the value of the entry, and returns an `OccupiedEntry`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// let entry = cache.entry(1).insert("a");
    /// assert_eq!(entry.key(), &1);
    /// entry.remove();
    /// assert!(cache.is_empty());
    /// ```
    pub fn insert(self, value: V) -> OccupiedEntry<'a, K, V, Q, L, S> {
        self.try_insert(value)
            .unwrap_or_else(|_| panic!("Cache does not have sufficient capacity"))
    }

    /// Tries to sets the value of the entry, and returns an `OccupiedEntry`. If the new entry/value
    /// is rejected by the limiter, returns the rejected entry as an `Result::Err`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// let entry = cache.entry(1).try_insert("a").unwrap();
    /// assert_eq!(entry.key(), &1);
    /// entry.remove();
    /// assert!(cache.is_empty());
    /// ```
    #[allow(clippy::type_complexity)]
    pub fn try_insert(self, value: V) -> Result<OccupiedEntry<'a, K, V, Q, L, S>, (Option<K>, V)> {
        match self {
            Entry::Occupied(mut entry) => {
                entry.try_insert(value).map_err(|v| (None, v))?;
                Ok(entry)
            }
            Entry::Vacant(entry) => entry.try_insert_entry(value).map_err(|(k, v)| (Some(k), v)),
        }
    }

    /// Ensures a value is in the entry by inserting the default if empty, and returns a mutable
    /// reference to the value in the entry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_insert(1);
    /// assert_eq!(cache.get(&"a"), Some(&1));
    ///
    /// *cache.entry("a").or_insert(10) *= 2;
    /// assert_eq!(cache.get(&"a"), Some(&2));
    /// ```
    pub fn or_insert(self, default: V) -> &'a mut V {
        self.or_insert_with(move || default)
    }

    /// Ensures a value is in the entry by inserting the result of the default function if empty,
    /// and returns a mutable reference to the value in the entry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_insert_with(|| 1);
    /// assert_eq!(cache.get(&"a"), Some(&1));
    /// ```
    pub fn or_insert_with(self, default: impl FnOnce() -> V) -> &'a mut V {
        self.or_insert_with_key(move |_| default())
    }

    /// Ensures a value is in the entry by inserting, if empty, the result of the default function.
    /// This method allows for generating key-derived values for insertion by providing the default
    /// function a reference to the key that was moved during the .entry(key) method call.
    ///
    /// The reference to the moved/to_owned key is provided so that cloning or copying the key is
    /// unnecessary, unlike with `Entry::or_insert_with`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("abc").or_insert_with_key(|key| key.len());
    /// assert_eq!(cache.get(&"abc"), Some(&3));
    /// ```
    pub fn or_insert_with_key(self, default: impl FnOnce(&K) -> V) -> &'a mut V {
        match self {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let entry = VacantEntry {
                    cache: entry.cache,
                    key: OwnedKey(Q::into_owned(entry.key)),
                };
                let value = default(entry.key());
                entry.insert(value)
            }
        }
    }

    /// Provides in-place mutable access to an occupied entry before any potential inserts into the
    /// map.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a")
    ///     .and_modify(|x| *x += 1)
    ///     .or_insert(1);
    /// assert_eq!(cache.get(&"a"), Some(&1));
    ///
    /// cache.entry("a")
    ///     .and_modify(|x| *x += 1)
    ///     .or_insert(1);
    /// assert_eq!(cache.get(&"a"), Some(&2));
    /// ```
    pub fn and_modify(mut self, f: impl FnOnce(&mut V)) -> Self {
        if let Entry::Occupied(entry) = &mut self {
            f(entry.get_mut());
        }
        self
    }
}

impl<'a, K: Hash + Eq, V: Default, Q: InsertionKey<K>, L: Limiter<K, V, S>, S: BuildHasher>
    Entry<'a, K, V, Q, L, S>
{
    /// Ensures a value is in the entry by inserting the default value if empty, and returns a
    /// mutable reference to the value in the entry.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry("a").or_default();
    /// assert_eq!(cache.get(&"a"), Some(&0));
    /// ```
    pub fn or_default(self) -> &'a mut V {
        self.or_insert_with(V::default)
    }
}

impl<'a, K: Hash + Eq + Debug, V: Debug, Q: Key, L: Limiter<K, V, S>, S: BuildHasher> Debug
    for Entry<'a, K, V, Q, L, S>
where
    Q::Key: Debug,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Entry::Occupied(entry) => f.debug_tuple("Entry").field(entry).finish(),
            Entry::Vacant(entry) => f.debug_tuple("Entry").field(entry).finish(),
        }
    }
}

/// An LRU Cache
pub struct LruCache<K, V, L = SizeLimited, S = DefaultHasher> {
    map: HashSet<EntryWrapper<K, V>, S>,
    limiter: L,

    // root is a sigil node to facilitate inserting entries
    root: Option<NonNull<LruEntry<K, V>>>,
}

impl<K: Hash + Eq, V> LruCache<K, V, SizeLimited> {
    /// Creates a new LRU Cache that holds at most `cap` items.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache: LruCache<isize, &str> = LruCache::new(10);
    /// ```
    pub fn new(cap: usize) -> LruCache<K, V> {
        LruCache::construct(SizeLimited::new(cap), HashSet::with_capacity(cap))
    }
}

impl<K: Hash + Eq, V> LruCache<K, V, Unlimited> {
    /// Creates a new LRU Cache that never automatically evicts items.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, Unlimited};
    /// let mut cache: LruCache<isize, &str, Unlimited> = LruCache::unbounded();
    /// ```
    pub fn unbounded() -> LruCache<K, V, Unlimited> {
        LruCache::construct(Unlimited, HashSet::default())
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> LruCache<K, V, SizeLimited, S> {
    /// Creates a new LRU Cache that holds at most `cap` items and
    /// uses the provided hash builder to hash keys.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, DefaultHasher};
    ///
    /// let s = DefaultHasher::default();
    /// let mut cache: LruCache<isize, &str> = LruCache::with_hasher(10, s);
    /// ```
    pub fn with_hasher(cap: usize, hash_builder: S) -> LruCache<K, V, SizeLimited, S> {
        LruCache::construct(
            SizeLimited::new(cap),
            HashSet::with_capacity_and_hasher(cap, hash_builder),
        )
    }

    /// Returns the maximum number of key-value pairs the cache can hold.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// use std::num::NonZeroUsize;
    /// let mut cache: LruCache<isize, &str> = LruCache::new(2);
    /// assert_eq!(cache.cap(), 2);
    /// ```
    pub fn cap(&self) -> usize {
        self.limiter.limit()
    }

    /// Resizes the cache. If the new capacity is smaller than the size of the current
    /// cache any entries past the new capacity are discarded.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// use std::num::NonZeroUsize;
    /// let mut cache: LruCache<isize, &str> = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.resize(4);
    /// cache.put(3, "c");
    /// cache.put(4, "d");
    ///
    /// assert_eq!(cache.len(), 4);
    /// assert_eq!(cache.get(&1), Some(&"a"));
    /// assert_eq!(cache.get(&2), Some(&"b"));
    /// assert_eq!(cache.get(&3), Some(&"c"));
    /// assert_eq!(cache.get(&4), Some(&"d"));
    /// ```
    pub fn resize(&mut self, cap: usize) {
        // return early if capacity doesn't change
        if cap == self.limiter.limit() {
            return;
        }
        self.limiter_mut().set_limit(cap);
        self.shrink_to_fit();
    }
}

impl<K: Hash + Eq, V, S: BuildHasher> LruCache<K, V, Unlimited, S> {
    /// Creates a new LRU Cache that never automatically evicts items and
    /// uses the provided hash builder to hash keys.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, DefaultHasher, Unlimited};
    ///
    /// let s = DefaultHasher::default();
    /// let mut cache: LruCache<isize, &str, Unlimited> = LruCache::unbounded_with_hasher(s);
    /// ```
    pub fn unbounded_with_hasher(hash_builder: S) -> LruCache<K, V, Unlimited, S> {
        LruCache::construct(Unlimited, HashSet::with_hasher(hash_builder))
    }
}

impl<K: Hash + Eq, V, L: Limiter<K, V, DefaultHasher>> LruCache<K, V, L> {
    /// Creates a new LRU Cache with the given limiter.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, SizeLimited};
    /// let mut cache = LruCache::<usize, usize>::with_limiter(SizeLimited::new(10));
    /// ```
    pub fn with_limiter(limiter: L) -> LruCache<K, V, L> {
        LruCache::construct(limiter, HashSet::default())
    }
}

impl<K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher> LruCache<K, V, L, S> {
    /// Creates a new LRU Cache with the given limiter and uses the provided hash builder to hash
    /// keys.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::{LruCache, DefaultHasher, SizeLimited};
    ///
    /// let s = DefaultHasher::default();
    /// let mut cache = LruCache::<usize, usize>::with_limiter_and_hasher(SizeLimited::new(10), s);
    /// ```
    pub fn with_limiter_and_hasher(limiter: L, hash_builder: S) -> LruCache<K, V, L, S> {
        LruCache::construct(limiter, HashSet::with_hasher(hash_builder))
    }

    /// Creates a new LRU Cache with the given capacity.
    fn construct(limiter: L, map: HashSet<EntryWrapper<K, V>, S>) -> LruCache<K, V, L, S> {
        LruCache {
            map,
            limiter,
            root: None,
        }
    }

    /// Gets the given key’s corresponding entry in the map for in-place manipulation.
    ///
    /// # Example
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry(1).or_insert("a");
    /// cache.entry(2).or_default();
    ///
    /// assert_eq!(cache.get(&1), Some(&"a"));
    /// assert_eq!(cache.get(&2), Some(&""))
    /// ```
    pub fn entry(&mut self, k: K) -> Entry<K, V, OwnedKey<K>, L, S> {
        self.entry_for(OwnedKey(k))
    }

    /// Gets the given key’s corresponding entry by reference in the map for in-place manipulation.
    ///
    /// # Example
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry_ref(&1).or_insert("a");
    /// cache.entry_ref(&2).or_default();
    ///
    /// assert_eq!(cache.get(&1), Some(&"a"));
    /// assert_eq!(cache.get(&2), Some(&""))
    /// ```
    pub fn entry_ref<'a, 'b, Q: ?Sized + Hash + Eq>(
        &'a mut self,
        k: &'b Q,
    ) -> Entry<'a, K, V, BorrowedKey<'b, Q>, L, S>
    where
        K: Borrow<Q>,
    {
        self.entry_for(BorrowedKey(k))
    }

    /// Gets the entry for the LRU in the map for in-place manipulation.
    ///
    /// # Example
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.entry_ref(&1).or_insert("a");
    /// cache.entry_ref(&2).or_default();
    ///
    /// assert_eq!(cache.entry_lru().unwrap().key(), &1);
    /// // note: entry_lru doesn't promote by itself. Promotion only happens if you access
    /// //    the entry's value without using one of the peek methods
    /// assert_eq!(cache.entry_lru().unwrap().get(), &"a");
    /// assert_eq!(cache.entry_lru().unwrap().get(), &"");
    /// ```
    pub fn entry_lru(&mut self) -> Option<OccupiedEntry<K, V, BorrowedKey<K>, L, S>> {
        if self.is_empty() {
            return None;
        }
        let node = unsafe { NonNull::new_unchecked(self.root.unwrap_unchecked().as_ref().prev) };
        Some(OccupiedEntry {
            cache: self,
            node,
            extra: OccupiedExtra::Key(None),
        })
    }

    pub fn entry_for<Q>(&mut self, k: Q) -> Entry<K, V, Q, L, S>
    where
        Q: Key,
        K: Borrow<Q::Key>,
    {
        match self
            .map
            .get(KeyWrapper::from_ref(Q::as_ref(&k)))
            .map(|x| x.0)
        {
            None => Entry::Vacant(VacantEntry {
                cache: self,
                key: k,
            }),
            Some(node) => Entry::Occupied(OccupiedEntry {
                cache: self,
                node,
                extra: OccupiedExtra::Key(Some(k)),
            }),
        }
    }

    /// Puts a key-value pair into cache. If the key already exists in the cache, then it updates
    /// the key's value and returns the old value. Otherwise, `None` is returned.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// assert_eq!(None, cache.put(1, "a"));
    /// assert_eq!(None, cache.put(2, "b"));
    /// assert_eq!(Some("b"), cache.put(2, "beta"));
    ///
    /// assert_eq!(cache.get(&1), Some(&"a"));
    /// assert_eq!(cache.get(&2), Some(&"beta"));
    /// ```
    pub fn put(&mut self, k: K, v: V) -> Option<V> {
        Some(match self.entry(k) {
            Entry::Occupied(mut entry) => entry.insert(v),
            Entry::Vacant(entry) => entry.try_insert(v).err()?.1,
        })
    }

    /// Pushes a key-value pair into the cache. If an entry with key `k` already exists in
    /// the cache or another cache entry is removed (due to the lru's capacity),
    /// then it returns the old entry's key-value pair. Otherwise, returns `None`.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// assert_eq!(None, cache.push(1, "a"));
    /// assert_eq!(None, cache.push(2, "b"));
    ///
    /// // This push call returns (2, "b") because that was previously 2's entry in the cache.
    /// assert_eq!(Some((2, "b")), cache.push(2, "beta"));
    ///
    /// // This push call returns (1, "a") because the cache is at capacity and 1's entry was the lru entry.
    /// assert_eq!(Some((1, "a")), cache.push(3, "alpha"));
    ///
    /// assert_eq!(cache.get(&1), None);
    /// assert_eq!(cache.get(&2), Some(&"beta"));
    /// assert_eq!(cache.get(&3), Some(&"alpha"));
    /// ```
    pub fn push(&mut self, k: K, v: V) -> Option<(K, V)> {
        Some(match self.entry(k) {
            Entry::Occupied(entry) => entry.replace_entry(v),
            Entry::Vacant(entry) => match entry.try_insert_entry(v) {
                Ok(mut entry) => entry.take_evicted()?,
                Err(rejected) => rejected,
            },
        })
    }

    /// Returns a reference to the value of the key in the cache or `None` if it is not
    /// present in the cache. Moves the key to the head of the LRU list if it exists.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(2, "c");
    /// cache.put(3, "d");
    ///
    /// assert_eq!(cache.get(&1), None);
    /// assert_eq!(cache.get(&2), Some(&"c"));
    /// assert_eq!(cache.get(&3), Some(&"d"));
    /// ```
    pub fn get<'a, Q>(&'a mut self, k: &Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        Some(self.get_mut(k)?)
    }

    /// Returns a mutable reference to the value of the key in the cache or `None` if it
    /// is not present in the cache. Moves the key to the head of the LRU list if it exists.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put("apple", 8);
    /// cache.put("banana", 4);
    /// cache.put("banana", 6);
    /// cache.put("pear", 2);
    ///
    /// assert_eq!(cache.get_mut(&"apple"), None);
    /// assert_eq!(cache.get_mut(&"banana"), Some(&mut 6));
    /// assert_eq!(cache.get_mut(&"pear"), Some(&mut 2));
    /// ```
    pub fn get_mut<'a, Q>(&'a mut self, k: &Q) -> Option<&'a mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.entry_ref(k) {
            Entry::Occupied(entry) => Some(entry.into_mut()),
            Entry::Vacant(_) => None,
        }
    }

    /// Returns a reference to the value of the key in the cache if it is
    /// present in the cache and moves the key to the head of the LRU list.
    /// If the key does not exist the provided `FnOnce` is used to populate
    /// the list and a reference is returned.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(2, "c");
    /// cache.put(3, "d");
    ///
    /// assert_eq!(cache.get_or_insert(2, ||"a"), &"c");
    /// assert_eq!(cache.get_or_insert(3, ||"a"), &"d");
    /// assert_eq!(cache.get_or_insert(1, ||"a"), &"a");
    /// assert_eq!(cache.get_or_insert(1, ||"b"), &"a");
    /// ```
    pub fn get_or_insert<'a, F>(&'a mut self, k: K, f: F) -> &'a V
    where
        F: FnOnce() -> V,
    {
        self.get_or_insert_mut(k, f)
    }

    /// Returns a reference to the value of the key in the cache if it is
    /// present in the cache and moves the key to the head of the LRU list.
    /// If the key does not exist the provided `FnOnce` is used to populate
    /// the list and a reference is returned. If the cache has zero total
    /// capacity, returns the entry which could not be inserted as an Err.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(2, "c");
    /// cache.put(3, "d");
    ///
    /// assert_eq!(cache.try_get_or_insert(2, ||"a"), Ok(&"c"));
    /// assert_eq!(cache.try_get_or_insert(3, ||"a"), Ok(&"d"));
    /// assert_eq!(cache.try_get_or_insert(1, ||"a"), Ok(&"a"));
    /// assert_eq!(cache.try_get_or_insert(1, ||"b"), Ok(&"a"));
    /// ```
    pub fn try_get_or_insert<'a, F>(&'a mut self, k: K, f: F) -> Result<&'a V, (K, V)>
    where
        F: FnOnce() -> V,
    {
        Ok(self.try_get_or_insert_mut(k, f)?)
    }

    /// Returns a mutable reference to the value of the key in the cache if it is
    /// present in the cache and moves the key to the head of the LRU list.
    /// If the key does not exist the provided `FnOnce` is used to populate
    /// the list and a mutable reference is returned.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    ///
    /// let v = cache.get_or_insert_mut(2, ||"c");
    /// assert_eq!(v, &"b");
    /// *v = "d";
    /// assert_eq!(cache.get_or_insert_mut(2, ||"e"), &mut "d");
    /// assert_eq!(cache.get_or_insert_mut(3, ||"f"), &mut "f");
    /// assert_eq!(cache.get_or_insert_mut(3, ||"e"), &mut "f");
    /// ```
    pub fn get_or_insert_mut<'a, F>(&'a mut self, k: K, f: F) -> &'a mut V
    where
        F: FnOnce() -> V,
    {
        self.try_get_or_insert_mut(k, f)
            .unwrap_or_else(|_| panic!("Cache has zero capacity"))
    }

    /// Returns a mutable reference to the value of the key in the cache if it is
    /// present in the cache and moves the key to the head of the LRU list.
    /// If the key does not exist the provided `FnOnce` is used to populate
    /// the list and a mutable reference is returned. If the cache has zero total
    /// capacity, returns the entry which could not be inserted as an Err.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    ///
    /// let v = cache.try_get_or_insert_mut(2, ||"c").unwrap();
    /// assert_eq!(v, &"b");
    /// *v = "d";
    /// assert_eq!(cache.try_get_or_insert_mut(2, ||"e"), Ok(&mut "d"));
    /// assert_eq!(cache.try_get_or_insert_mut(3, ||"f"), Ok(&mut "f"));
    /// assert_eq!(cache.try_get_or_insert_mut(3, ||"e"), Ok(&mut "f"));
    /// ```
    pub fn try_get_or_insert_mut<'a, F>(&'a mut self, k: K, f: F) -> Result<&'a mut V, (K, V)>
    where
        F: FnOnce() -> V,
    {
        match self.entry(k) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => entry.try_insert(f()),
        }
    }

    /// Returns a reference to the value corresponding to the key in the cache or `None` if it is
    /// not present in the cache. Unlike `get`, `peek` does not update the LRU list so the key's
    /// position will be unchanged.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    ///
    /// assert_eq!(cache.peek(&1), Some(&"a"));
    /// assert_eq!(cache.peek(&2), Some(&"b"));
    /// ```
    pub fn peek<'a, Q>(&'a self, k: &Q) -> Option<&'a V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map
            .get(KeyWrapper::from_ref(k))
            .map(|node| unsafe { &*node.0.as_ref().val.as_ptr() })
    }

    /// Returns a mutable reference to the value corresponding to the key in the cache or `None`
    /// if it is not present in the cache. Unlike `get_mut`, `peek_mut` does not update the LRU
    /// list so the key's position will be unchanged.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    ///
    /// assert_eq!(cache.peek_mut(&1), Some(&mut "a"));
    /// assert_eq!(cache.peek_mut(&2), Some(&mut "b"));
    /// ```
    pub fn peek_mut<'a, Q>(&'a mut self, k: &Q) -> Option<&'a mut V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.entry_ref(k) {
            Entry::Occupied(entry) => Some(entry.into_peek()),
            Entry::Vacant(_) => None,
        }
    }

    /// Returns the value corresponding to the least recently used item or `None` if the
    /// cache is empty. Like `peek`, `peek_lru` does not update the LRU list so the item's
    /// position will be unchanged.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    ///
    /// assert_eq!(cache.peek_lru(), Some((&1, &"a")));
    /// ```
    pub fn peek_lru<'a>(&'a self) -> Option<(&'a K, &'a V)> {
        if self.is_empty() {
            return None;
        }

        let (key, val);
        unsafe {
            // safety: we can unwrap root unchecked because if we're not empty, we've already
            //  allocated
            let node = self.root.unwrap_unchecked().as_ref().prev;
            key = &(*(*node).key.as_ptr()) as &K;
            val = &(*(*node).val.as_ptr()) as &V;
        }

        Some((key, val))
    }

    /// Returns a bool indicating whether the given key is in the cache. Does not update the
    /// LRU list.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    ///
    /// assert!(!cache.contains(&1));
    /// assert!(cache.contains(&2));
    /// assert!(cache.contains(&3));
    /// ```
    pub fn contains<Q>(&self, k: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        self.map.contains(KeyWrapper::from_ref(k))
    }

    /// Removes and returns the value corresponding to the key from the cache or
    /// `None` if it does not exist.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(2, "a");
    ///
    /// assert_eq!(cache.pop(&1), None);
    /// assert_eq!(cache.pop(&2), Some("a"));
    /// assert_eq!(cache.pop(&2), None);
    /// assert_eq!(cache.len(), 0);
    /// ```
    pub fn pop<Q>(&mut self, k: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        Some(self.pop_entry(k)?.1)
    }

    /// Removes and returns the key and the value corresponding to the key from the cache or
    /// `None` if it does not exist.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "a");
    ///
    /// assert_eq!(cache.pop(&1), Some("a"));
    /// assert_eq!(cache.pop_entry(&2), Some((2, "a")));
    /// assert_eq!(cache.pop(&1), None);
    /// assert_eq!(cache.pop_entry(&2), None);
    /// assert_eq!(cache.len(), 0);
    /// ```
    pub fn pop_entry<Q>(&mut self, k: &Q) -> Option<(K, V)>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        match self.entry_ref(k) {
            Entry::Occupied(entry) => Some(entry.remove_entry()),
            Entry::Vacant(_) => None,
        }
    }

    /// Removes and returns the key and value corresponding to the least recently
    /// used item or `None` if the cache is empty.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    ///
    /// cache.put(2, "a");
    /// cache.put(3, "b");
    /// cache.put(4, "c");
    /// cache.get(&3);
    ///
    /// assert_eq!(cache.pop_lru(), Some((4, "c")));
    /// assert_eq!(cache.pop_lru(), Some((3, "b")));
    /// assert_eq!(cache.pop_lru(), None);
    /// assert_eq!(cache.len(), 0);
    /// ```
    pub fn pop_lru(&mut self) -> Option<(K, V)> {
        Some(self.entry_lru()?.remove_entry())
    }

    /// Marks the key as the most recently used one.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    /// cache.get(&1);
    /// cache.get(&2);
    ///
    /// // If we do `pop_lru` now, we would pop 3.
    /// // assert_eq!(cache.pop_lru(), Some((3, "c")));
    ///
    /// // By promoting 3, we make sure it isn't popped.
    /// cache.promote(&3);
    /// assert_eq!(cache.pop_lru(), Some((1, "a")));
    /// ```
    pub fn promote<'a, Q>(&'a mut self, k: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Entry::Occupied(mut entry) = self.entry_ref(k) {
            entry.promote();
        }
    }

    /// Marks the key as the least recently used one.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(3);
    ///
    /// cache.put(1, "a");
    /// cache.put(2, "b");
    /// cache.put(3, "c");
    /// cache.get(&1);
    /// cache.get(&2);
    ///
    /// // If we do `pop_lru` now, we would pop 3.
    /// // assert_eq!(cache.pop_lru(), Some((3, "c")));
    ///
    /// // By demoting 1 and 2, we make sure those are popped first.
    /// cache.demote(&2);
    /// cache.demote(&1);
    /// assert_eq!(cache.pop_lru(), Some((1, "a")));
    /// assert_eq!(cache.pop_lru(), Some((2, "b")));
    /// ```
    pub fn demote<'a, Q>(&'a mut self, k: &Q)
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        if let Entry::Occupied(mut entry) = self.entry_ref(k) {
            entry.demote();
        }
    }

    /// Returns the number of key-value pairs that are currently in the the cache.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    /// assert_eq!(cache.len(), 0);
    ///
    /// cache.put(1, "a");
    /// assert_eq!(cache.len(), 1);
    ///
    /// cache.put(2, "b");
    /// assert_eq!(cache.len(), 2);
    ///
    /// cache.put(3, "c");
    /// assert_eq!(cache.len(), 2);
    /// ```
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns a bool indicating whether the cache is empty or not.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    /// assert!(cache.is_empty());
    ///
    /// cache.put(1, "a");
    /// assert!(!cache.is_empty());
    /// ```
    pub fn is_empty(&self) -> bool {
        self.map.len() == 0
    }

    /// Gets a reference to the cache's limiter.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let cache = LruCache::<usize, usize>::new(10);
    /// assert_eq!(cache.cap(), cache.limiter().limit());
    /// ```
    pub fn limiter(&self) -> &L {
        &self.limiter
    }

    /// Gets a mutable reference to the cache's limiter. The actual reference is wrapped in a
    /// deref-able guard which handles automatically updating the cache if the limiter's limit
    /// changes.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    /// cache.put(1, 1);
    /// cache.put(2, 2);
    /// cache.limiter_mut().set_limit(1);
    /// assert_eq!(cache.len(), 1);
    /// ```
    pub fn limiter_mut(&mut self) -> impl '_ + DerefMut<Target = L> {
        struct Guard<'a, K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher>(
            &'a mut LruCache<K, V, L, S>,
        );

        impl<'a, K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher> Deref for Guard<'a, K, V, L, S> {
            type Target = L;

            fn deref(&self) -> &Self::Target {
                &self.0.limiter
            }
        }

        impl<'a, K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher> DerefMut for Guard<'a, K, V, L, S> {
            fn deref_mut(&mut self) -> &mut Self::Target {
                &mut self.0.limiter
            }
        }

        impl<'a, K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher> Drop for Guard<'a, K, V, L, S> {
            fn drop(&mut self) {
                while self.0.limiter.is_oversized(self.0) {
                    self.0.pop_lru();
                }
            }
        }

        Guard(self)
    }

    /// Shrinks the capacity of the cache as much as possible. This will not evict any entries.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache = LruCache::new(2);
    /// cache.put(1, 1);
    /// cache.put(2, 2);
    /// cache.shrink_to_fit();
    /// ```
    pub fn shrink_to_fit(&mut self) {
        self.map.shrink_to_fit();
    }

    /// Clears the contents of the cache.
    ///
    /// # Example
    ///
    /// ```
    /// use lru::LruCache;
    /// let mut cache: LruCache<isize, &str> = LruCache::new(2);
    /// assert_eq!(cache.len(), 0);
    ///
    /// cache.put(1, "a");
    /// assert_eq!(cache.len(), 1);
    ///
    /// cache.put(2, "b");
    /// assert_eq!(cache.len(), 2);
    ///
    /// cache.clear();
    /// assert_eq!(cache.len(), 0);
    /// ```
    pub fn clear(&mut self) {
        while self.pop_lru().is_some() {}
    }

    /// An iterator visiting all entries in most-recently used order. The iterator element type is
    /// `(&K, &V)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lru::LruCache;
    ///
    /// let mut cache = LruCache::new(3);
    /// cache.put("a", 1);
    /// cache.put("b", 2);
    /// cache.put("c", 3);
    ///
    /// for (key, val) in cache.iter() {
    ///     println!("key: {} val: {}", key, val);
    /// }
    /// ```
    pub fn iter(&self) -> Iter<'_, K, V> {
        Iter {
            len: self.len(),
            ptr: unsafe { self.root.map_or(ptr::null_mut(), |x| x.as_ref().next) },
            end: unsafe { self.root.map_or(ptr::null_mut(), |x| x.as_ref().prev) },
            phantom: PhantomData,
        }
    }

    /// An iterator visiting all entries in most-recently-used order, giving a mutable reference on
    /// V.  The iterator element type is `(&K, &mut V)`.
    ///
    /// # Examples
    ///
    /// ```
    /// use lru::LruCache;
    ///
    /// struct HddBlock {
    ///     dirty: bool,
    ///     data: [u8; 512]
    /// }
    ///
    /// let mut cache = LruCache::new(3);
    /// cache.put(0, HddBlock { dirty: false, data: [0x00; 512]});
    /// cache.put(1, HddBlock { dirty: true,  data: [0x55; 512]});
    /// cache.put(2, HddBlock { dirty: true,  data: [0x77; 512]});
    ///
    /// // write dirty blocks to disk.
    /// for (block_id, block) in cache.iter_mut() {
    ///     if block.dirty {
    ///         // write block to disk
    ///         block.dirty = false
    ///     }
    /// }
    /// ```
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        IterMut {
            len: self.len(),
            ptr: unsafe { self.root.map_or(ptr::null_mut(), |x| x.as_ref().next) },
            end: unsafe { self.root.map_or(ptr::null_mut(), |x| x.as_ref().prev) },
            phantom: PhantomData,
        }
    }

    fn detach(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            (*(*node).prev).next = (*node).next;
            (*(*node).next).prev = (*node).prev;
        }
    }

    fn alloc_root(&mut self) {
        self.root.get_or_insert_with(|| unsafe {
            let root = Box::into_raw(Box::new(LruEntry::new_sigil()));
            (*root).next = root;
            (*root).prev = root;
            NonNull::new_unchecked(root)
        });
    }

    // Attaches `node` after the sigil `self.head` node.
    fn attach(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            let root = self.root.unwrap_unchecked().as_ptr();
            (*node).next = (*root).next;
            (*node).prev = root;
            (*root).next = node;
            (*(*node).next).prev = node;
        }
    }

    // Attaches `node` before the sigil `self.tail` node.
    fn attach_last(&mut self, node: *mut LruEntry<K, V>) {
        unsafe {
            let root = self.root.unwrap_unchecked().as_ptr();
            (*node).next = root;
            (*node).prev = (*root).prev;
            (*root).prev = node;
            (*(*node).prev).next = node;
        }
    }
}

impl<K, V, L, S> Drop for LruCache<K, V, L, S> {
    fn drop(&mut self) {
        self.map.drain().for_each(|node| unsafe {
            let mut node = *Box::from_raw(node.0.as_ptr());
            ptr::drop_in_place((node).key.as_mut_ptr());
            ptr::drop_in_place((node).val.as_mut_ptr());
        });
        // We rebox the head/tail, and because these are maybe-uninit
        // they do not have the absent k/v dropped.

        if let Some(root) = self.root {
            let _ = unsafe { *Box::from_raw(root.as_ptr()) };
        }
    }
}

impl<'a, K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher> IntoIterator
    for &'a LruCache<K, V, L, S>
{
    type Item = (&'a K, &'a V);
    type IntoIter = Iter<'a, K, V>;

    fn into_iter(self) -> Iter<'a, K, V> {
        self.iter()
    }
}

impl<'a, K: Hash + Eq, V, L: Limiter<K, V, S>, S: BuildHasher> IntoIterator
    for &'a mut LruCache<K, V, L, S>
{
    type Item = (&'a K, &'a mut V);
    type IntoIter = IterMut<'a, K, V>;

    fn into_iter(self) -> IterMut<'a, K, V> {
        self.iter_mut()
    }
}

// The compiler does not automatically derive Send and Sync for LruCache because it contains
// raw pointers. The raw pointers are safely encapsulated by LruCache though so we can
// implement Send and Sync for it below.
unsafe impl<K: Send, V: Send, L: Send, S: Send> Send for LruCache<K, V, L, S> {}
unsafe impl<K: Sync, V: Sync, L: Sync, S: Sync> Sync for LruCache<K, V, L, S> {}

impl<K: Hash + Eq, V, L: Limiter<K, V, S> + Debug, S: BuildHasher> fmt::Debug
    for LruCache<K, V, L, S>
{
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("LruCache")
            .field("len", &self.len())
            .field("limiter", &self.limiter())
            .finish()
    }
}

/// An iterator over the entries of a `LruCache`.
///
/// This `struct` is created by the [`iter`] method on [`LruCache`][`LruCache`]. See its
/// documentation for more.
///
/// [`iter`]: struct.LruCache.html#method.iter
/// [`LruCache`]: struct.LruCache.html
pub struct Iter<'a, K: 'a, V: 'a> {
    len: usize,

    ptr: *const LruEntry<K, V>,
    end: *const LruEntry<K, V>,

    phantom: PhantomData<&'a K>,
}

impl<'a, K, V> Iterator for Iter<'a, K, V> {
    type Item = (&'a K, &'a V);

    fn next(&mut self) -> Option<(&'a K, &'a V)> {
        if self.len == 0 {
            return None;
        }

        let key = unsafe { &(*(*self.ptr).key.as_ptr()) as &K };
        let val = unsafe { &(*(*self.ptr).val.as_ptr()) as &V };

        self.len -= 1;
        self.ptr = unsafe { (*self.ptr).next };

        Some((key, val))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }

    fn count(self) -> usize {
        self.len
    }
}

impl<'a, K, V> DoubleEndedIterator for Iter<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a V)> {
        if self.len == 0 {
            return None;
        }

        let key = unsafe { &(*(*self.end).key.as_ptr()) as &K };
        let val = unsafe { &(*(*self.end).val.as_ptr()) as &V };

        self.len -= 1;
        self.end = unsafe { (*self.end).prev };

        Some((key, val))
    }
}

impl<'a, K, V> ExactSizeIterator for Iter<'a, K, V> {}
impl<'a, K, V> FusedIterator for Iter<'a, K, V> {}

impl<'a, K, V> Clone for Iter<'a, K, V> {
    fn clone(&self) -> Iter<'a, K, V> {
        Iter {
            len: self.len,
            ptr: self.ptr,
            end: self.end,
            phantom: PhantomData,
        }
    }
}

// The compiler does not automatically derive Send and Sync for Iter because it contains
// raw pointers.
unsafe impl<'a, K: Send, V: Send> Send for Iter<'a, K, V> {}
unsafe impl<'a, K: Sync, V: Sync> Sync for Iter<'a, K, V> {}

/// An iterator over mutables entries of a `LruCache`.
///
/// This `struct` is created by the [`iter_mut`] method on [`LruCache`][`LruCache`]. See its
/// documentation for more.
///
/// [`iter_mut`]: struct.LruCache.html#method.iter_mut
/// [`LruCache`]: struct.LruCache.html
pub struct IterMut<'a, K: 'a, V: 'a> {
    len: usize,

    ptr: *mut LruEntry<K, V>,
    end: *mut LruEntry<K, V>,

    phantom: PhantomData<&'a K>,
}

impl<'a, K, V> Iterator for IterMut<'a, K, V> {
    type Item = (&'a K, &'a mut V);

    fn next(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.len == 0 {
            return None;
        }

        let key = unsafe { &mut (*(*self.ptr).key.as_mut_ptr()) as &mut K };
        let val = unsafe { &mut (*(*self.ptr).val.as_mut_ptr()) as &mut V };

        self.len -= 1;
        self.ptr = unsafe { (*self.ptr).next };

        Some((key, val))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.len, Some(self.len))
    }

    fn count(self) -> usize {
        self.len
    }
}

impl<'a, K, V> DoubleEndedIterator for IterMut<'a, K, V> {
    fn next_back(&mut self) -> Option<(&'a K, &'a mut V)> {
        if self.len == 0 {
            return None;
        }

        let key = unsafe { &mut (*(*self.end).key.as_mut_ptr()) as &mut K };
        let val = unsafe { &mut (*(*self.end).val.as_mut_ptr()) as &mut V };

        self.len -= 1;
        self.end = unsafe { (*self.end).prev };

        Some((key, val))
    }
}

impl<'a, K, V> ExactSizeIterator for IterMut<'a, K, V> {}
impl<'a, K, V> FusedIterator for IterMut<'a, K, V> {}

// The compiler does not automatically derive Send and Sync for Iter because it contains
// raw pointers.
unsafe impl<'a, K: Send, V: Send> Send for IterMut<'a, K, V> {}
unsafe impl<'a, K: Sync, V: Sync> Sync for IterMut<'a, K, V> {}

/// An iterator that moves out of a `LruCache`.
///
/// This `struct` is created by the [`into_iter`] method on [`LruCache`][`LruCache`]. See its
/// documentation for more.
///
/// [`into_iter`]: struct.LruCache.html#method.into_iter
/// [`LruCache`]: struct.LruCache.html
pub struct IntoIter<K, V>
where
    K: Hash + Eq,
{
    cache: LruCache<K, V>,
}

impl<K, V> Iterator for IntoIter<K, V>
where
    K: Hash + Eq,
{
    type Item = (K, V);

    fn next(&mut self) -> Option<(K, V)> {
        self.cache.pop_lru()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let len = self.cache.len();
        (len, Some(len))
    }

    fn count(self) -> usize {
        self.cache.len()
    }
}

impl<K, V> ExactSizeIterator for IntoIter<K, V> where K: Hash + Eq {}
impl<K, V> FusedIterator for IntoIter<K, V> where K: Hash + Eq {}

impl<K: Hash + Eq, V> IntoIterator for LruCache<K, V> {
    type Item = (K, V);
    type IntoIter = IntoIter<K, V>;

    fn into_iter(self) -> IntoIter<K, V> {
        IntoIter { cache: self }
    }
}

#[cfg(test)]
mod tests {
    use super::{AddBehavior, CostLimited, Limiter, LruCache, SizeLimited};
    use core::fmt::Debug;
    use scoped_threadpool::Pool;
    use std::cell::RefCell;
    use std::hash::Hash;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn assert_opt_eq<V: PartialEq + Debug>(opt: Option<&V>, v: V) {
        assert!(opt.is_some());
        assert_eq!(opt.unwrap(), &v);
    }

    fn assert_opt_eq_mut<V: PartialEq + Debug>(opt: Option<&mut V>, v: V) {
        assert!(opt.is_some());
        assert_eq!(opt.unwrap(), &v);
    }

    fn assert_opt_eq_tuple<K: PartialEq + Debug, V: PartialEq + Debug>(
        opt: Option<(&K, &V)>,
        kv: (K, V),
    ) {
        assert!(opt.is_some());
        let res = opt.unwrap();
        assert_eq!(res.0, &kv.0);
        assert_eq!(res.1, &kv.1);
    }

    fn assert_opt_eq_mut_tuple<K: PartialEq + Debug, V: PartialEq + Debug>(
        opt: Option<(&K, &mut V)>,
        kv: (K, V),
    ) {
        assert!(opt.is_some());
        let res = opt.unwrap();
        assert_eq!(res.0, &kv.0);
        assert_eq!(res.1, &kv.1);
    }

    #[test]
    fn test_unbounded() {
        let mut cache = LruCache::unbounded();
        for i in 0..13370 {
            cache.put(i, ());
        }
        assert_eq!(cache.len(), 13370);
    }

    #[test]
    #[cfg(feature = "hashbrown")]
    fn test_with_hasher() {
        use hashbrown::hash_map::DefaultHashBuilder;

        let s = DefaultHashBuilder::default();
        let mut cache = LruCache::with_hasher(16, s);

        for i in 0..13370 {
            cache.put(i, ());
        }
        assert_eq!(cache.len(), 16);
    }

    #[test]
    fn test_put_and_get() {
        let mut cache = LruCache::new(2);
        assert!(cache.is_empty());

        assert_eq!(cache.put("apple", "red"), None);
        assert_eq!(cache.put("banana", "yellow"), None);

        assert_eq!(cache.cap(), 2);
        assert_eq!(cache.len(), 2);
        assert!(!cache.is_empty());
        assert_opt_eq(cache.get(&"apple"), "red");
        assert_opt_eq(cache.get(&"banana"), "yellow");
    }

    #[test]
    fn test_put_and_get_or_insert() {
        let mut cache = LruCache::new(2);
        assert!(cache.is_empty());

        assert_eq!(cache.put("apple", "red"), None);
        assert_eq!(cache.put("banana", "yellow"), None);

        assert_eq!(cache.cap(), 2);
        assert_eq!(cache.len(), 2);
        assert!(!cache.is_empty());
        assert_eq!(cache.get_or_insert("apple", || "orange"), &"red");
        assert_eq!(cache.get_or_insert("banana", || "orange"), &"yellow");
        assert_eq!(cache.get_or_insert("lemon", || "orange"), &"orange");
        assert_eq!(cache.get_or_insert("lemon", || "red"), &"orange");
    }

    #[test]
    fn test_put_and_get_or_insert_mut() {
        let mut cache = LruCache::new(2);
        assert!(cache.is_empty());

        assert_eq!(cache.put("apple", "red"), None);
        assert_eq!(cache.put("banana", "yellow"), None);

        assert_eq!(cache.cap(), 2);
        assert_eq!(cache.len(), 2);

        let v = cache.get_or_insert_mut("apple", || "orange");
        assert_eq!(v, &"red");
        *v = "blue";

        assert_eq!(cache.get_or_insert_mut("apple", || "orange"), &"blue");
        assert_eq!(cache.get_or_insert_mut("banana", || "orange"), &"yellow");
        assert_eq!(cache.get_or_insert_mut("lemon", || "orange"), &"orange");
        assert_eq!(cache.get_or_insert_mut("lemon", || "red"), &"orange");
    }

    #[test]
    fn test_put_and_get_mut() {
        let mut cache = LruCache::new(2);

        cache.put("apple", "red");
        cache.put("banana", "yellow");

        assert_eq!(cache.cap(), 2);
        assert_eq!(cache.len(), 2);
        assert_opt_eq_mut(cache.get_mut(&"apple"), "red");
        assert_opt_eq_mut(cache.get_mut(&"banana"), "yellow");
    }

    #[test]
    fn test_get_mut_and_update() {
        let mut cache = LruCache::new(2);

        cache.put("apple", 1);
        cache.put("banana", 3);

        {
            let v = cache.get_mut(&"apple").unwrap();
            *v = 4;
        }

        assert_eq!(cache.cap(), 2);
        assert_eq!(cache.len(), 2);
        assert_opt_eq_mut(cache.get_mut(&"apple"), 4);
        assert_opt_eq_mut(cache.get_mut(&"banana"), 3);
    }

    #[test]
    fn test_put_update() {
        let mut cache = LruCache::new(2);

        assert_eq!(cache.put("apple", "red"), None);
        assert_eq!(cache.put("apple", "green"), Some("red"));

        assert_eq!(cache.len(), 1);
        assert_opt_eq(cache.get(&"apple"), "green");
    }

    #[test]
    fn test_put_removes_oldest() {
        let mut cache = LruCache::new(2);

        assert_eq!(cache.put("apple", "red"), None);
        assert_eq!(cache.put("banana", "yellow"), None);
        assert_eq!(cache.put("pear", "green"), None);

        assert!(cache.get(&"apple").is_none());
        assert_opt_eq(cache.get(&"banana"), "yellow");
        assert_opt_eq(cache.get(&"pear"), "green");

        // Even though we inserted "apple" into the cache earlier it has since been removed from
        // the cache so there is no current value for `put` to return.
        assert_eq!(cache.put("apple", "green"), None);
        assert_eq!(cache.put("tomato", "red"), None);

        assert!(cache.get(&"pear").is_none());
        assert_opt_eq(cache.get(&"apple"), "green");
        assert_opt_eq(cache.get(&"tomato"), "red");
    }

    #[test]
    fn test_peek() {
        let mut cache = LruCache::new(2);

        cache.put("apple", "red");
        cache.put("banana", "yellow");

        assert_opt_eq(cache.peek(&"banana"), "yellow");
        assert_opt_eq(cache.peek(&"apple"), "red");

        cache.put("pear", "green");

        assert!(cache.peek(&"apple").is_none());
        assert_opt_eq(cache.peek(&"banana"), "yellow");
        assert_opt_eq(cache.peek(&"pear"), "green");
    }

    #[test]
    fn test_peek_mut() {
        let mut cache = LruCache::new(2);

        cache.put("apple", "red");
        cache.put("banana", "yellow");

        assert_opt_eq_mut(cache.peek_mut(&"banana"), "yellow");
        assert_opt_eq_mut(cache.peek_mut(&"apple"), "red");
        assert!(cache.peek_mut(&"pear").is_none());

        cache.put("pear", "green");

        assert!(cache.peek_mut(&"apple").is_none());
        assert_opt_eq_mut(cache.peek_mut(&"banana"), "yellow");
        assert_opt_eq_mut(cache.peek_mut(&"pear"), "green");

        {
            let v = cache.peek_mut(&"banana").unwrap();
            *v = "green";
        }

        assert_opt_eq_mut(cache.peek_mut(&"banana"), "green");
    }

    #[test]
    fn test_peek_lru() {
        let mut cache = LruCache::new(2);

        assert!(cache.peek_lru().is_none());

        cache.put("apple", "red");
        cache.put("banana", "yellow");
        assert_opt_eq_tuple(cache.peek_lru(), ("apple", "red"));

        cache.get(&"apple");
        assert_opt_eq_tuple(cache.peek_lru(), ("banana", "yellow"));

        cache.clear();
        assert!(cache.peek_lru().is_none());
    }

    #[test]
    fn test_contains() {
        let mut cache = LruCache::new(2);

        cache.put("apple", "red");
        cache.put("banana", "yellow");
        cache.put("pear", "green");

        assert!(!cache.contains(&"apple"));
        assert!(cache.contains(&"banana"));
        assert!(cache.contains(&"pear"));
    }

    #[test]
    fn test_pop() {
        let mut cache = LruCache::new(2);

        cache.put("apple", "red");
        cache.put("banana", "yellow");

        assert_eq!(cache.len(), 2);
        assert_opt_eq(cache.get(&"apple"), "red");
        assert_opt_eq(cache.get(&"banana"), "yellow");

        let popped = cache.pop(&"apple");
        assert!(popped.is_some());
        assert_eq!(popped.unwrap(), "red");
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&"apple").is_none());
        assert_opt_eq(cache.get(&"banana"), "yellow");
    }

    #[test]
    fn test_pop_entry() {
        let mut cache = LruCache::new(2);
        cache.put("apple", "red");
        cache.put("banana", "yellow");

        assert_eq!(cache.len(), 2);
        assert_opt_eq(cache.get(&"apple"), "red");
        assert_opt_eq(cache.get(&"banana"), "yellow");

        let popped = cache.pop_entry(&"apple");
        assert!(popped.is_some());
        assert_eq!(popped.unwrap(), ("apple", "red"));
        assert_eq!(cache.len(), 1);
        assert!(cache.get(&"apple").is_none());
        assert_opt_eq(cache.get(&"banana"), "yellow");
    }

    #[test]
    fn test_pop_lru() {
        let mut cache = LruCache::new(200);

        for i in 0..75 {
            cache.put(i, "A");
        }
        for i in 0..75 {
            cache.put(i + 100, "B");
        }
        for i in 0..75 {
            cache.put(i + 200, "C");
        }
        assert_eq!(cache.len(), 200);

        for i in 0..75 {
            assert_opt_eq(cache.get(&(74 - i + 100)), "B");
        }
        assert_opt_eq(cache.get(&25), "A");

        for i in 26..75 {
            assert_eq!(cache.pop_lru(), Some((i, "A")));
        }
        for i in 0..75 {
            assert_eq!(cache.pop_lru(), Some((i + 200, "C")));
        }
        for i in 0..75 {
            assert_eq!(cache.pop_lru(), Some((74 - i + 100, "B")));
        }
        assert_eq!(cache.pop_lru(), Some((25, "A")));
        for _ in 0..50 {
            assert_eq!(cache.pop_lru(), None);
        }
    }

    #[test]
    fn test_clear() {
        let mut cache = LruCache::new(2);

        cache.put("apple", "red");
        cache.put("banana", "yellow");

        assert_eq!(cache.len(), 2);
        assert_opt_eq(cache.get(&"apple"), "red");
        assert_opt_eq(cache.get(&"banana"), "yellow");

        cache.clear();
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn test_resize_larger() {
        let mut cache = LruCache::new(2);

        cache.put(1, "a");
        cache.put(2, "b");
        cache.resize(4);
        cache.put(3, "c");
        cache.put(4, "d");

        assert_eq!(cache.len(), 4);
        assert_eq!(cache.get(&1), Some(&"a"));
        assert_eq!(cache.get(&2), Some(&"b"));
        assert_eq!(cache.get(&3), Some(&"c"));
        assert_eq!(cache.get(&4), Some(&"d"));
    }

    #[test]
    fn test_resize_smaller() {
        let mut cache = LruCache::new(4);

        cache.put(1, "a");
        cache.put(2, "b");
        cache.put(3, "c");
        cache.put(4, "d");

        cache.resize(2);

        assert_eq!(cache.len(), 2);
        assert!(cache.get(&1).is_none());
        assert!(cache.get(&2).is_none());
        assert_eq!(cache.get(&3), Some(&"c"));
        assert_eq!(cache.get(&4), Some(&"d"));
    }

    #[test]
    fn test_send() {
        use std::thread;

        let mut cache = LruCache::new(4);
        cache.put(1, "a");

        let handle = thread::spawn(move || {
            assert_eq!(cache.get(&1), Some(&"a"));
        });

        assert!(handle.join().is_ok());
    }

    #[test]
    fn test_multiple_threads() {
        let mut pool = Pool::new(1);
        let mut cache = LruCache::new(4);
        cache.put(1, "a");

        let cache_ref = &cache;
        pool.scoped(|scoped| {
            scoped.execute(move || {
                assert_eq!(cache_ref.peek(&1), Some(&"a"));
            });
        });

        assert_eq!((cache_ref).peek(&1), Some(&"a"));
    }

    #[test]
    fn test_iter_forwards() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);

        {
            // iter const
            let mut iter = cache.iter();
            assert_eq!(iter.len(), 3);
            assert_opt_eq_tuple(iter.next(), ("c", 3));

            assert_eq!(iter.len(), 2);
            assert_opt_eq_tuple(iter.next(), ("b", 2));

            assert_eq!(iter.len(), 1);
            assert_opt_eq_tuple(iter.next(), ("a", 1));

            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next(), None);
        }
        {
            // iter mut
            let mut iter = cache.iter_mut();
            assert_eq!(iter.len(), 3);
            assert_opt_eq_mut_tuple(iter.next(), ("c", 3));

            assert_eq!(iter.len(), 2);
            assert_opt_eq_mut_tuple(iter.next(), ("b", 2));

            assert_eq!(iter.len(), 1);
            assert_opt_eq_mut_tuple(iter.next(), ("a", 1));

            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next(), None);
        }
    }

    #[test]
    fn test_iter_backwards() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);

        {
            // iter const
            let mut iter = cache.iter();
            assert_eq!(iter.len(), 3);
            assert_opt_eq_tuple(iter.next_back(), ("a", 1));

            assert_eq!(iter.len(), 2);
            assert_opt_eq_tuple(iter.next_back(), ("b", 2));

            assert_eq!(iter.len(), 1);
            assert_opt_eq_tuple(iter.next_back(), ("c", 3));

            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next_back(), None);
        }

        {
            // iter mut
            let mut iter = cache.iter_mut();
            assert_eq!(iter.len(), 3);
            assert_opt_eq_mut_tuple(iter.next_back(), ("a", 1));

            assert_eq!(iter.len(), 2);
            assert_opt_eq_mut_tuple(iter.next_back(), ("b", 2));

            assert_eq!(iter.len(), 1);
            assert_opt_eq_mut_tuple(iter.next_back(), ("c", 3));

            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next_back(), None);
        }
    }

    #[test]
    fn test_iter_forwards_and_backwards() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);

        {
            // iter const
            let mut iter = cache.iter();
            assert_eq!(iter.len(), 3);
            assert_opt_eq_tuple(iter.next(), ("c", 3));

            assert_eq!(iter.len(), 2);
            assert_opt_eq_tuple(iter.next_back(), ("a", 1));

            assert_eq!(iter.len(), 1);
            assert_opt_eq_tuple(iter.next(), ("b", 2));

            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next_back(), None);
        }
        {
            // iter mut
            let mut iter = cache.iter_mut();
            assert_eq!(iter.len(), 3);
            assert_opt_eq_mut_tuple(iter.next(), ("c", 3));

            assert_eq!(iter.len(), 2);
            assert_opt_eq_mut_tuple(iter.next_back(), ("a", 1));

            assert_eq!(iter.len(), 1);
            assert_opt_eq_mut_tuple(iter.next(), ("b", 2));

            assert_eq!(iter.len(), 0);
            assert_eq!(iter.next_back(), None);
        }
    }

    #[test]
    fn test_iter_multiple_threads() {
        let mut pool = Pool::new(1);
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);

        let mut iter = cache.iter();
        assert_eq!(iter.len(), 3);
        assert_opt_eq_tuple(iter.next(), ("c", 3));

        {
            let iter_ref = &mut iter;
            pool.scoped(|scoped| {
                scoped.execute(move || {
                    assert_eq!(iter_ref.len(), 2);
                    assert_opt_eq_tuple(iter_ref.next(), ("b", 2));
                });
            });
        }

        assert_eq!(iter.len(), 1);
        assert_opt_eq_tuple(iter.next(), ("a", 1));

        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_iter_clone() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);

        let mut iter = cache.iter();
        let mut iter_clone = iter.clone();

        assert_eq!(iter.len(), 2);
        assert_opt_eq_tuple(iter.next(), ("b", 2));
        assert_eq!(iter_clone.len(), 2);
        assert_opt_eq_tuple(iter_clone.next(), ("b", 2));

        assert_eq!(iter.len(), 1);
        assert_opt_eq_tuple(iter.next(), ("a", 1));
        assert_eq!(iter_clone.len(), 1);
        assert_opt_eq_tuple(iter_clone.next(), ("a", 1));

        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
        assert_eq!(iter_clone.len(), 0);
        assert_eq!(iter_clone.next(), None);
    }

    #[test]
    fn test_into_iter() {
        let mut cache = LruCache::new(3);
        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);

        let mut iter = cache.into_iter();
        assert_eq!(iter.len(), 3);
        assert_eq!(iter.next(), Some(("a", 1)));

        assert_eq!(iter.len(), 2);
        assert_eq!(iter.next(), Some(("b", 2)));

        assert_eq!(iter.len(), 1);
        assert_eq!(iter.next(), Some(("c", 3)));

        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_that_pop_actually_detaches_node() {
        let mut cache = LruCache::new(5);

        cache.put("a", 1);
        cache.put("b", 2);
        cache.put("c", 3);
        cache.put("d", 4);
        cache.put("e", 5);

        assert_eq!(cache.pop(&"c"), Some(3));

        cache.put("f", 6);

        let mut iter = cache.iter();
        assert_opt_eq_tuple(iter.next(), ("f", 6));
        assert_opt_eq_tuple(iter.next(), ("e", 5));
        assert_opt_eq_tuple(iter.next(), ("d", 4));
        assert_opt_eq_tuple(iter.next(), ("b", 2));
        assert_opt_eq_tuple(iter.next(), ("a", 1));
        assert!(iter.next().is_none());
    }

    #[test]
    fn test_get_with_borrow() {
        use alloc::string::String;

        let mut cache = LruCache::new(2);

        let key = String::from("apple");
        cache.put(key, "red");

        assert_opt_eq(cache.get("apple"), "red");
    }

    #[test]
    fn test_get_mut_with_borrow() {
        use alloc::string::String;

        let mut cache = LruCache::new(2);

        let key = String::from("apple");
        cache.put(key, "red");

        assert_opt_eq_mut(cache.get_mut("apple"), "red");
    }

    #[test]
    fn test_no_memory_leaks() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter;

        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let n = 100;
        for _ in 0..n {
            let mut cache = LruCache::new(1);
            for i in 0..n {
                cache.put(i, DropCounter {});
            }
        }
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), n * n);
    }

    #[test]
    fn test_no_memory_leaks_with_clear() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter;

        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let n = 100;
        for _ in 0..n {
            let mut cache = LruCache::new(1);
            for i in 0..n {
                cache.put(i, DropCounter {});
            }
            cache.clear();
        }
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), n * n);
    }

    #[test]
    fn test_no_memory_leaks_with_resize() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        struct DropCounter;

        impl Drop for DropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let n = 100;
        for _ in 0..n {
            let mut cache = LruCache::new(1);
            for i in 0..n {
                cache.put(i, DropCounter {});
            }
            cache.clear();
        }
        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), n * n);
    }

    #[test]
    fn test_no_memory_leaks_with_pop() {
        static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

        #[derive(Hash, Eq)]
        struct KeyDropCounter(usize);

        impl PartialEq for KeyDropCounter {
            fn eq(&self, other: &Self) -> bool {
                self.0.eq(&other.0)
            }
        }

        impl Drop for KeyDropCounter {
            fn drop(&mut self) {
                DROP_COUNT.fetch_add(1, Ordering::SeqCst);
            }
        }

        let n = 100;
        for _ in 0..n {
            let mut cache = LruCache::new(1);

            for i in 0..100 {
                cache.put(KeyDropCounter(i), i);
                cache.pop(&KeyDropCounter(i));
            }
        }

        assert_eq!(DROP_COUNT.load(Ordering::SeqCst), n * n * 2);
    }

    #[test]
    fn test_promote_and_demote() {
        let mut cache = LruCache::new(5);
        for i in 0..5 {
            cache.push(i, i);
        }
        cache.promote(&1);
        cache.promote(&0);
        cache.demote(&3);
        cache.demote(&4);
        assert_eq!(cache.pop_lru(), Some((4, 4)));
        assert_eq!(cache.pop_lru(), Some((3, 3)));
        assert_eq!(cache.pop_lru(), Some((2, 2)));
        assert_eq!(cache.pop_lru(), Some((1, 1)));
        assert_eq!(cache.pop_lru(), Some((0, 0)));
        assert_eq!(cache.pop_lru(), None);
    }

    #[test]
    fn test_zero_cap() {
        let mut cache = LruCache::new(0);
        assert_eq!(cache.put(0, 0), Some(0));
        assert_eq!(cache.push(1, 1), Some((1, 1)));
        assert_eq!(cache.try_get_or_insert(2, || 2), Err((2, 2)));
        assert_eq!(cache.try_get_or_insert_mut(3, || 3), Err((3, 3)));
    }

    #[test]
    fn test_size_limiter() {
        let mut cache = LruCache::new(2);
        cache.put(1, 1);
        cache.put(2, 2);
        assert_eq!(cache.push(3, 3), Some((1, 1)));
        let _ = cache.pop_lru();
        assert_eq!(cache.push(4, 4), None);
        assert_eq!(cache.push(5, 5), Some((3, 3)));
    }

    #[test]
    fn test_unlimited_limiter() {
        let mut cache = LruCache::unbounded();
        for i in 0..1000 {
            cache.put(i, i);
        }
        assert_eq!(cache.len(), 1000);
    }

    struct TraceLimited<L> {
        pub limiter: L,
        adds: RefCell<usize>,
        updates: RefCell<usize>,
        removes: RefCell<usize>,
    }

    impl<L> TraceLimited<L> {
        pub fn new(limiter: L) -> Self {
            Self {
                limiter,
                adds: RefCell::new(0),
                updates: RefCell::new(0),
                removes: RefCell::new(0),
            }
        }

        pub fn reset(&mut self) -> (usize, usize, usize) {
            (
                self.adds.replace(0),
                self.updates.replace(0),
                self.removes.replace(0),
            )
        }
    }

    impl<K: Hash + Eq, V, L: Limiter<K, V, S>, S> Limiter<K, V, S> for TraceLimited<L> {
        fn is_oversized(&self, cache: &LruCache<K, V, impl Limiter<K, V, S>, S>) -> bool {
            self.limiter.is_oversized(cache)
        }

        fn on_add(
            &self,
            cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
            key: &K,
            value: &V,
        ) -> AddBehavior {
            *self.adds.borrow_mut() += 1;
            self.limiter.on_add(cache, key, value)
        }

        fn on_update(
            &self,
            cache: &LruCache<K, V, impl Limiter<K, V, S>, S>,
            old_key: &K,
            old_value: &V,
            new_key: Option<&K>,
            new_value: Option<&V>,
        ) -> AddBehavior {
            *self.updates.borrow_mut() += 1;
            self.limiter
                .on_update(cache, old_key, old_value, new_key, new_value)
        }

        fn on_remove(&self, cache: &LruCache<K, V, impl Limiter<K, V, S>, S>, key: &K, value: &V) {
            *self.removes.borrow_mut() += 1;
            self.limiter.on_remove(cache, key, value)
        }
    }

    #[test]
    fn test_limit_put() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.put(0, 0);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 0));
        cache.put(1, 1);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 1));
        cache.put(1, 2);
        assert_eq!(cache.limiter_mut().reset(), (0, 1, 0));
    }

    #[test]
    fn test_limit_push() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.push(0, 0);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 0));
        cache.push(1, 1);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 1));
        cache.push(1, 2);
        assert_eq!(cache.limiter_mut().reset(), (0, 1, 0));
    }

    #[test]
    fn test_limit_get_or_insert() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.get_or_insert(0, || 0);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 0));
        cache.get_or_insert(0, || 0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.get_or_insert(1, || 1);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 1));
    }

    #[test]
    fn test_limit_get_or_insert_mut() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.get_or_insert_mut(0, || 0);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 0));
        cache.get_or_insert_mut(0, || 0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.get_or_insert_mut(1, || 1);
        assert_eq!(cache.limiter_mut().reset(), (1, 0, 1));
    }

    #[test]
    fn test_limit_pop() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.pop(&0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.push(0, 0);
        cache.limiter_mut().reset();
        cache.pop(&1);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.pop(&0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 1));
        cache.pop(&0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
    }

    #[test]
    fn test_limit_pop_entry() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.pop_entry(&0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.push(0, 0);
        cache.limiter_mut().reset();
        cache.pop_entry(&1);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.pop_entry(&0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 1));
        cache.pop_entry(&0);
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
    }

    #[test]
    fn test_limit_pop_lru() {
        let mut cache = LruCache::with_limiter(TraceLimited::new(SizeLimited::new(1)));
        cache.pop_lru();
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
        cache.push(0, 0);
        cache.limiter_mut().reset();
        cache.pop_lru();
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 1));
        cache.pop_lru();
        assert_eq!(cache.limiter_mut().reset(), (0, 0, 0));
    }

    #[test]
    fn test_cost_limited() {
        let mut cache = LruCache::with_limiter(CostLimited::with_func(
            10,
            (|_key: &usize| 0, |value: &usize| *value),
        ));
        for i in 0..5 {
            cache.put(i, i);
        }
        assert_eq!(cache.len(), 5);
        {
            let mut entry = cache.entry(10).insert(10);
            for i in 0..5 {
                assert_eq!(entry.take_evicted(), Some((i, i)));
            }
            assert_eq!(entry.take_evicted(), None);
        }
        assert_eq!(cache.len(), 1);
    }
}

/// Doctests for what should *not* compile
///
/// ```compile_fail
/// let mut cache = lru::LruCache::<u32, u32>::unbounded();
/// let _: &'static u32 = cache.get_or_insert(0, || 92);
/// ```
///
/// ```compile_fail
/// let mut cache = lru::LruCache::<u32, u32>::unbounded();
/// let _: Option<(&'static u32, _)> = cache.peek_lru();
/// let _: Option<(_, &'static u32)> = cache.peek_lru();
/// ```
fn _test_lifetimes() {}
