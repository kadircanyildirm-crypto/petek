//! Archetypes: tables that store every entity with exactly the same set of
//! components, and the cached transitions between them.

use std::fmt;
use std::ptr::NonNull;
use std::slice;

use crate::bundle::{Bundle, BundleId};
use crate::change::Tick;
use crate::component::{ComponentId, Components};
use crate::entity::Entity;
use crate::hash::FastHashMap;
use crate::storage::column::Column;
use crate::storage::drop_each;

/// Identifies an archetype within one world.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ArchetypeId(u32);

impl ArchetypeId {
    /// The archetype of entities that have no components.
    pub(crate) const EMPTY: Self = Self(0);

    #[inline]
    fn index(self) -> usize {
        self.0 as usize
    }
}

/// A table holding every entity that has one particular set of components.
///
/// Each component gets a column and each entity a row, so iterating over a
/// component touches contiguous memory.
///
/// Every column holds exactly one item per entity. Structural changes break
/// this briefly, but restore it before any user code can run.
pub struct Archetype {
    /// The components of this archetype, sorted.
    component_ids: Box<[ComponentId]>,
    /// One column per component, in the same order as `component_ids`.
    columns: Box<[Column]>,
    /// The entity in each row.
    entities: Vec<Entity>,
    /// Transitions taken when a bundle is inserted into one of the entities.
    insert_edges: FastHashMap<BundleId, TransitionId>,
    /// Transitions taken when a bundle is removed from one of the entities;
    /// `None` if this archetype lacks some of the bundle's components.
    remove_edges: FastHashMap<BundleId, Option<TransitionId>>,
}

// SAFETY: columns are only ever built from `ComponentInfo`s, which only
// describe components, and components are `Send + Sync`. The change ticks are
// in `UnsafeCell`s, which queries only write while they have exclusive access
// to the component (see `TickSlice`). Everything else in an archetype is plain
// data.
unsafe impl Send for Archetype {}
// SAFETY: as above.
unsafe impl Sync for Archetype {}

impl Archetype {
    fn new(component_ids: Box<[ComponentId]>, components: &Components) -> Self {
        let columns = component_ids
            .iter()
            .map(|&id| Column::new(components.info(id)))
            .collect();
        Self {
            component_ids,
            columns,
            entities: Vec::new(),
            insert_edges: FastHashMap::default(),
            remove_edges: FastHashMap::default(),
        }
    }

