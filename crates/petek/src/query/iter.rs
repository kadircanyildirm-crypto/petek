//! Iteration over the entities a query matches.

use std::fmt;
use std::iter::FusedIterator;
use std::ops::Range;
use std::slice;

use super::{QueryData, QueryFilter};
use crate::archetype::{Archetype, ArchetypeId, Archetypes};

/// The archetypes a query iterator has left to visit.
pub(crate) enum Matched<'w, 's> {
    /// Archetypes that are each still to be checked against the query.
    Scan(slice::Iter<'w, Archetype>),
    /// Archetypes already known to match the query, listed by a
    /// [`QueryState`](super::QueryState).
    Cached(&'w Archetypes, slice::Iter<'s, ArchetypeId>),
}

/// An iterator over the items of a query, one per matching entity.
///
/// Entities are visited archetype by archetype, in no particular order. The
/// items borrow the world for `'w`; a [`QueryState`](super::QueryState) that
/// the iterator came from is only borrowed for `'s`, while iterating.
///
/// The number of items left is known exactly, but computing it takes time
/// proportional to the number of archetypes left.
pub struct QueryIter<'w, 's, D: QueryData, F: QueryFilter = ()> {
    /// `None` if the query cannot match anything, because some of its
    /// components are not registered.
    state: Option<(D::State, F::State)>,
    archetypes: Matched<'w, 's>,
    /// The archetype being visited, and its rows that are left.
    current: Option<(D::Fetch<'w>, Range<usize>)>,
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
    ///   [`check_access`](super::check_access)).
    pub(crate) unsafe fn new(
        state: Option<(D::State, F::State)>,
        archetypes: Matched<'w, 's>,
    ) -> Self {
        Self {
            state,
            archetypes,
            current: None,
        }
    }

    /// Moves on to the next matching archetype that has entities, and
    /// prepares to fetch from it.
    fn next_archetype(&mut self) -> Option<(D::Fetch<'w>, Range<usize>)> {
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
        let fetch = unsafe { D::fetch(data, archetype) };
        Some((fetch, 0..archetype.len()))
    }
}

impl<'w, D: QueryData, F: QueryFilter> Iterator for QueryIter<'w, '_, D, F> {
    type Item = D::Item<'w>;

    #[inline]
    fn next(&mut self) -> Option<D::Item<'w>> {
        loop {
            if let Some((fetch, rows)) = &mut self.current {
                if let Some(row) = rows.next() {
                    // SAFETY: the row is in bounds of the archetype `fetch` was
                    // prepared for, the world cannot change for `'w`, each row
                    // is visited once, and `new`'s caller holds the access.
                    return Some(unsafe { D::item(fetch, row) });
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
        while let Some((fetch, rows)) = current {
            for row in rows {
                // SAFETY: as in `next`.
                acc = f(acc, unsafe { D::item(&fetch, row) });
            }
            current = self.next_archetype();
        }
        acc
    }

    /// The exact number of items left, computed from the archetype sizes in
    /// time proportional to the number of archetypes.
    fn size_hint(&self) -> (usize, Option<usize>) {
        let current = self.current.as_ref().map_or(0, |(_, rows)| rows.len());
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
        let len = current + rest;
        (len, Some(len))
    }

    /// Counts the items from the archetype sizes, without visiting them.
    fn count(self) -> usize {
        self.size_hint().0
    }
}

impl<D: QueryData, F: QueryFilter> ExactSizeIterator for QueryIter<'_, '_, D, F> {}

impl<D: QueryData, F: QueryFilter> FusedIterator for QueryIter<'_, '_, D, F> {}

impl<D: QueryData, F: QueryFilter> fmt::Debug for QueryIter<'_, '_, D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryIter")
            .field("remaining", &self.size_hint().0)
            .finish_non_exhaustive()
    }
}
