//! Iteration over the entities a query matches.

use std::fmt;
use std::iter::FusedIterator;
use std::ops::Range;
use std::slice;

use super::{ArchetypeFilter, QueryData, QueryFilter};
use crate::archetype::{Archetype, ArchetypeId, Archetypes};
use crate::change::Ticks;

/// The archetypes a query iterator has left to visit.
pub(crate) enum Matched<'w, 's> {
    /// Archetypes that are each still to be checked against the query.
    Scan(slice::Iter<'w, Archetype>),
    /// Archetypes already known to match the query, listed by a
    /// [`QueryState`](super::QueryState).
    Cached(&'w Archetypes, slice::Iter<'s, ArchetypeId>),
}

/// The archetype a query iterator is visiting.
struct Current<'w, D: QueryData, F: QueryFilter> {
    data: D::Fetch<'w>,
    filter: F::Fetch<'w>,
    /// The rows left to visit.
    rows: Range<usize>,
}

/// An iterator over the items of a query, one per matching entity.
///
/// Entities are visited archetype by archetype, in an order that only depends
/// on the history of the world. The items borrow the world for `'w`; a
/// [`QueryState`](super::QueryState) that the iterator came from is only
/// borrowed for `'s`, while iterating.
///
/// Unless the query uses [`Added`](super::Added) or
/// [`Changed`](super::Changed), the number of items left is known exactly,
/// though computing it takes time proportional to the number of archetypes
/// left.
///
/// [`for_each`](Iterator::for_each), and adapters that use
/// [`fold`](Iterator::fold), such as `sum`, run faster than a `for` loop:
/// they visit each archetype in a tight loop of their own, while a `for`
/// loop calls [`next`](Iterator::next) for each entity, which keeps its
/// place in memory between calls. On a single archetype, the `petek`
/// benchmarks measure about a third more time for a `for` loop.
pub struct QueryIter<'w, 's, D: QueryData, F: QueryFilter = ()> {
    /// `None` if the query cannot match anything, because some of its
    /// components are not registered.
    state: Option<(D::State, F::State)>,
    archetypes: Matched<'w, 's>,
    ticks: Ticks,
    current: Option<Current<'w, D, F>>,
}

impl<'w, 's, D: QueryData, F: QueryFilter> QueryIter<'w, 's, D, F> {
    /// Creates an iterator over the matching entities of `archetypes`.
    ///
    /// # Safety
    ///
    /// - The state, if any, must come from the world that the archetypes
    ///   belong to, and `Matched::Cached` must only list archetypes that
    ///   match it, each at most once.
    /// - For `'w`, the caller must hold the access that `D` reports, and `D`
    ///   must not access any component mutably more than once (see
    ///   [`check_access`](super::check_access)). No one else may write the
    ///   ticks that `F` reads.
    pub(crate) unsafe fn new(
        state: Option<(D::State, F::State)>,
        archetypes: Matched<'w, 's>,
        ticks: Ticks,
    ) -> Self {
        Self {
            state,
            archetypes,
            ticks,
            current: None,
        }
    }

    /// Moves on to the next matching archetype that has entities, and
    /// prepares to fetch from it.
    fn next_archetype(&mut self) -> Option<Current<'w, D, F>> {
        let (data, filter) = self.state.as_ref()?;
        let matches = |archetype: &&Archetype| {
            archetype.len() > 0 && D::matches(data, archetype) && F::matches(filter, archetype)
        };
        let archetype = match &mut self.archetypes {
            Matched::Scan(archetypes) => archetypes.find(matches)?,
            Matched::Cached(archetypes, ids) => {
                let archetypes: &'w Archetypes = archetypes;
                ids.map(|&id| archetypes.get(id))
                    .find(|archetype| archetype.len() > 0)?
            }
        };
        // SAFETY: the archetype matches, and `new`'s caller guarantees that
        // the state comes from its world.
        let (data, filter) = unsafe {
            (
                D::fetch(data, archetype, self.ticks),
                F::fetch(filter, archetype, self.ticks),
            )
        };
        Some(Current {
            data,
            filter,
            rows: 0..archetype.len(),
        })
    }

    /// The number of rows left in the matching archetypes, whether or not the
    /// filter lets them through.
    fn rows_left(&self) -> usize {
        let current = self
            .current
            .as_ref()
            .map_or(0, |current| current.rows.len());
        let rest: usize = match (&self.state, &self.archetypes) {
            (None, _) => 0,
            (Some((data, filter)), Matched::Scan(archetypes)) => archetypes
                .clone()
                .filter(|archetype| D::matches(data, archetype) && F::matches(filter, archetype))
                .map(Archetype::len)
                .sum(),
            (Some(_), Matched::Cached(archetypes, ids)) => {
                ids.clone().map(|&id| archetypes.get(id).len()).sum()
            }
        };
        current + rest
    }
}

impl<'w, D: QueryData, F: QueryFilter> Iterator for QueryIter<'w, '_, D, F> {
    type Item = D::Item<'w>;

    #[inline]
    fn next(&mut self) -> Option<D::Item<'w>> {
        loop {
            if let Some(current) = &mut self.current {
                for row in current.rows.by_ref() {
                    // SAFETY: the row is in bounds of the archetype the
                    // fetches were prepared for, the world cannot change for
                    // `'w`, and `new`'s caller rules out writes to the ticks.
                    if F::IS_ARCHETYPAL || unsafe { F::filter_row(&current.filter, row) } {
                        // SAFETY: as above, each row is visited once, and
                        // `new`'s caller holds the access.
                        return Some(unsafe { D::item(&current.data, row) });
                    }
                }
            }
            self.current = Some(self.next_archetype()?);
        }
    }

    /// Visits each archetype in a loop of its own, which optimizes better than
    /// repeated calls to `next`. `for_each` and most adapters use this.
    fn fold<B, G>(mut self, init: B, mut f: G) -> B
    where
        G: FnMut(B, D::Item<'w>) -> B,
    {
        let mut acc = init;
        let mut current = self.current.take().or_else(|| self.next_archetype());
        while let Some(Current { data, filter, rows }) = current {
            for row in rows {
                // SAFETY: as in `next`.
                if F::IS_ARCHETYPAL || unsafe { F::filter_row(&filter, row) } {
                    // SAFETY: as in `next`.
                    acc = f(acc, unsafe { D::item(&data, row) });
                }
            }
            current = self.next_archetype();
        }
        acc
    }

    /// Exact unless the filter checks single rows, computed from the
    /// archetype sizes in time proportional to the number of archetypes.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let rows = self.rows_left();
        (if F::IS_ARCHETYPAL { rows } else { 0 }, Some(rows))
    }

    /// Counts the items, from the archetype sizes alone unless the filter
    /// checks single rows.
    fn count(self) -> usize {
        if F::IS_ARCHETYPAL {
            self.rows_left()
        } else {
            self.fold(0, |count, _| count + 1)
        }
    }
}

impl<D: QueryData, F: ArchetypeFilter> ExactSizeIterator for QueryIter<'_, '_, D, F> {}

impl<D: QueryData, F: QueryFilter> FusedIterator for QueryIter<'_, '_, D, F> {}

impl<D: QueryData, F: QueryFilter> fmt::Debug for QueryIter<'_, '_, D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryIter")
            .field("rows_left", &self.rows_left())
            .finish_non_exhaustive()
    }
}