    /// The number of entities (rows).
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }

    /// The entity in each row.
    #[inline]
    pub(crate) fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// Returns `true` if this archetype has component `id`.
    #[inline]
    pub(crate) fn contains(&self, id: ComponentId) -> bool {
        self.column_index(id).is_some()
    }

    /// The index of the column that stores component `id`, if there is one.
    #[inline]
    pub(crate) fn column_index(&self, id: ComponentId) -> Option<usize> {
        self.component_ids.binary_search(&id).ok()
    }

    #[inline]
    pub(crate) fn column(&self, index: usize) -> &Column {
        &self.columns[index]
    }

    #[inline]
    pub(crate) fn column_mut(&mut self, index: usize) -> &mut Column {
        &mut self.columns[index]
    }

    /// Checks, in debug builds, that every column holds one item per entity.
    pub(crate) fn debug_assert_consistent(&self) {
        debug_assert!(
            self.columns
                .iter()
                .all(|column| column.len() == self.entities.len()),
            "archetype columns are out of sync with its entities",
        );
        for column in &self.columns {
            column.debug_assert_consistent();
        }
    }

    /// Makes room for at least `additional` more rows.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.entities.reserve(additional);
        for column in &mut self.columns {
            column.reserve(additional);
        }
    }

    /// Moves the components of `bundle` into the spare slots of their
    /// columns, without counting them yet (see [`Column::write_spare`]).
    ///
    /// Nothing about the archetype changes until
    /// [`commit_staged`](Self::commit_staged) is called, so if the bundle
    /// panics partway, the staged values are merely leaked.
    ///
    /// # Panics
    ///
    /// If the bundle hands over a different number of components than
    /// `columns` lists, which only a broken `Bundle` implementation can do.
    ///
    /// # Safety
    ///
    /// - `columns` must list, in bundle order, the distinct columns that
    ///   store the components of `B`.
    /// - Each of those columns must have spare capacity.
    pub(crate) unsafe fn stage_bundle<B: Bundle>(&mut self, bundle: B, columns: &[usize]) {
        let mut columns = columns.iter();
        bundle.take_components(&mut |value| {
            let &column = columns
                .next()
                .expect("bundle has more components than it reported");
            // SAFETY: the column stores this component and has room, and
            // `take_components` hands over a valid value that is now ours.
            unsafe { self.columns[column].write_spare(value) };
        });
        assert!(
            columns.next().is_none(),
            "bundle has fewer components than it reported",
        );
    }

    /// Counts the components previously staged in `columns`.
    ///
    /// # Safety
    ///
    /// [`stage_bundle`](Self::stage_bundle) must have staged a value in each
    /// of these columns since they last changed.
    ///
    /// The values count as added and changed at `tick`. This cannot panic
    /// once the archetype has [reserved](Self::reserve) room for them.
    pub(crate) unsafe fn commit_staged(&mut self, columns: &[usize], tick: Tick) {
        for &column in columns {
            // SAFETY: the caller guarantees a value was staged here and that
            // there is room.
            unsafe { self.columns[column].commit_spare(tick) };
        }
    }

    /// Appends `entity`, completing the last row.
    ///
    /// # Safety
    ///
    /// Every column must already hold the new row's component.
    pub(crate) unsafe fn push_entity(&mut self, entity: Entity) {
        self.entities.push(entity);
    }

    /// Overwrites components of `row` with those of `bundle`, dropping the
    /// old values and marking the new ones as changed at `tick`.
    ///
    /// Each column stays consistent even if a destructor or the bundle
    /// panics.
    ///
    /// # Safety
    ///
    /// - `row` must be less than `len()`.
    /// - `columns` must list, in bundle order, the distinct columns that
    ///   store the components of `B`.
    pub(crate) unsafe fn replace_bundle<B: Bundle>(
        &mut self,
        row: usize,
        bundle: B,
        columns: &[usize],
        tick: Tick,
    ) {
        debug_assert!(row < self.len());
        let mut columns = columns.iter();
        bundle.take_components(&mut |value| {
            let &column = columns
                .next()
                .expect("bundle has more components than it reported");
            // SAFETY: `row` is in bounds, the column stores this component,
            // and `take_components` hands over a valid value.
            unsafe { self.columns[column].replace_unchecked(row, value, tick) };
        });
    }

    /// Moves the entity at `row` to a new row at the end of `dst`, along with
    /// the components listed in `moves` as `(column here, column in dst)`.
    ///
    /// This runs no user code, but leaves both archetypes incomplete: `row` is
    /// still in every column of `self` not listed in `moves`, and the new row
    /// of `dst` lacks every component not listed there. The caller must
    /// complete both before anything else accesses either archetype.
    ///
    /// Returns the entity that moved into `row` in `self`, if any.
    ///
    /// # Safety
    ///
    /// - `row` must be less than `len()`.
    /// - Each pair must map a column here to a column of `dst` that stores
    ///   the same component; the columns must be distinct.
    /// - `dst` must have room for one more row (see
    ///   [`reserve`](Self::reserve)).
    pub(crate) unsafe fn move_row_to(
        &mut self,
        row: usize,
        dst: &mut Archetype,
        moves: &[(usize, usize)],
    ) -> Option<Entity> {
        debug_assert!(row < self.len());
        for &(src_column, dst_column) in moves {
            // SAFETY: `row` is in bounds, and the caller guarantees the
            // destination column stores the same type and has room.
            unsafe {
                self.columns[src_column]
                    .swap_remove_into_unchecked(row, &mut dst.columns[dst_column]);
            }
        }
        let entity = self.entities.swap_remove(row);
        dst.entities.push(entity);
        self.entities.get(row).copied()
    }

    /// Removes `row` from the entity list by moving the last entity into its
    /// place, and returns that moved entity, if any.
    ///
    /// # Safety
    ///
    /// `row` must be less than `len()`, and the caller must remove `row` from
    /// every column before anything else accesses this archetype.
    pub(crate) unsafe fn swap_remove_entity(&mut self, row: usize) -> Option<Entity> {
        debug_assert!(row < self.len());
        self.entities.swap_remove(row);
        self.entities.get(row).copied()
    }

    /// Removes `row` from every column, dropping its components.
    ///
    /// Since destructors run here, this must be the last step of a structural
    /// change. If one panics, `row` is still removed from the other columns
    /// and their values dropped, so the archetype stays consistent.
    ///
    /// # Safety
    ///
    /// `row` must be in bounds of every column.
    pub(crate) unsafe fn drop_row(&mut self, row: usize) {
        let columns = &mut self.columns;
        drop_each(0..columns.len(), |index| {
            // SAFETY: the caller guarantees `row` is in bounds of this column.
            unsafe { columns[index].swap_remove_and_drop_unchecked(row) };
        });
    }

    /// Removes `row` from the columns that a [`Transition`] lists as
    /// replaced, dropping those components. Like [`drop_row`](Self::drop_row),
    /// it finishes even if a destructor panics.
    ///
    /// # Safety
    ///
    /// `row` must be in bounds of each of these columns, and the columns must
    /// be distinct.
    pub(crate) unsafe fn drop_replaced(&mut self, row: usize, replaced: &[(usize, usize)]) {
        let all = &mut self.columns;
        drop_each(0..replaced.len(), |index| {
            // SAFETY: the caller guarantees `row` is in bounds of this column.
            unsafe { all[replaced[index].0].swap_remove_and_drop_unchecked(row) };
        });
    }

    /// Removes `row` from the given columns, moving their values into a new
    /// `B`.
    ///
    /// If building the bundle panics, `row` is still removed from the columns
    /// it did not get to, leaking those values, so the archetype stays
    /// consistent.
    ///
    /// # Safety
    ///
    /// - `row` must be in bounds of each of these columns.
    /// - `columns` must list, in bundle order, the distinct columns that
    ///   store the components of `B`.
    pub(crate) unsafe fn take_bundle<B: Bundle>(&mut self, row: usize, columns: &[usize]) -> B {
        /// Removes `row` from the columns that are left when dropped.
        struct Take<'a> {
            columns: &'a mut [Column],
            left: slice::Iter<'a, usize>,
            row: usize,
        }

        impl Take<'_> {
            fn next(&mut self) -> NonNull<u8> {
                let &column = self
                    .left
                    .next()
                    .expect("bundle has more components than it reported");
                // SAFETY: `take_bundle`'s caller guarantees `row` is in bounds
                // of this column. The returned value stays valid until the
                // column changes again, and each column is visited once.
                unsafe { self.columns[column].swap_remove_and_forget_unchecked(self.row) }
            }
        }

        impl Drop for Take<'_> {
            fn drop(&mut self) {
                for &column in &mut self.left {
                    // SAFETY: as in `next`.
                    unsafe { self.columns[column].swap_remove_and_forget_unchecked(self.row) };
                }
            }
        }

        let mut take = Take {
            columns: &mut self.columns,
            left: columns.iter(),
            row,
        };
        // SAFETY: `take.next` yields valid values of `B`'s components in
        // bundle order, handing over ownership of each.
        unsafe { B::from_components(&mut || take.next()) }
    }

    /// Removes every entity, dropping all components.
    ///
    /// Like [`drop_row`](Self::drop_row), it finishes even if a destructor
    /// panics.
    pub(crate) fn clear(&mut self) {
        self.entities.clear();
        let columns = &mut self.columns;
        drop_each(0..columns.len(), |index| columns[index].clear());
    }
}

