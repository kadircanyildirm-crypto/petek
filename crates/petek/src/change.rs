//! Change detection: ticks that record when components were added or last
//! changed.

use std::cell::UnsafeCell;
use std::fmt;
use std::ops::{Deref, DerefMut};

/// A point in a world's history.
///
/// Each world keeps a tick that moves forward every time something is
/// written. Components remember the tick at which they were added and at which
/// they last changed, and a [`QueryState`](crate::query::QueryState)
/// remembers the tick of its last run, so that the
/// [`Added`](crate::query::Added) and [`Changed`](crate::query::Changed)
/// filters can tell what is new since then.
///
/// Ticks are 64-bit, so they never wrap around in practice.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Tick(u64);

impl Tick {
    /// The tick before anything happened.
    pub const ZERO: Self = Self(0);

    /// The tick after this one.
    ///
    /// # Panics
    ///
    /// If ticks run out, which takes 2⁶⁴ writes.
    #[must_use]
    pub(crate) fn next(self) -> Self {
        self.after(1)
    }

    /// The tick `n` ticks after this one.
    ///
    /// # Panics
    ///
    /// If ticks run out.
    #[must_use]
    pub(crate) fn after(self, n: usize) -> Self {
        let next = u64::try_from(n)
            .ok()
            .and_then(|n| self.0.checked_add(n))
            .expect("change ticks ran out");
        Self(next)
    }
}

/// The ticks a query compares against: what is newer than `last_run` counts
/// as added or changed, and what it writes is stamped with `this_run`.
#[derive(Clone, Copy, Debug)]
pub struct Ticks {
    pub(crate) last_run: Tick,
    pub(crate) this_run: Tick,
}

/// A column of ticks, one per row of an archetype.
///
/// The ticks are in `UnsafeCell`s, so that a query holding only a shared
/// reference to the world can stamp the components it writes. That is sound
/// because a query only writes the ticks of components it has exclusive
/// access to; see [`QueryData`](crate::query::QueryData).
pub(crate) type TickColumn = Vec<UnsafeCell<Tick>>;

/// A shared view of a [`TickColumn`], held by queries.
///
/// Unlike `&[UnsafeCell<Tick>]`, it is `Send` and `Sync`, so that query
/// iterators are too. That is sound for the same reason that writing through
/// it is: queries only read ticks no one is writing, and only write ticks no
/// one else is accessing.
#[derive(Clone, Copy, Debug)]
pub struct TickSlice<'w>(&'w [UnsafeCell<Tick>]);

// SAFETY: see the type's documentation.
unsafe impl Send for TickSlice<'_> {}
// SAFETY: see the type's documentation.
unsafe impl Sync for TickSlice<'_> {}

impl<'w> TickSlice<'w> {
    pub(crate) fn new(ticks: &'w [UnsafeCell<Tick>]) -> Self {
        Self(ticks)
    }

    /// Reads the tick at `row`.
    ///
    /// # Safety
    ///
    /// `row` must be in bounds, and no one may be writing the tick.
    #[inline]
    pub(crate) unsafe fn read(self, row: usize) -> Tick {
        debug_assert!(row < self.0.len());
        // SAFETY: the caller guarantees `row` is in bounds and the tick is
        // not being written.
        unsafe { *self.0.get_unchecked(row).get() }
    }

    /// Borrows the tick at `row` mutably for `'w`.
    ///
    /// # Safety
    ///
    /// `row` must be in bounds, and no one else may access the tick for `'w`.
    #[inline]
    pub(crate) unsafe fn get_mut(self, row: usize) -> &'w mut Tick {
        debug_assert!(row < self.0.len());
        // SAFETY: the caller guarantees `row` is in bounds and the access is
        // exclusive for `'w`.
        unsafe { &mut *self.0.get_unchecked(row).get() }
    }
}

/// A mutable reference to a component that marks it as changed when it is
/// written through.
///
/// Queries for `&mut T` yield a `Mut<T>`, and so does
/// [`World::get_mut`](crate::World::get_mut). It dereferences to `T`, so it
/// can mostly be used like `&mut T`; only writing through it, with
/// [`DerefMut`], marks the component as changed for the
/// [`Changed`](crate::query::Changed) filter. It can also tell whether the
/// component was [added](Self::is_added) or [changed](Self::is_changed)
/// since the query last ran.
///
/// ```
/// use petek::{Component, World};
/// use petek::query::{Changed, QueryState};
///
/// struct Health(u32);
/// impl Component for Health {}
///
/// let mut world = World::new();
/// let a = world.spawn(Health(10));
/// let b = world.spawn(Health(10));
///
/// let mut changed = QueryState::<&Health, Changed<Health>>::new(&mut world);
/// assert_eq!(changed.iter(&world).count(), 2); // newly added counts as changed
///
/// for (i, mut health) in world.query_mut::<&mut Health>().into_iter().enumerate() {
///     if i == 0 {
///         health.0 -= 1; // writes, so marks it changed
///     } else {
///         let _ = health.0; // only reads
///     }
/// }
/// assert_eq!(changed.iter(&world).count(), 1);
/// ```
pub struct Mut<'w, T: ?Sized> {
    value: &'w mut T,
    /// When the value was added, which `Mut` never changes.
    added: Tick,
    changed: &'w mut Tick,
    ticks: Ticks,
}

impl<'w, T: ?Sized> Mut<'w, T> {
    pub(crate) fn new(value: &'w mut T, added: Tick, changed: &'w mut Tick, ticks: Ticks) -> Self {
        Self {
            value,
            added,
            changed,
            ticks,
        }
    }

    /// Returns `true` if the component was added since the query that handed
    /// it out last ran.
    ///
    /// A one-off query, or [`World::get_mut`](crate::World::get_mut), has
    /// never run before, so to it every component counts as added.
    #[must_use]
    #[inline]
    pub fn is_added(&self) -> bool {
        self.added > self.ticks.last_run
    }

    /// Returns `true` if the component was added or changed since the query
    /// that handed it out last ran, including through this `Mut`.
    #[must_use]
    #[inline]
    pub fn is_changed(&self) -> bool {
        *self.changed > self.ticks.last_run
    }

    /// Returns the underlying reference, marking the component as changed.
    #[must_use]
    pub fn into_inner(self) -> &'w mut T {
        *self.changed = self.ticks.this_run;
        self.value
    }
}

impl<T: ?Sized> Deref for Mut<'_, T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        self.value
    }
}

impl<T: ?Sized> DerefMut for Mut<'_, T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut T {
        *self.changed = self.ticks.this_run;
        self.value
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Mut<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Mut").field(&self.value).finish()
    }
}
