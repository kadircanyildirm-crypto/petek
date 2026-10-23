//! Queries that remember which archetypes match them.

use std::any::type_name;
use std::fmt;

use super::data::Access;
use super::iter::Matched;
use super::{
    Query, QueryData, QueryError, QueryFilter, QueryIter, ReadOnlyQueryData, check_access,
};
use crate::archetype::ArchetypeId;
use crate::change::{Tick, Ticks};
use crate::component::ComponentId;
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
///     for (mut position, velocity) in movement.iter_mut(&mut world) {
///         position.0 += velocity.0;
///     }
/// }
/// assert_eq!(world.get::<Position>(ball).unwrap().0, 3.0);
/// ```
///
/// A `QueryState` belongs to the world it was created for, and using it with
/// another world panics.
///
/// It is not part of the world, so [restoring](World::restore) a snapshot of
/// the world does not rewind it. Its [`Added`](super::Added) and
/// [`Changed`](super::Changed) filters may then see too little, since it
/// remembers running at a tick the world has not reached again yet. To roll
/// it back too, keep a clone of it alongside the snapshot.
pub struct QueryState<D: QueryData, F: QueryFilter = ()> {
    world: WorldId,
    data: D::State,
    filter: F::State,
    /// The matching archetypes among the first `checked` archetypes of the
    /// world, when its archetype epoch was `epoch`.
    matched: Vec<ArchetypeId>,
    checked: usize,
    epoch: u64,
    /// The world's tick when the query last ran; what is newer counts as
    /// added or changed.
    last_run: Tick,
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
            epoch: world.archetypes().epoch(),
            last_run: Tick::ZERO,
        }
    }

    /// Iterates over the items of every matching entity.
    ///
    /// [`Added`](super::Added) and [`Changed`](super::Changed) see what is
    /// newer than the previous run of `iter` or `iter_mut`.
    ///
    /// # Panics
    ///
    /// If `world` is not the world this state was created for.
    #[track_caller]
    pub fn iter<'w, 's>(&'s mut self, world: &'w World) -> QueryIter<'w, 's, D, F>
    where
        D: ReadOnlyQueryData,
    {
        self.update(world);
        let ticks = self.run(world.change_tick());
        // SAFETY: `update` checked that the state comes from `world` and
        // brought `matched` up to date. `D` only reads, and `&World` rules out
        // writes for `'w`.
        unsafe { self.iter_unchecked(world, ticks) }
    }

    /// Iterates over the items of every matching entity, which may borrow
    /// components mutably.
    ///
    /// # Panics
    ///
    /// If `world` is not the world this state was created for.
    #[track_caller]
    pub fn iter_mut<'w, 's>(&'s mut self, world: &'w mut World) -> QueryIter<'w, 's, D, F> {
        self.update(world);
        let ticks = self.run(world.increment_change_tick());
        // SAFETY: as in `iter`, except that `D` may write: `new` checked that
        // it accesses no component mutably more than once, and `&mut World`
        // gives it exclusive access for `'w`.
        unsafe { self.iter_unchecked(world, ticks) }
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
        let ticks = self.peek(world.change_tick());
        // SAFETY: the states come from `world`, `D` only reads, and `&World`
        // rules out writes for `'w`.
        unsafe { Query::<D, F>::new(world, Some(self.data), self.filter, ticks) }.get(entity)
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
        let ticks = self.peek(world.increment_change_tick());
        // SAFETY: the states come from `world`, `new` checked that `D`
        // accesses no component mutably more than once, and `&mut World`
        // gives it exclusive access for `'w`.
        unsafe { Query::<D, F>::new(world, Some(self.data), self.filter, ticks) }.get(entity)
    }

    /// Returns a query over `world` for a system, that visits the matching
    /// archetypes this state lists, after checking those created since the
    /// last update. It sees what changed since `ticks.last_run`, rather than
    /// since this state's last run, which it does not count as a run.
    ///
    /// # Safety
    ///
    /// `world` must be the world this state was created for, and for `'w`,
    /// the caller must hold the access that
    /// [`visit_access`](Self::visit_access) reports.
    pub(crate) unsafe fn query_unchecked<'w>(
        &'w mut self,
        world: &'w World,
        ticks: Ticks,
    ) -> Query<'w, D, F> {
        self.update(world);
        // SAFETY: `update` checked that the states come from `world` and
        // brought `matched` up to date, `new` checked that `D` accesses no
        // component mutably more than once, and the caller holds the access.
        unsafe { Query::new_in_system(world, self.data, self.filter, &self.matched, ticks) }
    }

    /// Calls `f` with every component the query accesses, and how, including
    /// those whose ticks its filter reads.
    pub(crate) fn visit_access(&self, f: &mut impl FnMut(ComponentId, Access)) {
        D::visit_access(&self.data, f);
        F::visit_access(&self.filter, f);
    }

    /// Iterates over the matching entities, without checking anything.
    ///
    /// # Safety
    ///
    /// `update` must have been called with `world` since it last changed
    /// structurally, and for `'w`, the caller must hold the access that `D`
    /// reports.
    unsafe fn iter_unchecked<'w, 's>(
        &'s self,
        world: &'w World,
        ticks: Ticks,
    ) -> QueryIter<'w, 's, D, F> {
        let archetypes = Matched::Cached(world.archetypes(), self.matched.iter());
        // SAFETY: the states come from `world`, `matched` lists each matching
        // archetype once, and the caller holds the access.
        unsafe { QueryIter::new(Some((self.data, self.filter)), archetypes, ticks) }
    }

    /// Starts a run at `this_run`: the run sees what is newer than the last
    /// one, and the next run will see what is newer than this one.
    fn run(&mut self, this_run: Tick) -> Ticks {
        let ticks = self.peek(this_run);
        self.last_run = this_run;
        ticks
    }

    /// The ticks of a run at `this_run` that does not count as a run, such as
    /// fetching a single entity.
    fn peek(&self, this_run: Tick) -> Ticks {
        Ticks {
            last_run: self.last_run,
            this_run,
        }
    }

    /// Checks the archetypes created since the last update.
    ///
    /// Earlier results stay valid unless archetypes were removed since, which
    /// changes the world's archetype epoch. Everything is checked again then.
    #[track_caller]
    fn update(&mut self, world: &World) {
        self.check_world(world);
        let archetypes = world.archetypes();
        if self.epoch != archetypes.epoch() {
            self.matched.clear();
            self.checked = 0;
            self.epoch = archetypes.epoch();
        }
        // Reserving first means that pushing cannot panic halfway, which
        // would leave archetypes listed but not counted as checked, and so
        // listed twice after the next update.
        self.matched.reserve(archetypes.len() - self.checked);
        for (id, archetype) in archetypes.iter_from(self.checked) {
            if D::matches(&self.data, archetype) && F::matches(&self.filter, archetype) {
                self.matched.push(id);
            }
        }
        self.checked = archetypes.len();
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

impl<D: QueryData, F: QueryFilter> Clone for QueryState<D, F> {
    fn clone(&self) -> Self {
        Self {
            world: self.world,
            data: self.data,
            filter: self.filter,
            matched: self.matched.clone(),
            checked: self.checked,
            epoch: self.epoch,
            last_run: self.last_run,
        }
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
