//! A `Vec` whose item type is only known at runtime.

use std::alloc::{self, Layout};
use std::mem::MaybeUninit;
use std::ptr::{self, NonNull};

/// Drops a value through a type-erased pointer.
///
/// # Safety
///
/// The pointer must point to a valid, owned value of the type the function was
/// created for, and that value must not be used again afterwards.
pub(crate) type DropFn = unsafe fn(NonNull<u8>);

/// Returns the [`DropFn`] for `T`, or `None` if `T` needs no dropping.
pub(crate) fn drop_fn_of<T>() -> Option<DropFn> {
    unsafe fn drop_ptr<T>(ptr: NonNull<u8>) {
        // SAFETY: the caller guarantees that `ptr` points to an owned `T`.
        unsafe { ptr.cast::<T>().drop_in_place() }
    }
    std::mem::needs_drop::<T>().then_some(drop_ptr::<T> as DropFn)
}

/// A growable array of items described only by a [`Layout`] and a [`DropFn`].
///
/// This is the storage behind one component column of an archetype: it works
/// like `Vec<T>` with the type erased. The vector does not know `T`, so most
/// methods are `unsafe` and rely on the caller to only hand it pointers to
/// values of the type it was created for.
pub(crate) struct BlobVec {
    /// Layout of one item, padded to a multiple of its alignment.
    item_layout: Layout,
    drop: Option<DropFn>,
    /// The allocation, or an aligned dangling pointer while nothing is
    /// allocated (always the case for zero-sized items).
    data: NonNull<u8>,
    /// Capacity in items; `usize::MAX` for zero-sized items.
    capacity: usize,
    len: usize,
}

