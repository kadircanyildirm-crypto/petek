//! A component column: the values, and the ticks at which each was added and
//! last changed.

use std::cell::UnsafeCell;
use std::ptr::NonNull;

use super::blob_vec::BlobVec;
use crate::change::{Tick, TickColumn, TickSlice};
use crate::component::ComponentInfo;

/// The values of one component in an archetype, with their change ticks.
///
/// The three vectors always have the same length, except between
/// [`write_spare`](Self::write_spare) and
/// [`commit_spare`](Self::commit_spare). Every method updates the ticks
/// before running a destructor, so they stay in step even if one panics.
pub(crate) struct Column {
    data: BlobVec,
    added: TickColumn,
    changed: TickColumn,
}

impl Column {
    pub(crate) fn new(info: &ComponentInfo) -> Self {
        Self {
            data: BlobVec::new(info.layout(), info.drop_fn()),
            added: Vec::new(),
            changed: Vec::new(),
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }

    /// Makes room for at least `additional` more values, so that adding them
    /// cannot fail.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.data.reserve(additional);
        self.added.reserve(additional);
        self.changed.reserve(additional);
    }

    /// Moves a value into the spare slot just past the end, without counting
    /// it yet. See [`BlobVec::write_spare`].
    ///
    /// # Safety
    ///
    /// As for [`BlobVec::write_spare`].
    #[inline]
    pub(crate) unsafe fn write_spare(&mut self, value: NonNull<u8>) {
        // SAFETY: the caller upholds `write_spare`'s contract.
        unsafe { self.data.write_spare(value) };
    }

    /// Takes ownership of the value written by
    /// [`write_spare`](Self::write_spare), as added and changed at `tick`.
    ///
    /// # Safety
    ///
    /// As for [`BlobVec::commit_spare`], and there must be room for the
    /// ticks (see [`reserve`](Self::reserve)), so that this cannot panic.
    #[inline]
    pub(crate) unsafe fn commit_spare(&mut self, tick: Tick) {
        debug_assert!(self.added.len() < self.added.capacity());
        debug_assert!(self.changed.len() < self.changed.capacity());
        self.added.push(UnsafeCell::new(tick));
        self.changed.push(UnsafeCell::new(tick));
        // SAFETY: the caller upholds `commit_spare`'s contract.
        unsafe { self.data.commit_spare() };
    }

    /// A pointer to the first value; see [`BlobVec::as_ptr`].
    #[inline]
    pub(crate) fn as_ptr(&self) -> NonNull<u8> {
        self.data.as_ptr()
    }

    /// The tick at which each value was added.
    #[inline]
    pub(crate) fn added_ticks(&self) -> TickSlice<'_> {
        TickSlice::new(&self.added)
    }

    /// The tick at which each value last changed.
    #[inline]
    pub(crate) fn changed_ticks(&self) -> TickSlice<'_> {
        TickSlice::new(&self.changed)
    }

    /// Returns a pointer to the value at `row`, for reading.
    ///
    /// # Safety
    ///
    /// `row` must be less than `len()`.
    #[inline]
    pub(crate) unsafe fn get_unchecked(&self, row: usize) -> NonNull<u8> {
        // SAFETY: the caller guarantees `row` is in bounds.
        unsafe { self.data.get_unchecked(row) }
    }

    /// Returns a pointer to the value at `row` for writing, and its change
    /// tick.
    ///
    /// # Safety
    ///
    /// `row` must be less than `len()`.
    #[inline]
    pub(crate) unsafe fn get_unchecked_mut(&mut self, row: usize) -> (NonNull<u8>, &mut Tick) {
        debug_assert!(row < self.changed.len());
        // SAFETY: the caller guarantees `row` is in bounds of every vector.
        unsafe {
            let changed = self.changed.get_unchecked_mut(row).get_mut();
            (self.data.get_unchecked_mut(row), changed)
        }
    }

    /// Drops the value at `row`, moves `value` into its place, and marks it as
    /// changed at `tick`. See [`BlobVec::replace_unchecked`].
    ///
    /// # Safety
    ///
    /// As for [`BlobVec::replace_unchecked`].
    pub(crate) unsafe fn replace_unchecked(&mut self, row: usize, value: NonNull<u8>, tick: Tick) {
        debug_assert!(row < self.changed.len());
        // SAFETY: the caller guarantees `row` is in bounds of every vector.
        unsafe { *self.changed.get_unchecked_mut(row).get_mut() = tick };
        // SAFETY: the caller upholds `replace_unchecked`'s contract.
        unsafe { self.data.replace_unchecked(row, value) };
    }

    /// Removes the value at `row` and drops it, moving the last value into its
    /// place.
    ///
    /// # Safety
    ///
    /// `row` must be less than `len()`.
    pub(crate) unsafe fn swap_remove_and_drop_unchecked(&mut self, row: usize) {
        self.added.swap_remove(row);
        self.changed.swap_remove(row);
        // SAFETY: the caller guarantees `row` is in bounds.
        unsafe { self.data.swap_remove_and_drop_unchecked(row) };
    }

    /// Removes the value at `row` without dropping it, moving the last value
    /// into its place. See [`BlobVec::swap_remove_and_forget_unchecked`].
    ///
    /// # Safety
    ///
    /// `row` must be less than `len()`.
    pub(crate) unsafe fn swap_remove_and_forget_unchecked(&mut self, row: usize) -> NonNull<u8> {
        self.added.swap_remove(row);
        self.changed.swap_remove(row);
        // SAFETY: the caller guarantees `row` is in bounds.
        unsafe { self.data.swap_remove_and_forget_unchecked(row) }
    }

    /// Moves the value at `row`, with its ticks, to the end of `dst`, moving
    /// this column's last value into its place.
    ///
    /// # Safety
    ///
    /// - `row` must be less than `len()`.
    /// - `dst` must store the same component, and must have room for one more
    ///   value (see [`reserve`](Self::reserve)).
    pub(crate) unsafe fn swap_remove_into_unchecked(&mut self, row: usize, dst: &mut Column) {
        debug_assert!(dst.added.len() < dst.added.capacity());
        debug_assert!(dst.changed.len() < dst.changed.capacity());
        dst.added.push(self.added.swap_remove(row));
        dst.changed.push(self.changed.swap_remove(row));
        // SAFETY: the caller guarantees `row` is in bounds, and that `dst`
        // stores the same type and has room.
        unsafe { self.data.swap_remove_into_unchecked(row, &mut dst.data) };
    }

    /// Drops all values, keeping the allocations. If a destructor panics, the
    /// rest are still dropped.
    pub(crate) fn clear(&mut self) {
        self.added.clear();
        self.changed.clear();
        self.data.clear();
    }

    /// Checks, in debug builds, that the values and ticks are in step.
    pub(crate) fn debug_assert_consistent(&self) {
        debug_assert!(
            self.added.len() == self.data.len() && self.changed.len() == self.data.len(),
            "a column's ticks are out of step with its values",
        );
    }
}
