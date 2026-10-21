//! Archetypes: tables that store every entity with exactly the same set of
//! components.

use std::collections::HashMap;

use crate::bundle::{Bundle, BundleId};
use crate::component::{ComponentId, Components};
use crate::entity::Entity;
use crate::storage::blob_vec::BlobVec;

/// Identifies an archetype within one world.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct ArchetypeId(u32);

impl ArchetypeId {
    /// The archetype of entities that have no components.
    pub(crate) const EMPTY: Self = Self(0);

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
/// this briefly, but restore it before any user code (a component's
/// destructor) can run.
pub(crate) struct Archetype {
    /// The components of this archetype, sorted.
    component_ids: Box<[ComponentId]>,
    /// One column per component, in the same order as `component_ids`.
    columns: Box<[BlobVec]>,
    /// The entity in each row.
    entities: Vec<Entity>,
    /// Cached targets of inserting a bundle into an entity of this archetype.
    insert_edges: HashMap<BundleId, ArchetypeId>,
}

// SAFETY: columns are only ever created for components, which are
// `Send + Sync`; everything else in an archetype is plain data.
unsafe impl Send for Archetype {}
// SAFETY: as above.
unsafe impl Sync for Archetype {}

impl Archetype {
    fn new(component_ids: Box<[ComponentId]>, components: &Components) -> Self {
        let columns = component_ids
            .iter()
            .map(|&id| {
                let info = components.info(id);
                BlobVec::new(info.layout(), info.drop_fn())
            })
            .collect();
        Self {
            component_ids,
            columns,
            entities: Vec::new(),
            insert_edges: HashMap::new(),
        }
    }

    /// The number of entities (rows).
    pub(crate) fn len(&self) -> usize {
        self.entities.len()
    }

    /// The index of the column that stores component `id`, if there is one.
    pub(crate) fn column_index(&self, id: ComponentId) -> Option<usize> {
        self.component_ids.binary_search(&id).ok()
    }

    pub(crate) fn column(&self, index: usize) -> &BlobVec {
        &self.columns[index]
    }

    /// Makes room for at least `additional` more rows.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.entities.reserve(additional);
        for column in &mut self.columns {
            column.reserve(additional);
        }
    }

    /// Pushes the components of `bundle` onto their columns, as part of a new
    /// row.
    ///
    /// # Safety
    ///
    /// - `component_ids` must list the components of `B` in bundle order, and
    ///   each must have a column here.
    /// - The caller must complete the row before anything else accesses this
    ///   archetype.
    pub(crate) unsafe fn push_bundle<B: Bundle>(
        &mut self,
        bundle: B,
        component_ids: &[ComponentId],
    ) {
        let mut ids = component_ids.iter();
        bundle.take_components(&mut |value| {
            let id = ids
                .next()
                .expect("bundle has more components than it reported");
            let column = self
                .column_index(*id)
                .expect("archetype lacks a bundle component");
            // SAFETY: the column stores this component, and `take_components`
            // hands over a valid value that is now ours.
            unsafe { self.columns[column].push(value) };
        });
    }

    /// Appends `entity`, completing the last row.
    ///
    /// # Safety
    ///
    /// Every column must already hold the new row's component.
    pub(crate) unsafe fn push_entity(&mut self, entity: Entity) {
        self.entities.push(entity);
    }

    /// Removes `row` from the entity list by moving the last entity into its
    /// place, and returns that moved entity, if any.
    ///
    /// # Safety
    ///
    /// `row` must be less than `len()`, and the caller must remove `row` from
    /// every column before anything else accesses this archetype.
    pub(crate) unsafe fn swap_remove_entity(&mut self, row: usize) -> Option<Entity> {
        self.entities.swap_remove(row);
        self.entities.get(row).copied()
    }

    /// Removes `row` from the columns of every component `filter` selects,
    /// dropping the values.
    ///
    /// Since destructors run here, this must be the last step of a structural
    /// change. If one panics, `row` is still removed from the remaining
    /// columns (whose values are leaked), so the archetype stays consistent.
    ///
    /// # Safety
    ///
    /// `row` must be in bounds of every selected column.
    pub(crate) unsafe fn drop_from_columns(
        &mut self,
        row: usize,
        filter: impl Fn(ComponentId) -> bool,
    ) {
        /// Finishes removing the row if a destructor unwinds.
        struct Guard<'a, F: Fn(ComponentId) -> bool> {
            component_ids: &'a [ComponentId],
            columns: &'a mut [BlobVec],
            filter: F,
            row: usize,
            next: usize,
        }

        impl<F: Fn(ComponentId) -> bool> Drop for Guard<'_, F> {
            fn drop(&mut self) {
                for index in self.next..self.columns.len() {
                    if (self.filter)(self.component_ids[index]) {
                        // SAFETY: the caller of `drop_from_columns`
                        // guarantees `row` is in bounds of this column.
                        unsafe { self.columns[index].swap_remove_and_forget_unchecked(self.row) };
                    }
                }
            }
        }

        let mut guard = Guard {
            component_ids: &self.component_ids,
            columns: &mut self.columns,
            filter,
            row,
            next: 0,
        };
        while guard.next < guard.columns.len() {
            let index = guard.next;
            guard.next += 1;
            if (guard.filter)(guard.component_ids[index]) {
                // SAFETY: the caller guarantees `row` is in bounds of this
                // column.
                unsafe { guard.columns[index].swap_remove_and_drop_unchecked(row) };
            }
        }
    }
}

/// All archetypes of a world.
pub(crate) struct Archetypes {
    archetypes: Vec<Archetype>,
    by_components: HashMap<Box<[ComponentId]>, ArchetypeId>,
}

impl Archetypes {
    /// Creates the collection with just the empty archetype.
    pub(crate) fn new() -> Self {
        let empty = Archetype::new(Box::default(), &Components::default());
        Self {
            archetypes: vec![empty],
            by_components: HashMap::from([(Box::default(), ArchetypeId::EMPTY)]),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.archetypes.len()
    }

    pub(crate) fn get(&self, id: ArchetypeId) -> &Archetype {
        &self.archetypes[id.index()]
    }

    pub(crate) fn get_mut(&mut self, id: ArchetypeId) -> &mut Archetype {
        &mut self.archetypes[id.index()]
    }

    /// Returns the archetype an entity of `src` ends up in when `bundle`, with
    /// the given sorted component ids, is inserted into it.
    pub(crate) fn insert_target(
        &mut self,
        src: ArchetypeId,
        bundle: BundleId,
        bundle_ids: &[ComponentId],
        components: &Components,
    ) -> ArchetypeId {
        if let Some(&target) = self.get(src).insert_edges.get(&bundle) {
            return target;
        }
        let mut ids = self.get(src).component_ids.to_vec();
        ids.extend_from_slice(bundle_ids);
        ids.sort_unstable();
        ids.dedup();
        let target = self.get_or_insert(ids, components);
        self.get_mut(src).insert_edges.insert(bundle, target);
        target
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
        let id = ArchetypeId(u32::try_from(self.archetypes.len()).expect("too many archetypes"));
        let component_ids: Box<[ComponentId]> = component_ids.into();
        self.archetypes
            .push(Archetype::new(component_ids.clone(), components));
        self.by_components.insert(component_ids, id);
        id
    }
}
