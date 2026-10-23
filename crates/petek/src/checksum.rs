//! The hasher behind world checksums.

use std::hash::{Hash, Hasher};
use std::ptr::NonNull;
use std::slice;

/// Hashes the state of a world for
/// [`World::checksum`](crate::World::checksum).
///
/// Peers compare checksums to detect desyncs, so unlike the hasher of the
/// world's lookup tables, its results must not depend on the platform. It
/// works on 64-bit words, each made from an integer's value rather than its
/// bytes in memory, and reads byte slices as little-endian words. Each word is
/// mixed in like in rustc's `FxHasher`, and the result is finished with
/// `MurmurHash3`'s 64-bit finalizer, so that every bit of input affects every
/// bit of output.
///
/// It is not cryptographic: a peer that wants to hide a desync can.
pub(crate) struct ChecksumHasher {
    state: u64,
}

impl ChecksumHasher {
    /// A start other than zero, so that leading zero words still change the
    /// state.
    const START: u64 = 0x243f_6a88_85a3_08d3;
    const MULTIPLIER: u64 = 0x517c_c1b7_2722_0a95;

    pub(crate) fn new() -> Self {
        Self { state: Self::START }
    }

    #[inline]
    fn add(&mut self, word: u64) {
        self.state = (self.state.rotate_left(5) ^ word).wrapping_mul(Self::MULTIPLIER);
    }
}

impl Hasher for ChecksumHasher {
    fn finish(&self) -> u64 {
        let mut hash = self.state;
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xff51_afd7_ed55_8ccd);
        hash ^= hash >> 33;
        hash = hash.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        hash ^ (hash >> 33)
    }

    fn write(&mut self, bytes: &[u8]) {
        // The length keeps byte strings that only differ in trailing zeros
        // apart.
        self.write_usize(bytes.len());
        let mut chunks = bytes.chunks_exact(8);
        for chunk in &mut chunks {
            let mut word = [0; 8];
            word.copy_from_slice(chunk);
            self.add(u64::from_le_bytes(word));
        }
        let rest = chunks.remainder();
        if !rest.is_empty() {
            let mut word = [0; 8];
            word[..rest.len()].copy_from_slice(rest);
            self.add(u64::from_le_bytes(word));
        }
    }

    fn write_u8(&mut self, n: u8) {
        self.add(n.into());
    }

    fn write_u16(&mut self, n: u16) {
        self.add(n.into());
    }

    fn write_u32(&mut self, n: u32) {
        self.add(n.into());
    }

    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[allow(
        clippy::cast_possible_truncation,
        reason = "splitting the value into two halves"
    )]
    fn write_u128(&mut self, n: u128) {
        self.add(n as u64);
        self.add((n >> 64) as u64);
    }

    fn write_usize(&mut self, n: usize) {
        // Pointer-sized values hash the same on every platform they fit on.
        self.add(n as u64);
    }

    fn write_isize(&mut self, n: isize) {
        // Sign-extended, so that negative values hash the same everywhere.
        self.write_i64(n as i64);
    }
}

/// Hashes values through a type-erased pointer: `len` of them at `items`.
///
/// # Safety
///
/// `items` must point to `len` valid values of the type the function was
/// created for, which no one is writing.
pub(crate) type HashFn = unsafe fn(items: NonNull<u8>, len: usize, hasher: &mut ChecksumHasher);

/// Returns the [`HashFn`] for `T`.
pub(crate) fn hash_fn_of<T: Hash>() -> HashFn {
    unsafe fn hash_items<T: Hash>(items: NonNull<u8>, len: usize, hasher: &mut ChecksumHasher) {
        // SAFETY: the caller guarantees `items` points to `len` valid `T`s,
        // which no one is writing.
        let items = unsafe { slice::from_raw_parts(items.cast::<T>().as_ptr(), len) };
        T::hash_slice(items, hasher);
    }
    hash_items::<T>
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_of(value: impl Hash) -> u64 {
        let mut hasher = ChecksumHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn integers_hash_by_value() {
        // Each integer is one word, whatever its width or the byte order.
        let word = hash_of(7_u64);
        assert_eq!(hash_of(7_u8), word);
        assert_eq!(hash_of(7_u16), word);
        assert_eq!(hash_of(7_u32), word);
        assert_eq!(hash_of(7_usize), word);
        assert_eq!(hash_of(-1_isize), hash_of(-1_i64));
        assert_ne!(hash_of(0_u64), hash_of(()));
    }

    #[test]
    fn byte_strings_hash_their_length() {
        assert_ne!(
            hash_of([1_u8, 2].as_slice()),
            hash_of([1_u8, 2, 0].as_slice())
        );
        assert_ne!(hash_of("abcdefgh"), hash_of("abcdefgh\0"));
    }

    #[test]
    fn the_algorithm_does_not_change_by_accident() {
        // Peers compare checksums, so a change here breaks compatibility. CI
        // runs this on big-endian and 32-bit targets too.
        let mut hasher = ChecksumHasher::new();
        hasher.write_u32(42);
        hasher.write(b"petek");
        hasher.write_i64(-5);
        assert_eq!(hasher.finish(), 0x3988_7aee_e10b_9846);
    }

    #[test]
    fn slices_hash_like_their_items() {
        #[derive(Hash)]
        struct Item(u32, i16);

        let items = [Item(1, -1), Item(2, -2)];
        let mut hasher = ChecksumHasher::new();
        // SAFETY: the pointer is to two `Item`s, which nothing writes.
        unsafe { hash_fn_of::<Item>()(NonNull::from(&items).cast(), 2, &mut hasher) };

        let mut expected = ChecksumHasher::new();
        for item in &items {
            item.hash(&mut expected);
        }
        assert_eq!(hasher.finish(), expected.finish());
    }
}
