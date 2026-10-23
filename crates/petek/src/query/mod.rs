//! Queries: access to the components of every entity that matches a pattern.
//!
//! A query is described by two types:
//!
//! - its [`QueryData`] says what to fetch for each entity, such as
//!   `(Entity, &mut Position, &Velocity)`;
//! - its [`QueryFilter`] narrows down which entities match, without fetching
//!   anything, such as `Without<Frozen>`.
//!
//! [`World::query`] runs a query that only reads, and [`World::query_mut`]
//! one that may also write:
//!
//! ```
//! use petek::{Component, Entity, World};
//!
//! struct Position(f32);
//! impl Component for Position {}
//!
//! struct Velocity(f32);
//! impl Component for Velocity {}
//!
//! struct Frozen;
//! impl Component for Frozen {}
//!
//! let mut world = World::new();
//! let moving = world.spawn((Position(0.0), Velocity(1.0)));
//! let frozen = world.spawn((Position(0.0), Velocity(1.0), Frozen));
//!
//! let query = world.query_mut::<(&mut Position, &Velocity)>().without::<Frozen>();
//! for (mut position, velocity) in query {
//!     position.0 += velocity.0;
//! }
//!
//! let moved: Vec<Entity> = world
//!     .query::<(Entity, &Position)>()
//!     .into_iter()
//!     .filter_map(|(entity, position)| (position.0 > 0.0).then_some(entity))
//!     .collect();
//! assert_eq!(moved, [moving]);
//! ```
//!
//! Each run checks every archetype against the query. For a query that runs
//! over and over, a [`QueryState`] remembers which archetypes match, and only
//! checks archetypes created since its last run.
//!
//! A query borrows the world, so entities cannot be spawned, despawned or
//! changed structurally while it runs:
//!
//! ```compile_fail,E0499
//! # use petek::{Component, World};
//! # struct Position(f32);
//! # impl Component for Position {}
//! let mut world = World::new();
//! for position in world.query_mut::<&mut Position>() {
//!     world.spawn(Position(position.0));
//! }
//! ```

use std::any::type_name;
use std::error::Error;
use std::fmt;

use crate::archetype::ArchetypeId;
use crate::change::Ticks;
use crate::command::{CommandQueue, Commands};
use crate::component::{Component, Components};
use crate::entity::Entity;
use crate::world::{NoSuchEntity, World};

mod data;
mod filter;
mod iter;
mod state;

pub use data::{QueryData, ReadOnlyQueryData};
pub use filter::{Added, ArchetypeFilter, Changed, Or, QueryFilter, With, Without};
pub use iter::QueryIter;
pub use state::QueryState;

pub(crate) use data::{Access, Presence};
use iter::Matched;

/// Keeps the query traits from being implemented outside of this crate, since
/// the soundness of queries relies on their implementations.
mod sealed {
    /// Implemented by every [`QueryData`](super::QueryData).
    pub trait Data {}
    /// Implemented by every [`ReadOnlyQueryData`](super::ReadOnlyQueryData).
    pub trait ReadOnly {}
    /// Implemented by every [`QueryFilter`](super::QueryFilter).
    pub trait Filter {}
    /// Implemented by every [`ArchetypeFilter`](super::ArchetypeFilter).
    pub trait Archetypal {}
}

/// A query over a world, returned by [`World::query`] and
/// [`World::query_mut`].
///
/// Iterate over it to visit every matching entity, narrow it down first with
/// [`with`](Self::with), [`without`](Self::without) and
/// [`filter`](Self::filter), or look up a single entity with
/// [`get`](Self::get).
///
/// Iterating with `for` and [`get`](Self::get) consume the query, so that the
/// items can live as long as the borrow of the world.
/// [`iter_mut`](Self::iter_mut) and [`get_mut`](Self::get_mut) borrow it
/// instead, so it can be used again once their items are gone. A query that
/// only reads is `Copy`, and can be used any number of times anyway.
pub struct Query<'w, D: QueryData, F: QueryFilter = ()> {
    world: &'w World,
    /// `None` if some of `D`'s components are not registered, so that nothing
    /// can match.
    data: Option<D::State>,
    filter: F::State,
    /// The matching archetypes, if a [`QueryState`] listed them; otherwise
    /// every archetype is checked.
    matched: Option<&'w [ArchetypeId]>,
    ticks: Ticks,
    /// Whether the query is a system's parameter. Systems may run in
    /// parallel, so such a query must not reserve entities, which only
    /// systems that say they do may.
    in_system: bool,
}

