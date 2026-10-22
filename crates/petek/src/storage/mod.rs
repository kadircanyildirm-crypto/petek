//! Type-erased storage for component data.

pub(crate) mod blob_vec;
pub(crate) mod column;

use std::ops::Range;

/// Calls `drop_one` for every index in `range`, the way the elements of a
/// slice are dropped: if one call panics, the remaining indices are still
/// visited while unwinding, and a second panic aborts the process.
pub(crate) fn drop_each(range: Range<usize>, drop_one: impl FnMut(usize)) {
    struct Rest<F: FnMut(usize)> {
        range: Range<usize>,
        drop_one: F,
    }

    impl<F: FnMut(usize)> Drop for Rest<F> {
        fn drop(&mut self) {
            // Indices are only left over if a call below panicked.
            self.range.by_ref().for_each(&mut self.drop_one);
        }
    }

    let mut rest = Rest { range, drop_one };
    for index in rest.range.by_ref() {
        (rest.drop_one)(index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};

    #[test]
    fn drop_each_visits_every_index_even_after_a_panic() {
        let mut visited = Vec::new();
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            drop_each(0..5, |index| {
                visited.push(index);
                assert_ne!(index, 1, "boom");
            });
        }));
        assert!(result.is_err());
        assert_eq!(visited, [0, 1, 2, 3, 4]);
    }
}