impl BlobVec {
    /// Creates an empty vector for items with the given layout and drop
    /// function. Nothing is allocated until the first push.
    pub(crate) fn new(item_layout: Layout, drop: Option<DropFn>) -> Self {
        let item_layout = item_layout.pad_to_align();
        let capacity = if item_layout.size() == 0 {
            usize::MAX
        } else {
            0
        };
        Self {
            item_layout,
            drop,
            data: dangling(item_layout.align()),
            capacity,
            len: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Makes room for at least `additional` more items.
    pub(crate) fn reserve(&mut self, additional: usize) {
        let required = self.len.checked_add(additional).expect("capacity overflow");
        if required > self.capacity {
            self.grow(required);
        }
    }

    #[cold]
    fn grow(&mut self, required: usize) {
        // Zero-sized items have unlimited capacity, so only sized items get here.
        debug_assert_ne!(self.item_layout.size(), 0);
        let new_capacity = required.max(self.capacity * 2).max(4);
        let new_layout = array_layout(self.item_layout, new_capacity).expect("capacity overflow");
        let new_data = if self.capacity == 0 {
            // SAFETY: items are sized and `new_capacity > 0`, so the layout
            // is not zero-sized.
            unsafe { alloc::alloc(new_layout) }
        } else {
            let old_layout = array_layout(self.item_layout, self.capacity)
                .expect("the current allocation has a valid layout");
            // SAFETY: `data` was allocated by the global allocator with
            // `old_layout`, and `new_layout` validated that the new size is
            // non-zero and does not overflow `isize` when aligned.
            unsafe { alloc::realloc(self.data.as_ptr(), old_layout, new_layout.size()) }
        };
        self.data = NonNull::new(new_data).unwrap_or_else(|| alloc::handle_alloc_error(new_layout));
        self.capacity = new_capacity;
    }

    /// Moves the value behind `value` to the end of the vector.
    ///
    /// If this panics (on capacity overflow), the value is leaked.
    ///
    /// # Safety
    ///
    /// - `value` must point to a valid value of this vector's item type.
    /// - The value is moved into the vector: the caller must not use or drop
    ///   it afterwards (wrap it in `ManuallyDrop` or `mem::forget` it).
    /// - `value` must not point into this vector, since pushing may
    ///   reallocate.
    pub(crate) unsafe fn push(&mut self, value: NonNull<u8>) {
        self.reserve(1);
        // SAFETY: `reserve` made room for the slot at `len`, and the caller
        // guarantees `value` is a valid item outside this vector.
        unsafe {
            let slot = self.slot(self.len);
            ptr::copy_nonoverlapping(value.as_ptr(), slot.as_ptr(), self.item_layout.size());
        }
        self.len += 1;
    }

    /// Returns a pointer to the item at `index`.
    ///
    /// The pointer may be used to mutate the item for as long as the caller
    /// ensures no other access to it overlaps.
    ///
    /// # Safety
    ///
    /// `index` must be less than `len`.
    pub(crate) unsafe fn get_unchecked(&self, index: usize) -> NonNull<u8> {
        debug_assert!(index < self.len);
        // SAFETY: the caller guarantees `index < len <= capacity`.
        unsafe { self.slot(index) }
    }

    /// Drops the item at `index` and moves `value` into its place.
    ///
    /// If this panics (on capacity overflow, or in the old item's destructor),
    /// the vector stays valid and `value` is either stored or leaked.
    ///
    /// # Safety
    ///
    /// - `index` must be less than `len`.
    /// - `value` must satisfy the same requirements as for
    ///   [`push`](Self::push).
    pub(crate) unsafe fn replace_unchecked(&mut self, index: usize, value: NonNull<u8>) {
        debug_assert!(index < self.len);
        let size = self.item_layout.size();
        let Some(drop_fn) = self.drop else {
            // SAFETY: `index < len`, and the caller guarantees `value` is a
            // valid item outside this vector.
            unsafe { ptr::copy_nonoverlapping(value.as_ptr(), self.slot(index).as_ptr(), size) };
            return;
        };
        // If the old item's destructor panics, the slot must already hold the
        // new item, or the old one would be dropped a second time later. So
        // the old item is first moved to the spare slot just past the end,
        // where nothing will ever drop it again.
        self.reserve(1);
        // SAFETY: after `reserve`, `index < len < capacity`, so both slots are
        // in bounds and distinct. The old item is moved out before `value` is
        // moved in, and is then dropped exactly once.
        unsafe {
            let slot = self.slot(index);
            let spare = self.slot(self.len);
            ptr::copy_nonoverlapping(slot.as_ptr(), spare.as_ptr(), size);
            ptr::copy_nonoverlapping(value.as_ptr(), slot.as_ptr(), size);
            drop_fn(spare);
        }
    }

    /// Removes the item at `index` and drops it, moving the last item into
    /// its place.
    ///
    /// # Safety
    ///
    /// `index` must be less than `len`.
    pub(crate) unsafe fn swap_remove_and_drop_unchecked(&mut self, index: usize) {
        // SAFETY: the caller guarantees `index < len`.
        let removed = unsafe { self.swap_remove_and_forget_unchecked(index) };
        if let Some(drop_fn) = self.drop {
            // SAFETY: the removed item is valid and no longer counted by
            // `len`, so it is dropped exactly once.
            unsafe { drop_fn(removed) };
        }
    }

    /// Removes the item at `index` without dropping it, moving the last item
    /// into its place.
    ///
    /// Returns a pointer to the removed item, which now sits just past the
    /// end of the vector. The caller takes ownership of it and must move it
    /// out or drop it before the vector is mutated again.
    ///
    /// # Safety
    ///
    /// `index` must be less than `len`.
    pub(crate) unsafe fn swap_remove_and_forget_unchecked(&mut self, index: usize) -> NonNull<u8> {
        debug_assert!(index < self.len);
        let last = self.len - 1;
        // SAFETY: the caller guarantees `index < len`, and `last < len`.
        let (removed, last_slot) = unsafe { (self.slot(index), self.slot(last)) };
        if index != last {
            // SAFETY: both slots are in bounds and distinct, so they don't
            // overlap. Swapping `MaybeUninit<u8>` instead of `u8` keeps the
            // move untyped: padding bytes may be uninitialized, and pointer
            // provenance inside the items must survive.
            unsafe {
                ptr::swap_nonoverlapping(
                    removed.cast::<MaybeUninit<u8>>().as_ptr(),
                    last_slot.cast::<MaybeUninit<u8>>().as_ptr(),
                    self.item_layout.size(),
                );
            }
        }
        self.len = last;
        last_slot
    }

    /// Drops all items, keeping the allocation.
    pub(crate) fn clear(&mut self) {
        let len = self.len;
        // Shrink first: if a destructor panics, the remaining items leak
        // instead of being dropped twice.
        self.len = 0;
        if let Some(drop_fn) = self.drop {
            for index in 0..len {
                // SAFETY: every item below the old `len` is valid, and is no
                // longer counted by `len`, so each is dropped exactly once.
                unsafe { drop_fn(self.slot(index)) };
            }
        }
    }

    /// Returns a pointer to the slot at `index`, which may be past `len`.
    ///
    /// # Safety
    ///
    /// `index` must not exceed `capacity`.
    unsafe fn slot(&self, index: usize) -> NonNull<u8> {
        // SAFETY: with `index <= capacity` the offset stays within the
        // allocation (or is zero for zero-sized items), so it cannot overflow.
        unsafe { self.data.add(index * self.item_layout.size()) }
    }
}

impl Drop for BlobVec {
    fn drop(&mut self) {
        // If an item's destructor panics, the allocation below leaks, which
        // is safe.
        self.clear();
        if self.item_layout.size() != 0 && self.capacity != 0 {
            let layout = array_layout(self.item_layout, self.capacity)
                .expect("the current allocation has a valid layout");
            // SAFETY: `data` was allocated by the global allocator with this
            // layout and is not used again.
            unsafe { alloc::dealloc(self.data.as_ptr(), layout) };
        }
    }
}

/// The layout of `n` consecutive items, or `None` if it would be too large.
fn array_layout(item: Layout, n: usize) -> Option<Layout> {
    let size = item.size().checked_mul(n)?;
    Layout::from_size_align(size, item.align()).ok()
}

/// A non-null pointer with the given alignment, valid for zero-sized accesses.
fn dangling(align: usize) -> NonNull<u8> {
    // SAFETY: a `Layout`'s alignment is a non-zero power of two.
    unsafe { NonNull::new_unchecked(ptr::without_provenance_mut(align)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::marker::PhantomData;
    use std::mem::ManuallyDrop;
    use std::panic::{self, AssertUnwindSafe};
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A typed wrapper so the tests can drive `BlobVec` through a safe API.
    struct Typed<T>(BlobVec, PhantomData<T>);

    impl<T> Typed<T> {
        fn new() -> Self {
            Self(
                BlobVec::new(Layout::new::<T>(), drop_fn_of::<T>()),
                PhantomData,
            )
        }

        fn len(&self) -> usize {
            self.0.len()
        }

        fn push(&mut self, value: T) {
            let mut value = ManuallyDrop::new(value);
            // SAFETY: the vector was created for `T`, and `ManuallyDrop` hands
            // ownership of the value over to it.
            unsafe { self.0.push(NonNull::from(&mut *value).cast()) }
        }

        fn get(&self, index: usize) -> &T {
            assert!(index < self.len());
            // SAFETY: `index` is in bounds and the item is a `T`.
            unsafe { self.0.get_unchecked(index).cast::<T>().as_ref() }
        }

        fn replace(&mut self, index: usize, value: T) {
            assert!(index < self.len());
            let mut value = ManuallyDrop::new(value);
            // SAFETY: as in `push`, and `index` is in bounds.
            unsafe {
                self.0
                    .replace_unchecked(index, NonNull::from(&mut *value).cast())
            }
        }

        fn swap_remove(&mut self, index: usize) -> T {
            assert!(index < self.len());
            // SAFETY: `index` is in bounds, and the removed `T` is read out
            // exactly once, which takes ownership of it.
            unsafe {
                self.0
                    .swap_remove_and_forget_unchecked(index)
                    .cast::<T>()
                    .read()
            }
        }

        fn swap_remove_and_drop(&mut self, index: usize) {
            assert!(index < self.len());
            // SAFETY: `index` is in bounds.
            unsafe { self.0.swap_remove_and_drop_unchecked(index) }
        }
    }

    /// Counts how many times values sharing the same counter were dropped.
    struct Tracked {
        id: u32,
        drops: Rc<Cell<u32>>,
        panic_on_drop: bool,
    }

    impl Tracked {
        fn new(id: u32, drops: &Rc<Cell<u32>>) -> Self {
            Self {
                id,
                drops: drops.clone(),
                panic_on_drop: false,
            }
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            if self.panic_on_drop {
                panic!("Tracked({}) panicked on drop", self.id);
            }
        }
    }

    #[test]
    fn push_and_get_across_growth() {
        let mut vec = Typed::<u64>::new();
        for i in 0..100 {
            vec.push(i * 3);
        }
        assert_eq!(vec.len(), 100);
        assert!((0..100).all(|i| *vec.get(i as usize) == i * 3));
    }

    #[test]
    fn swap_remove_fills_the_hole_with_the_last_item() {
        let mut vec = Typed::<String>::new();
        for word in ["a", "b", "c", "d"] {
            vec.push(word.to_owned());
        }
        assert_eq!(vec.swap_remove(1), "b");
        assert_eq!([vec.get(0), vec.get(1), vec.get(2)], ["a", "d", "c"]);
        assert_eq!(vec.swap_remove(2), "c");
        assert_eq!(vec.len(), 2);
    }

    #[test]
    fn every_item_is_dropped_exactly_once() {
        let drops = Rc::new(Cell::new(0));
        let mut vec = Typed::new();
        for id in 0..10 {
            vec.push(Tracked::new(id, &drops));
        }

        vec.swap_remove_and_drop(3);
        assert_eq!(drops.get(), 1);

        vec.replace(0, Tracked::new(10, &drops));
        assert_eq!(drops.get(), 2);
        assert_eq!(vec.get(0).id, 10);

        let removed = vec.swap_remove(0);
        assert_eq!(drops.get(), 2);
        drop(removed);
        assert_eq!(drops.get(), 3);

        drop(vec);
        assert_eq!(drops.get(), 11);
    }

    #[test]
    fn clear_drops_items_and_keeps_the_vector_usable() {
        let drops = Rc::new(Cell::new(0));
        let mut vec = Typed::new();
        for id in 0..5 {
            vec.push(Tracked::new(id, &drops));
        }
        vec.0.clear();
        assert_eq!(drops.get(), 5);
        assert_eq!(vec.len(), 0);

        vec.push(Tracked::new(5, &drops));
        assert_eq!(vec.get(0).id, 5);
    }

    #[test]
    fn replace_survives_a_panicking_destructor() {
        let drops = Rc::new(Cell::new(0));
        let mut vec = Typed::new();
        let mut bomb = Tracked::new(0, &drops);
        bomb.panic_on_drop = true;
        vec.push(bomb);

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            vec.replace(0, Tracked::new(1, &drops));
        }));
        assert!(result.is_err());
        assert_eq!(vec.get(0).id, 1);

        drop(vec);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn zero_sized_items() {
        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct Zst;
        impl Drop for Zst {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut vec = Typed::new();
        for _ in 0..1000 {
            vec.push(Zst);
        }
        vec.swap_remove_and_drop(500);
        vec.replace(0, Zst);
        drop(vec.swap_remove(0));
        assert_eq!(vec.len(), 998);
        assert_eq!(DROPS.load(Ordering::Relaxed), 3);

        drop(vec);
        assert_eq!(DROPS.load(Ordering::Relaxed), 1001);
    }

    #[test]
    fn over_aligned_items() {
        #[repr(align(64))]
        struct Aligned(u8);

        let mut vec = Typed::new();
        for i in 0..20 {
            vec.push(Aligned(i));
        }
        for i in 0..20 {
            let item = vec.get(i);
            assert_eq!((item as *const Aligned).addr() % 64, 0);
            assert_eq!(item.0, i as u8);
        }
    }

    #[test]
    fn items_with_padding_survive_swaps() {
        let mut vec = Typed::<(u8, u64)>::new();
        for i in 0..8 {
            vec.push((i, u64::from(i) << 40));
        }
        assert_eq!(vec.swap_remove(0), (0, 0));
        assert_eq!(*vec.get(0), (7, 7 << 40));
    }
}