impl<'w, D: QueryData, F: QueryFilter> Query<'w, D, F> {
    /// Creates a query over `world`.
    ///
    /// # Safety
    ///
    /// - The states must come from `world`.
    /// - For `'w`, the caller must hold the access that `D` reports, and `D`
    ///   must not access any component mutably more than once (see
    ///   [`check_access`]).
    /// - `ticks.this_run` must not be older than any tick of the components
    ///   that `D` accesses.
    pub(crate) unsafe fn new(
        world: &'w World,
        data: Option<D::State>,
        filter: F::State,
        ticks: Ticks,
    ) -> Self {
        Self {
            world,
            data,
            filter,
            matched: None,
            ticks,
            in_system: false,
        }
    }

    /// Creates a query over `world` for a system, that only visits the
    /// archetypes in `matched`.
    ///
    /// # Safety
    ///
    /// As for [`new`](Self::new), and `matched` must list each archetype of
    /// `world` that matches the states, and only those, at most once.
    pub(crate) unsafe fn new_in_system(
        world: &'w World,
        data: D::State,
        filter: F::State,
        matched: &'w [ArchetypeId],
        ticks: Ticks,
    ) -> Self {
        Self {
            world,
            data: Some(data),
            filter,
            matched: Some(matched),
            ticks,
            in_system: true,
        }
    }

