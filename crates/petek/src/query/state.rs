//! Queries that remember which archetypes match them.

use std::any::type_name;
use std::fmt;

use super::iter::Matched;
use super::{
    Query, QueryData, QueryError, QueryFilter, QueryIter, ReadOnlyQueryData, check_access,
};
use crate::archetype::ArchetypeId;
use crate::entity::Entity;
use crate::world::{World, WorldId};

/// A query that remembers which archetypes match it.
///
/// [`World::query`] and [`World::query_mut`] check every archetype each time
/// they run. A `QueryState` checks each archetype once, the first time it runs
/// after the archetype was created, which suits queries that run over and
/// over, such as once per frame.
///
/// ```
/// use petek::{Component, World};
/// use petek::query::{QueryState, Without};
///
/// struct Position(f32);
/// impl Component for Position {}
///
/// struct Velocity(f32);
/// impl Component for Velocity {}
///
/// struct Frozen;
/// impl Component for Frozen {}
///
/// let mut world = World::new();
/// let mut movement =
///     QueryState::<(&mut Position, &Velocity), Without<Frozen>>::new(&mut world);
///
/// let ball = world.spawn((Position(0.0), Velocity(1.0)));
/// world.spawn((Position(0.0), Velocity(1.0), Frozen));
/// for _frame in 0..3 {
///     for (position, velocity) in movement.iter_mut(&mut world) {
///         position.0 += velocity.0;
///     }
/// }
/// assert_eq!(world.get::<Position>(ball).unwrap().0, 3.0);
/// ```
///
/// A `QueryState` belongs to the world it was created for, and using it with
/// another world panics.
pub struct QueryState<D: QueryData, F: QueryFilter = ()> {
    world: WorldId,
    data: D::State,
    filter: F::State,
    /// The matching archetypes among the first `checked` archetypes of the
    /// world.
    matched: Vec<ArchetypeId>,
    checked: usize,
}

impl<D: QueryData, F: QueryFilter> QueryState<D, F> {
    /// Creates the state of a query over `world`, registering the component
    /// types it needs.
    ///
    /// # Panics
    ///
    /// If `D` accesses a component mutably and also elsewhere, as
    /// [`World::query_mut`] does.
    #[must_use]
    #[track_caller]
    pub fn new(world: &mut World) -> Self {
        let data = D::init_state(world.components_mut());
        let filter = F::init_state(world.components_mut());
        check_access::<D>(&data, world.components());
        Self {
            world: world.id(),
            data,
            filter,
            matched: Vec::new(),
            checked: 0,
        }
    }

    /// Iterates over the items of every matching entity.
    ///
    /// # Panics
    ///
    /// If `world` is not the world this state was created for.
    #[track_caller]
    pub fn iter<'w>(&'w mut self, world: &'w World) -> QueryIter<'w, D, F>
    where
        D: ReadOnlyQueryData,
    {
        self.update(world);
        // SAFETY: `update` checked that the state comes from `world` and
        // brought `matched` up to date. `D` only reads, and `&World` rules out
        // writes for `'w`.
        unsafe { self.iter_unchecked(world) }
    }

    /// Iterates over the items of every matching entity, which may borrow
    /// components mutably.
    ///
    /// # Panics
    ///
    /// If `world` is not the world this state was created for.
    #[track_caller]
    pub fn iter_mut<'w>(&'w mut self, world: &'w mut World) -> QueryIter<'w, D, F> {
        self.update(world);
        // SAFETY: as in `iter`, except that `D` may write: `new` checked that
        // it accesses no component mutably more than once, and `&mut World`
        // gives it exclusive access for `'w`.
        unsafe { self.iter_unchecked(world) }
    }

