//! A `Vec` whose item type is only known at runtime.

use std::alloc::{self, Layout};
use std::mem::{self, MaybeUninit};
use std::ptr::{self, NonNull};

use super::drop_each;

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
    mem::needs_drop::<T>().then_some(drop_ptr::<T> as DropFn)
}

/// A growable array of items described only by a [`Layout`] and a [`DropFn`].
///
/// This is the storage behind one component column of an archetype: it works
/// like `Vec<T>` with the type erased. The vector does not know `T`, so most
/// methods are `unsafe` and rely on the caller to only hand it pointers to
/// values of the type it was created for.
///
/// Like `Vec`, it drops its items when dropped, keeps dropping the remaining
/// ones if a destructor panics, and frees its allocation either way.
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
    /// function. Nothing is allocated until an item is added.
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

    /// The number of items.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Makes room for at least `additional` more items.
    #[inline]
    pub(crate) fn reserve(&mut self, additional: usize) {
        if additional > self.capacity - self.len {
            self.grow(additional);
        }
    }

    #[cold]
    #[inline(never)]
    fn grow(&mut self, additional: usize) {
        // Zero-sized items have unlimited capacity, so only sized items get here.
        debug_assert_ne!(self.item_layout.size(), 0);
        let required = self.len.checked_add(additional).expect("capacity overflow");
        let new_capacity = required.max(self.capacity * 2).max(4);
        let new_layout = array_layout(self.item_layout, new_capacity).expect("capacity overflow");
        let new_data = match self.allocation() {
            // SAFETY: items are sized and `new_capacity > 0`, so the layout is
            // not zero-sized.
            None => unsafe { alloc::alloc(new_layout) },
            // SAFETY: `data` was allocated by the global allocator with
            // `old_layout`, and `new_layout` checked that the new size is
            // non-zero and does not overflow `isize` when aligned.
            Some(old_layout) => unsafe {
                alloc::realloc(self.data.as_ptr(), old_layout, new_layout.size())
            },
        };
        self.data = NonNull::new(new_data).unwrap_or_else(|| alloc::handle_alloc_error(new_layout));
        self.capacity = new_capacity;
    }

    /// Moves the value behind `value` into the spare slot just past the end,
    /// without counting it yet.
    ///
    /// Until [`commit_spare`](Self::commit_spare) is called, the vector does
    /// not own the value: it is leaked if the vector changes in the meantime.
    /// Writing a value first and committing it later lets callers fill several
    /// columns and only update them once nothing can fail anymore.
    ///
    /// # Safety
    ///
    /// - There must be spare capacity, as ensured by [`reserve`](Self::reserve).
    /// - `value` must point to a valid value of this vector's item type,
    ///   outside this vector. The value is moved: the caller must not use or
    ///   drop it afterwards.
    #[inline]
    pub(crate) unsafe fn write_spare(&mut self, value: NonNull<u8>) {
        debug_assert!(self.len < self.capacity);
        // SAFETY: the caller guarantees there is a spare slot at `len` and that
        // `value` is a valid item outside this vector.
        unsafe {
            let spare = self.slot(self.len);
            ptr::copy_nonoverlapping(value.as_ptr(), spare.as_ptr(), self.item_layout.size());
        }
    }

    /// Takes ownership of the value written by
    /// [`write_spare`](Self::write_spare), making it the last item.
    ///
    /// # Safety
    ///
    /// A value must have been written with `write_spare` since the length of
    /// this vector last changed.
    #[inline]
    pub(crate) unsafe fn commit_spare(&mut self) {
        debug_assert!(self.len < self.capacity);
        self.len += 1;
    }

    /// Returns a pointer to the first item.
    ///
    /// The pointer stays valid until the vector grows or is dropped. Writing
    /// through it does not need `&mut self`, because a reference to the
    /// vector only covers its header, not the items; the caller must make
    /// sure no one else is accessing those items instead.
    #[inline]
    pub(crate) fn as_ptr(&self) -> NonNull<u8> {
        self.data
    }

    /// Returns a pointer to the item at `index`, for reading.
    ///
    /// # Safety
    ///
    /// `index` must be less than `len`.
    #[inline]
    pub(crate) unsafe fn get_unchecked(&self, index: usize) -> NonNull<u8> {
        debug_assert!(index < self.len);
        // SAFETY: the caller guarantees `index < len <= capacity`.
        unsafe { self.slot(index) }
    }

    /// Returns a pointer to the item at `index`, for reading and writing.
    ///
    /// # Safety
    ///
    /// `index` must be less than `len`.
    #[inline]
    pub(crate) unsafe fn get_unchecked_mut(&mut self, index: usize) -> NonNull<u8> {
        debug_assert!(index < self.len);
        // SAFETY: the caller guarantees `index < len <= capacity`.
        unsafe { self.slot(index) }
    }

    /// Drops the item at `index` and moves `value` into its place.
    ///
    /// `value` is moved in even if the old item's destructor panics, so the
    /// slot always ends up holding a valid item.
    ///
    /// # Safety
    ///
    /// - `index` must be less than `len`.
    /// - `value` must satisfy the same requirements as for
    ///   [`write_spare`](Self::write_spare).
    pub(crate) unsafe fn replace_unchecked(&mut self, index: usize, value: NonNull<u8>) {
        /// Moves `value` into the slot when dropped, on return or unwind.
        struct MoveIn {
            value: NonNull<u8>,
            slot: NonNull<u8>,
            size: usize,
        }

        impl Drop for MoveIn {
            fn drop(&mut self) {
                // SAFETY: `replace_unchecked`'s caller guarantees `value` is a
                // valid item outside the vector, and `slot` is the in-bounds
                // slot whose old item has just been dropped.
                unsafe {
                    ptr::copy_nonoverlapping(self.value.as_ptr(), self.slot.as_ptr(), self.size);
                }
            }
        }

        debug_assert!(index < self.len);
        let move_in = MoveIn {
            value,
            // SAFETY: the caller guarantees `index < len`.
            slot: unsafe { self.slot(index) },
            size: self.item_layout.size(),
        };
        if let Some(drop_fn) = self.drop {
            // SAFETY: the slot holds a valid item, which is dropped exactly
            // once before `move_in` refills the slot.
            unsafe { drop_fn(move_in.slot) };
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
    /// out or drop it before the vector changes again.
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

    /// Moves the item at `index` to the end of `dst`, moving this vector's
    /// last item into its place.
    ///
    /// # Safety
    ///
    /// - `index` must be less than `len`.
    /// - `dst` must store the same item type, and must have spare capacity.
    pub(crate) unsafe fn swap_remove_into_unchecked(&mut self, index: usize, dst: &mut BlobVec) {
        debug_assert!(index < self.len);
        debug_assert_eq!(self.item_layout, dst.item_layout);
        let last = self.len - 1;
        // SAFETY: `index < len`, and `dst` stores the same type and has room,
        // so the item moves over and `self` stops counting it below. If it was
        // not the last item, the last one fills the hole: both slots are in
        // bounds and distinct.
        unsafe {
            dst.write_spare(self.slot(index));
            dst.commit_spare();
            if index != last {
                ptr::copy_nonoverlapping(
                    self.slot(last).as_ptr(),
                    self.slot(index).as_ptr(),
                    self.item_layout.size(),
                );
            }
        }
        self.len = last;
    }

    /// Drops all items, keeping the allocation.
    pub(crate) fn clear(&mut self) {
        // With `len` already zero, no item can be dropped twice, even if a
        // destructor panics.
        let len = mem::replace(&mut self.len, 0);
        let Some(drop_fn) = self.drop else {
            return;
        };
        drop_each(0..len, |index| {
            // SAFETY: the items below the old `len` are valid and no longer
            // counted, so each is dropped exactly once.
            unsafe { drop_fn(self.slot(index)) };
        });
    }

    /// The layout of the current allocation, or `None` if nothing is
    /// allocated.
    fn allocation(&self) -> Option<Layout> {
        if self.item_layout.size() == 0 || self.capacity == 0 {
            return None;
        }
        Some(array_layout(self.item_layout, self.capacity).expect("the allocated layout is valid"))
    }

    /// Returns a pointer to the slot at `index`, which may be past `len`.
    ///
    /// # Safety
    ///
    /// `index` must not exceed `capacity`.
    #[inline]
    unsafe fn slot(&self, index: usize) -> NonNull<u8> {
        // SAFETY: with `index <= capacity`, the offset stays within the
        // allocation (or is zero for zero-sized items), so it cannot overflow.
        unsafe { self.data.add(index * self.item_layout.size()) }
    }
}

impl Drop for BlobVec {
    fn drop(&mut self) {
        /// Frees the allocation when dropped, even if a destructor panics.
        struct Free(NonNull<u8>, Layout);

        impl Drop for Free {
            fn drop(&mut self) {
                // SAFETY: the pointer was allocated by the global allocator
                // with this layout, and its vector is going away.
                unsafe { alloc::dealloc(self.0.as_ptr(), self.1) };
            }
        }

        let _free = self.allocation().map(|layout| Free(self.data, layout));
        self.clear();
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

    /// A typed wrapper, so the tests can drive `BlobVec` through a safe API.
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
            self.0.reserve(1);
            // SAFETY: there is room, the vector was created for `T`, and
            // `ManuallyDrop` hands ownership of the value over.
            unsafe {
                self.0.write_spare(NonNull::from(&mut *value).cast());
                self.0.commit_spare();
            }
        }

        fn get(&self, index: usize) -> &T {
            assert!(index < self.len());
            // SAFETY: `index` is in bounds and the item is a `T`.
            unsafe { self.0.get_unchecked(index).cast::<T>().as_ref() }
        }

        fn get_mut(&mut self, index: usize) -> &mut T {
            assert!(index < self.len());
            // SAFETY: `index` is in bounds, the item is a `T`, and `&mut self`
            // makes the access exclusive.
            unsafe { self.0.get_unchecked_mut(index).cast::<T>().as_mut() }
        }

        fn replace(&mut self, index: usize, value: T) {
            assert!(index < self.len());
            let mut value = ManuallyDrop::new(value);
            // SAFETY: as in `push`, and `index` is in bounds.
            unsafe {
                self.0
                    .replace_unchecked(index, NonNull::from(&mut *value).cast());
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

        fn swap_remove_into(&mut self, index: usize, dst: &mut Self) {
            assert!(index < self.len());
            dst.0.reserve(1);
            // SAFETY: `index` is in bounds, and `dst` stores `T` and has room.
            unsafe { self.0.swap_remove_into_unchecked(index, &mut dst.0) }
        }
    }

    /// Counts drops in a shared counter, and optionally panics when dropped.
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

        fn bomb(id: u32, drops: &Rc<Cell<u32>>) -> Self {
            let mut bomb = Self::new(id, drops);
            bomb.panic_on_drop = true;
            bomb
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            assert!(!self.panic_on_drop, "Tracked({}) panicked on drop", self.id);
        }
    }

    #[test]
    fn push_and_get_across_growth() {
        let mut vec = Typed::<u64>::new();
        for i in 0..100 {
            vec.push(i * 3);
        }
        *vec.get_mut(7) += 1;
        assert_eq!(vec.len(), 100);
        assert_eq!(*vec.get(7), 22);
        assert!((8..100).all(|i| *vec.get(i) == i as u64 * 3));
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
    fn swap_remove_into_moves_items_between_vectors() {
        let mut src = Typed::<String>::new();
        let mut dst = Typed::<String>::new();
        for word in ["a", "b", "c"] {
            src.push(word.to_owned());
        }
        src.swap_remove_into(0, &mut dst);
        src.swap_remove_into(1, &mut dst);
        assert_eq!([dst.get(0), dst.get(1)], ["a", "b"]);
        assert_eq!(src.len(), 1);
        assert_eq!(src.get(0), "c");
    }

    #[test]
    fn staged_values_are_only_owned_once_committed() {
        let mut vec = Typed::<String>::new();
        vec.push("kept".to_owned());
        let mut staged = ManuallyDrop::new("staged".to_owned());
        vec.0.reserve(1);
        // SAFETY: there is room, and the vector stores `String`s.
        unsafe { vec.0.write_spare(NonNull::from(&mut *staged).cast()) };
        assert_eq!(vec.len(), 1);
        // SAFETY: a value was just written.
        unsafe { vec.0.commit_spare() };
        assert_eq!(vec.get(1), "staged");
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
    fn replace_moves_the_value_in_even_if_the_old_one_panics() {
        let drops = Rc::new(Cell::new(0));
        let mut vec = Typed::new();
        vec.push(Tracked::bomb(0, &drops));

        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            vec.replace(0, Tracked::new(1, &drops));
        }));
        assert!(result.is_err());
        assert_eq!(vec.get(0).id, 1);

        drop(vec);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn clear_drops_every_item_even_if_one_panics() {
        let drops = Rc::new(Cell::new(0));
        let mut vec = Typed::new();
        for id in 0..5 {
            vec.push(if id == 1 {
                Tracked::bomb(id, &drops)
            } else {
                Tracked::new(id, &drops)
            });
        }

        let result = panic::catch_unwind(AssertUnwindSafe(|| vec.0.clear()));
        assert!(result.is_err());
        assert_eq!(drops.get(), 5);
        assert_eq!(vec.len(), 0);

        vec.push(Tracked::new(5, &drops));
        assert_eq!(vec.get(0).id, 5);
    }

    #[test]
    fn dropping_frees_the_allocation_even_if_a_destructor_panics() {
        // Miri reports a leak if the allocation is not freed.
        let drops = Rc::new(Cell::new(0));
        let mut vec = Typed::new();
        vec.push(Tracked::new(0, &drops));
        vec.push(Tracked::bomb(1, &drops));
        vec.push(Tracked::new(2, &drops));

        let result = panic::catch_unwind(AssertUnwindSafe(|| drop(vec)));
        assert!(result.is_err());
        assert_eq!(drops.get(), 3);
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
        let mut other = Typed::new();
        for _ in 0..1000 {
            vec.push(Zst);
        }
        vec.swap_remove_and_drop(500);
        vec.replace(0, Zst);
        drop(vec.swap_remove(0));
        vec.swap_remove_into(0, &mut other);
        assert_eq!((vec.len(), other.len()), (997, 1));
        assert_eq!(DROPS.load(Ordering::Relaxed), 3);

        drop(vec);
        drop(other);
        assert_eq!(DROPS.load(Ordering::Relaxed), 1001);
    }

    #[test]
    fn over_aligned_items() {
        #[repr(align(64))]
        struct Aligned(usize);

        let mut vec = Typed::new();
        for i in 0..20 {
            vec.push(Aligned(i));
        }
        for i in 0..20 {
            let item = vec.get(i);
            assert_eq!(ptr::from_ref(item).addr() % 64, 0);
            assert_eq!(item.0, i);
        }
    }

    #[test]
    fn items_with_padding_survive_moves() {
        let mut vec = Typed::<(u8, u64)>::new();
        let mut other = Typed::new();
        for i in 0..8 {
            vec.push((i, u64::from(i) << 40));
        }
        assert_eq!(vec.swap_remove(0), (0, 0));
        assert_eq!(*vec.get(0), (7, 7 << 40));
        vec.swap_remove_into(1, &mut other);
        assert_eq!(*other.get(0), (1, 1 << 40));
    }
}