    /// Only matches entities that also have a `T` component.
    #[must_use]
    pub fn with<T: Component>(self) -> Query<'w, D, (F, With<T>)> {
        self.filter()
    }

    /// Only matches entities that do not have a `T` component.
    #[must_use]
    pub fn without<T: Component>(self) -> Query<'w, D, (F, Without<T>)> {
        self.filter()
    }

    /// Only matches entities that also pass the filter `G`, which decides
    /// for whole archetypes, such as [`With`] or [`Or`].
    ///
    /// Filters that compare ticks, [`Added`] and [`Changed`], belong in the
    /// query's type instead, as in `Query<&Health, Changed<Health>>`. They
    /// read the ticks of a component, and a schedule only knows what a
    /// system reads from the types of its parameters, so it could run a
    /// system that adds one here alongside another that writes those ticks:
    ///
    /// ```compile_fail,E0277
    /// # use petek::{Component, World};
    /// # use petek::query::Changed;
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// let world = World::new();
    /// let changed = world.query::<&Health>().filter::<Changed<Health>>();
    /// ```
    #[must_use]
    pub fn filter<G: ArchetypeFilter>(self) -> Query<'w, D, (F, G)> {
        // Archetype filters read nothing: they only check which components an
        // archetype has, which cannot change for `'w`. So this keeps `new`'s
        // guarantees. The archetypes that matched before may not match `G`,
        // so they are all checked again.
        Query {
            world: self.world,
            data: self.data,
            filter: (self.filter, G::get_state(self.world.components())),
            matched: None,
            ticks: self.ticks,
            in_system: self.in_system,
        }
    }

    /// Returns the item of `entity`.
    ///
    /// This is handy to borrow several components of one entity at once:
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// # struct Shield(u32);
    /// # impl Component for Shield {}
    /// let mut world = World::new();
    /// let player = world.spawn((Health(10), Shield(5)));
    ///
    /// let (mut health, mut shield) = world.query_mut::<(&mut Health, &mut Shield)>().get(player)?;
    /// let absorbed = shield.0.min(8);
    /// shield.0 -= absorbed;
    /// health.0 -= 8 - absorbed;
    /// # assert_eq!((health.0, shield.0), (7, 0));
    /// # Ok::<(), petek::query::QueryError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`QueryError::NoSuchEntity`] if `entity` is not alive, or
    /// [`QueryError::DoesNotMatch`] if the query does not match it.
    pub fn get(self, entity: Entity) -> Result<D::Item<'w>, QueryError> {
        let location = self
            .world
            .entities()
            .get(entity)
            .ok_or(QueryError::NoSuchEntity(entity))?;
        let archetype = self.world.archetypes().get(location.archetype);
        let row = location.row as usize;
        let Some(data) = &self.data else {
            return Err(QueryError::DoesNotMatch(entity));
        };
        if !(D::matches(data, archetype) && F::matches(&self.filter, archetype)) {
            return Err(QueryError::DoesNotMatch(entity));
        }
        // SAFETY: the archetype matches, the states come from its world, an
        // alive entity's row is in bounds, and nothing writes the ticks the
        // filter reads.
        let passes = unsafe { F::filter_row(&F::fetch(&self.filter, archetype, self.ticks), row) };
        if !passes {
            return Err(QueryError::DoesNotMatch(entity));
        }
        // SAFETY: as above. `new`'s caller holds the access, and since this
        // consumes the query, the row is fetched only once (unless the query
        // is read-only and was copied).
        Ok(unsafe { D::item(&D::fetch(data, archetype, self.ticks), row) })
    }

    /// Returns a [`Commands`] that records into `queue`, and can be used while
    /// this query runs, even if the query writes.
    ///
    /// That is sound because commands only reserve entities until the queue
    /// is applied, and queries never write the entity allocator. The
    /// commands do not borrow the query, so it can still be consumed:
    ///
    /// ```
    /// # use petek::command::CommandQueue;
    /// # use petek::{Component, Entity, World};
    /// # struct Fuse(u32);
    /// # impl Component for Fuse {}
    /// # struct Explosion;
    /// # impl Component for Explosion {}
    /// let mut world = World::new();
    /// world.spawn_batch([Fuse(1), Fuse(3)]);
    ///
    /// let mut queue = CommandQueue::new();
    /// let query = world.query_mut::<(Entity, &mut Fuse)>();
    /// let mut commands = query.commands(&mut queue);
    /// for (entity, mut fuse) in query {
    ///     fuse.0 -= 1;
    ///     if fuse.0 == 0 {
    ///         commands.despawn(entity);
    ///         commands.spawn(Explosion);
    ///     }
    /// }
    /// queue.apply(&mut world);
    /// assert_eq!(world.query::<&Fuse>().iter().count(), 1);
    /// assert_eq!(world.query::<&Explosion>().iter().count(), 1);
    /// ```
    ///
    /// In a [system](crate::system), take a [`Commands`] parameter instead.
    /// Systems may run in parallel, and only those that say they record
    /// commands are kept from reserving entities at the same time, which
    /// would hand them out in an order that depends on timing.
    ///
    /// # Panics
    ///
    /// If the query is a system's parameter, or the queue holds commands
    /// recorded for another world.
    #[track_caller]
    pub fn commands<'q>(&self, queue: &'q mut CommandQueue) -> Commands<'q>
    where
        'w: 'q,
    {
        assert!(
            !self.in_system,
            "a query in a system cannot record commands: take a `Commands` parameter instead",
        );
        queue.commands(self.world)
    }

    /// Iterates over the items of every matching entity, borrowing the query
    /// rather than consuming it.
    pub fn iter_mut(&mut self) -> QueryIter<'_, '_, D, F> {
        self.reborrow().into_iter()
    }

    /// Returns the item of `entity`, borrowing the query rather than consuming
    /// it, so that several entities can be looked up one after another.
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// let mut world = World::new();
    /// let (a, b) = (world.spawn(Health(10)), world.spawn(Health(20)));
    ///
    /// let mut health = world.query_mut::<&mut Health>();
    /// let stolen = std::mem::take(&mut health.get_mut(a)?.0);
    /// health.get_mut(b)?.0 += stolen;
    /// # assert_eq!(world.get::<Health>(b).unwrap().0, 30);
    /// # Ok::<(), petek::query::QueryError>(())
    /// ```
    ///
    /// The item borrows the query, so two items cannot be held at once, as
    /// they would alias if both belonged to the same entity:
    ///
    /// ```compile_fail,E0499
    /// # use petek::{Component, World};
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// let mut world = World::new();
    /// let a = world.spawn(Health(10));
    ///
    /// let mut health = world.query_mut::<&mut Health>();
    /// let first = health.get_mut(a)?;
    /// let second = health.get_mut(a)?;
    /// first.0 += second.0;
    /// # Ok::<(), petek::query::QueryError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// As for [`get`](Self::get).
    pub fn get_mut(&mut self, entity: Entity) -> Result<D::Item<'_>, QueryError> {
        self.reborrow().get(entity)
    }

    /// Returns a copy of this query that borrows it mutably, so that the two
    /// cannot be used at the same time.
    fn reborrow(&mut self) -> Query<'_, D, F> {
        Query {
            world: self.world,
            data: self.data,
            filter: self.filter,
            matched: self.matched,
            ticks: self.ticks,
            in_system: self.in_system,
        }
    }
}