    /// Returns the item of `entity`.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::NoSuchEntity`] if `entity` is not alive, or
    /// [`QueryError::DoesNotMatch`] if the query does not match it.
    ///
    /// # Panics
    ///
    /// If `world` is not the world this state was created for.
    #[track_caller]
    pub fn get<'w>(&self, world: &'w World, entity: Entity) -> Result<D::Item<'w>, QueryError>
    where
        D: ReadOnlyQueryData,
    {
        self.check_world(world);
        // SAFETY: the states come from `world`, `D` only reads, and `&World`
        // rules out writes for `'w`.
        unsafe { Query::<D, F>::new(world, Some(self.data), self.filter) }.get(entity)
    }

    /// Returns the item of `entity`, which may borrow components mutably.
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::NoSuchEntity`] if `entity` is not alive, or
    /// [`QueryError::DoesNotMatch`] if the query does not match it.
    ///
    /// # Panics
    ///
    /// If `world` is not the world this state was created for.
    #[track_caller]
    pub fn get_mut<'w>(
        &self,
        world: &'w mut World,
        entity: Entity,
    ) -> Result<D::Item<'w>, QueryError> {
        self.check_world(world);
        // SAFETY: the states come from `world`, `new` checked that `D`
        // accesses no component mutably more than once, and `&mut World`
        // gives it exclusive access for `'w`.
        unsafe { Query::<D, F>::new(world, Some(self.data), self.filter) }.get(entity)
    }

    /// Iterates over the matching entities, without checking anything.
    ///
    /// # Safety
    ///
    /// `update` must have been called with `world` since it last changed
    /// structurally, and for `'w`, the caller must hold the access that `D`
    /// reports.
    unsafe fn iter_unchecked<'w>(&'w self, world: &'w World) -> QueryIter<'w, D, F> {
        let archetypes = Matched::Cached(world.archetypes(), self.matched.iter());
        // SAFETY: the states come from `world`, `matched` lists each matching
        // archetype once, and the caller holds the access.
        unsafe { QueryIter::new(Some((self.data, self.filter)), archetypes) }
    }

    /// Checks the archetypes created since the last update.
    ///
    /// Archetypes are never removed, so earlier results stay valid.
    #[track_caller]
    fn update(&mut self, world: &World) {
        self.check_world(world);
        for (id, archetype) in world.archetypes().iter_from(self.checked) {
            if D::matches(&self.data, archetype) && F::matches(&self.filter, archetype) {
                self.matched.push(id);
            }
        }
        self.checked = world.archetypes().len();
    }

    /// Panics if `world` is not the world this state was created for, whose
    /// component and archetype ids would mean something else.
    #[track_caller]
    fn check_world(&self, world: &World) {
        assert!(
            world.id() == self.world,
            "a `QueryState` was used with a world other than the one it was created for",
        );
    }
}

impl<D: QueryData, F: QueryFilter> fmt::Debug for QueryState<D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueryState")
            .field("data", &type_name::<D>())
            .field("filter", &type_name::<F>())
            .field("matched_archetypes", &self.matched.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::Component;

    struct A(u32);
    impl Component for A {}

    struct B;
    impl Component for B {}

    #[test]
    fn archetypes_are_checked_once() {
        let mut world = World::new();
        let mut state = QueryState::<&A>::new(&mut world);
        assert_eq!(state.iter(&world).count(), 0);

        world.spawn(A(1));
        world.spawn(B);
        world.spawn((A(2), B));
        assert_eq!(state.iter(&world).map(|a| a.0).sum::<u32>(), 3);
        assert_eq!(state.matched.len(), 2);
        assert_eq!(state.checked, world.archetypes().len());

        // Nothing new: nothing is checked again.
        world.spawn(A(3));
        assert_eq!(state.iter(&world).count(), 3);
        assert_eq!(state.matched.len(), 2);
    }

    #[test]
    #[should_panic(expected = "world other than the one it was created for")]
    fn another_world_is_rejected() {
        let mut world = World::new();
        let mut state = QueryState::<&A>::new(&mut world);

        // Component and archetype ids differ between worlds.
        let mut other = World::new();
        other.spawn(B);
        other.spawn(A(1));
        let _ = state.iter(&other).count();
    }
}