impl fmt::Debug for Archetype {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Archetype")
            .field("component_ids", &self.component_ids)
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

/// Identifies a cached [`Transition`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct TransitionId(u32);

/// A precomputed plan for moving an entity between archetypes when a bundle is
/// inserted into or removed from it.
///
/// Its column indices are worked out once, the first time the transition is
/// needed, so the structural change itself needs no lookups. No column is
/// listed twice.
pub(crate) struct Transition {
    pub(crate) src: ArchetypeId,
    pub(crate) dst: ArchetypeId,
    /// For each component of the bundle, in bundle order: its column in `dst`
    /// when inserting, or in `src` when removing.
    pub(crate) bundle_columns: Box<[usize]>,
    /// `(column in src, column in dst)` for each component that is carried
    /// over unchanged. Empty if `src == dst`.
    pub(crate) moved_columns: Box<[(usize, usize)]>,
    /// When inserting into a different archetype, `(column in src, column in
    /// dst)` for each component whose value the bundle replaces. Empty
    /// otherwise.
    pub(crate) replaced_columns: Box<[(usize, usize)]>,
}

/// All archetypes of a world, and the transitions between them.
pub(crate) struct Archetypes {
    /// Indexed by `ArchetypeId`.
    by_id: Vec<Archetype>,
    by_components: FastHashMap<Box<[ComponentId]>, ArchetypeId>,
    transitions: Vec<Transition>,
}

impl Archetypes {
    /// Creates the collection with just the empty archetype.
    pub(crate) fn new() -> Self {
        let empty = Archetype::new(Box::default(), &Components::new());
        Self {
            by_id: vec![empty],
            by_components: [(Box::default(), ArchetypeId::EMPTY)].into_iter().collect(),
            transitions: Vec::new(),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Iterates over all archetypes, in order of their ids.
    #[inline]
    pub(crate) fn iter(&self) -> slice::Iter<'_, Archetype> {
        self.by_id.iter()
    }

    /// Iterates over the archetypes from the `start`-th on, with their ids.
    pub(crate) fn iter_from(
        &self,
        start: usize,
    ) -> impl Iterator<Item = (ArchetypeId, &Archetype)> + '_ {
        let archetypes = self.by_id.get(start..).unwrap_or_default();
        (start..).zip(archetypes).map(|(index, archetype)| {
            let id = u32::try_from(index).expect("archetype ids fit in a u32");
            (ArchetypeId(id), archetype)
        })
    }