impl<'w, D: ReadOnlyQueryData, F: QueryFilter> Query<'w, D, F> {
    /// Iterates over the items of every matching entity.
    #[must_use]
    pub fn iter(&self) -> QueryIter<'w, 'w, D, F> {
        (*self).into_iter()
    }
}

impl<'w, D: QueryData, F: QueryFilter> IntoIterator for Query<'w, D, F> {
    type Item = D::Item<'w>;
    type IntoIter = QueryIter<'w, 'w, D, F>;

    fn into_iter(self) -> Self::IntoIter {
        let state = self.data.map(|data| (data, self.filter));
        let archetypes = self.world.archetypes();
        let matched = match self.matched {
            Some(ids) => Matched::Cached(archetypes, ids.iter()),
            None => Matched::Scan(archetypes.iter()),
        };
        // SAFETY: the state comes from the world whose archetypes are visited,
        // `new_cached`'s caller guarantees the list of matching archetypes,
        // and `new`'s caller holds the access.
        unsafe { QueryIter::new(state, matched, self.ticks) }
    }
}

impl<'a, D: QueryData, F: QueryFilter> IntoIterator for &'a mut Query<'_, D, F> {
    type Item = D::Item<'a>;
    type IntoIter = QueryIter<'a, 'a, D, F>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter_mut()
    }
}

impl<'w, D: ReadOnlyQueryData, F: QueryFilter> IntoIterator for &Query<'w, D, F> {
    type Item = D::Item<'w>;
    type IntoIter = QueryIter<'w, 'w, D, F>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

// Copying a read-only query is sound: any number of them can run at once.
impl<D: ReadOnlyQueryData, F: QueryFilter> Clone for Query<'_, D, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<D: ReadOnlyQueryData, F: QueryFilter> Copy for Query<'_, D, F> {}

impl<D: QueryData, F: QueryFilter> fmt::Debug for Query<'_, D, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Query")
            .field("data", &type_name::<D>())
            .field("filter", &type_name::<F>())
            .finish_non_exhaustive()
    }
}

/// The error returned when a query cannot fetch the item of an entity.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum QueryError {
    /// The entity was never spawned or has been despawned.
    NoSuchEntity(Entity),
    /// The entity lacks a component the query needs, or has one it excludes.
    DoesNotMatch(Entity),
}

impl From<NoSuchEntity> for QueryError {
    fn from(error: NoSuchEntity) -> Self {
        Self::NoSuchEntity(error.0)
    }
}

impl fmt::Display for QueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchEntity(entity) => write!(f, "entity {entity} does not exist"),
            Self::DoesNotMatch(entity) => write!(f, "entity {entity} does not match the query"),
        }
    }
}

impl Error for QueryError {}

/// Panics if `D` accesses a component mutably and also anywhere else, which
/// would let it hand out aliasing references.
#[track_caller]
pub(crate) fn check_access<D: QueryData>(state: &D::State, components: &Components) {
    let mut conflict = None;
    let mut i = 0;
    D::visit_access(state, &mut |id, access| {
        let mut j = 0;
        D::visit_access(state, &mut |other, other_access| {
            let writes = access == Access::Write || other_access == Access::Write;
            if j > i && id == other && writes {
                conflict.get_or_insert(id);
            }
            j += 1;
        });
        i += 1;
    });
    if let Some(id) = conflict {
        panic!(
            "query `{}` accesses `{}` more than once, mutably at least once",
            type_name::<D>(),
            components.info(id).name(),
        );
    }
}
