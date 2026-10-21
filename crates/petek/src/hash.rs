//! A fast hasher for the world's internal lookup tables.
//!
//! Their keys are `TypeId`s and small integer ids, which are never chosen by
//! an attacker, so the standard library's DoS-resistant `SipHash` would only
//! cost time. This is the multiply-and-rotate scheme of rustc's `FxHasher`.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

/// A `HashMap` using [`FastHasher`].
pub(crate) type FastHashMap<K, V> = HashMap<K, V, BuildHasherDefault<FastHasher>>;

/// A fast, non-cryptographic hasher. See the module documentation.
#[derive(Default)]
pub(crate) struct FastHasher {
    hash: u64,
}

impl FastHasher {
    const SEED: u64 = 0x51_7c_c1_b7_27_22_0a_95;

    #[inline]
    fn add(&mut self, word: u64) {
        self.hash = (self.hash.rotate_left(5) ^ word).wrapping_mul(Self::SEED);
    }
}

impl Hasher for FastHasher {
    #[inline]
    fn write(&mut self, bytes: &[u8]) {
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

    #[inline]
    fn write_u8(&mut self, n: u8) {
        self.add(n.into());
    }

    #[inline]
    fn write_u32(&mut self, n: u32) {
        self.add(n.into());
    }

    #[inline]
    fn write_u64(&mut self, n: u64) {
        self.add(n);
    }

    #[inline]
    fn write_usize(&mut self, n: usize) {
        self.add(n as u64);
    }

    #[inline]
    fn finish(&self) -> u64 {
        self.hash
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;
    use std::hash::BuildHasher;

    fn hash_of(value: impl std::hash::Hash) -> u64 {
        BuildHasherDefault::<FastHasher>::default().hash_one(value)
    }

    #[test]
    fn distinguishes_small_ids_and_type_ids() {
        let ids: Vec<u64> = (0..1000_u32).map(hash_of).collect();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len());

        assert_ne!(hash_of(TypeId::of::<u32>()), hash_of(TypeId::of::<i32>()));
    }

    #[test]
    fn byte_writes_cover_every_byte() {
        assert_ne!(hash_of([1_u8; 9].as_slice()), hash_of([1_u8; 8].as_slice()));
        assert_ne!(hash_of("abcdefghi"), hash_of("abcdefghj"));
    }
}