    #[inline]
    pub(crate) fn get(&self, id: ArchetypeId) -> &Archetype {
        &self.by_id[id.index()]
    }

    #[inline]
    pub(crate) fn get_mut(&mut self, id: ArchetypeId) -> &mut Archetype {
        &mut self.by_id[id.index()]
    }

    /// Returns a transition with its source archetype and, unless it is the
    /// same, its target archetype.
    #[inline]
    pub(crate) fn split(
        &mut self,
        id: TransitionId,
    ) -> (&Transition, &mut Archetype, Option<&mut Archetype>) {
        let transition = &self.transitions[id.0 as usize];
        let (src, dst) = (transition.src.index(), transition.dst.index());
        if src == dst {
            return (transition, &mut self.by_id[src], None);
        }
        let (src, dst) = get_two_mut(&mut self.by_id, src, dst);
        (transition, src, Some(dst))
    }

    /// Returns the transition for inserting a bundle, with the given distinct
    /// components in bundle order, into an entity of `src`.
    pub(crate) fn insert_transition(
        &mut self,
        src: ArchetypeId,
        bundle: BundleId,
        bundle_ids: &[ComponentId],
        components: &Components,
    ) -> TransitionId {
        if let Some(&id) = self.get(src).insert_edges.get(&bundle) {
            return id;
        }
        let mut ids: Vec<_> = self
            .get(src)
            .component_ids
            .iter()
            .chain(bundle_ids)
            .copied()
            .collect();
        ids.sort_unstable();
        ids.dedup();
        let dst = self.get_or_insert(ids, components);

        let (src_archetype, dst_archetype) = (self.get(src), self.get(dst));
        let column_in_dst = |id| {
            dst_archetype
                .column_index(id)
                .expect("target has every component")
        };
        let bundle_columns = bundle_ids.iter().map(|&id| column_in_dst(id)).collect();
        let (mut moved_columns, mut replaced_columns) = (Vec::new(), Vec::new());
        if src != dst {
            for (src_column, &id) in src_archetype.component_ids.iter().enumerate() {
                if bundle_ids.contains(&id) {
                    replaced_columns.push((src_column, column_in_dst(id)));
                } else {
                    moved_columns.push((src_column, column_in_dst(id)));
                }
            }
        }

        let id = self.push_transition(Transition {
            src,
            dst,
            bundle_columns,
            moved_columns: moved_columns.into(),
            replaced_columns: replaced_columns.into(),
        });
        self.get_mut(src).insert_edges.insert(bundle, id);
        id
    }

    /// Returns the transition for removing a bundle, with the given distinct
    /// components in bundle order, from an entity of `src`, or `None` if
    /// `src` lacks any of them.
    pub(crate) fn remove_transition(
        &mut self,
        src: ArchetypeId,
        bundle: BundleId,
        bundle_ids: &[ComponentId],
        components: &Components,
    ) -> Option<TransitionId> {
        if let Some(&id) = self.get(src).remove_edges.get(&bundle) {
            return id;
        }
        let src_archetype = self.get(src);
        let id = if bundle_ids
            .iter()
            .all(|&id| src_archetype.column_index(id).is_some())
        {
            let ids = src_archetype
                .component_ids
                .iter()
                .copied()
                .filter(|id| !bundle_ids.contains(id))
                .collect();
            let dst = self.get_or_insert(ids, components);

            let (src_archetype, dst_archetype) = (self.get(src), self.get(dst));
            let column_in_src = |id| {
                src_archetype
                    .column_index(id)
                    .expect("source has every component")
            };
            let bundle_columns = bundle_ids.iter().map(|&id| column_in_src(id)).collect();
            let moved_columns = if src == dst {
                Box::default()
            } else {
                dst_archetype
                    .component_ids
                    .iter()
                    .enumerate()
                    .map(|(dst_column, &id)| (column_in_src(id), dst_column))
                    .collect()
            };
            Some(self.push_transition(Transition {
                src,
                dst,
                bundle_columns,
                moved_columns,
                replaced_columns: Box::default(),
            }))
        } else {
            None
        };
        self.get_mut(src).remove_edges.insert(bundle, id);
        id
    }

    /// Removes every entity from every archetype, dropping all components.
    ///
    /// Like [`Archetype::drop_row`], it finishes even if a destructor panics.
    pub(crate) fn clear_entities(&mut self) {
        let archetypes = &mut self.by_id;
        drop_each(0..archetypes.len(), |index| archetypes[index].clear());
    }

    fn push_transition(&mut self, transition: Transition) -> TransitionId {
        let id = TransitionId(u32::try_from(self.transitions.len()).expect("too many transitions"));
        self.transitions.push(transition);
        id
    }

    /// Returns the archetype with exactly these sorted components, creating it
    /// if needed.
    fn get_or_insert(
        &mut self,
        component_ids: Vec<ComponentId>,
        components: &Components,
    ) -> ArchetypeId {
        debug_assert!(component_ids.is_sorted());
        if let Some(&id) = self.by_components.get(component_ids.as_slice()) {
            return id;
        }
        let id = ArchetypeId(u32::try_from(self.by_id.len()).expect("too many archetypes"));
        let component_ids: Box<[ComponentId]> = component_ids.into();
        self.by_id
            .push(Archetype::new(component_ids.clone(), components));
        self.by_components.insert(component_ids, id);
        id
    }
}

/// Borrows two different elements of a slice mutably.
fn get_two_mut<T>(slice: &mut [T], a: usize, b: usize) -> (&mut T, &mut T) {
    assert_ne!(a, b, "cannot borrow an element twice");
    if a < b {
        let (left, right) = slice.split_at_mut(b);
        (&mut left[a], &mut right[0])
    } else {
        let (left, right) = slice.split_at_mut(a);
        (&mut right[0], &mut left[b])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::Bundles;
    use crate::component::Component;

    struct A;
    impl Component for A {}

    struct B;
    impl Component for B {}

    struct C;
    impl Component for C {}

    fn ids_of<T: Bundle>(
        bundles: &mut Bundles,
        components: &mut Components,
    ) -> (BundleId, Box<[ComponentId]>) {
        let id = bundles.register::<T>(components);
        (id, bundles.info(id).component_ids.clone())
    }

    #[test]
    fn transitions_are_computed_once_and_cached() {
        let (mut components, mut bundles) = (Components::new(), Bundles::default());
        let mut archetypes = Archetypes::new();
        let (ab, ab_ids) = ids_of::<(A, B)>(&mut bundles, &mut components);
        let (cb, cb_ids) = ids_of::<(C, B)>(&mut bundles, &mut components);

        let spawn = archetypes.insert_transition(ArchetypeId::EMPTY, ab, &ab_ids, &components);
        assert_eq!(
            archetypes.insert_transition(ArchetypeId::EMPTY, ab, &ab_ids, &components),
            spawn
        );
        let with_ab = archetypes.transitions[spawn.0 as usize].dst;

        // Inserting (C, B) into {A, B}: A moves, B is replaced, C is new.
        let insert = archetypes.insert_transition(with_ab, cb, &cb_ids, &components);
        let insert = &archetypes.transitions[insert.0 as usize];
        let with_abc = insert.dst;
        assert_eq!(archetypes.get(with_abc).component_ids.len(), 3);
        assert_eq!(&*insert.moved_columns, [(0, 0)]);
        assert_eq!(&*insert.replaced_columns, [(1, 1)]);
        assert_eq!(&*insert.bundle_columns, [2, 1]);

        // Removing (C, B) from {A, B} is impossible, but from {A, B, C} it
        // leaves {A}.
        assert_eq!(
            archetypes.remove_transition(with_ab, cb, &cb_ids, &components),
            None
        );
        let removal = archetypes
            .remove_transition(with_abc, cb, &cb_ids, &components)
            .unwrap();
        let removal = &archetypes.transitions[removal.0 as usize];
        assert_eq!(&*removal.bundle_columns, [2, 1]);
        assert_eq!(&*removal.moved_columns, [(0, 0)]);
        assert_eq!(archetypes.len(), 4);
    }

    #[test]
    fn get_two_mut_borrows_both_orders() {
        let mut items = [1, 2, 3];
        let (a, b) = get_two_mut(&mut items, 2, 0);
        std::mem::swap(a, b);
        assert_eq!(items, [3, 2, 1]);
    }
}
