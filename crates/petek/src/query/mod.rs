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
//! for (position, velocity) in query {
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

use crate::component::{Component, Components};
use crate::entity::Entity;
use crate::world::{NoSuchEntity, World};

mod data;
mod filter;
mod iter;

pub use data::{QueryData, ReadOnlyQueryData};
pub use filter::{Or, QueryFilter, With, Without};
pub use iter::QueryIter;

use data::Access;

/// Keeps the query traits from being implemented outside of this crate, since
/// the soundness of queries relies on their implementations.
mod sealed {
    /// Implemented by every [`QueryData`](super::QueryData).
    pub trait Data {}
    /// Implemented by every [`ReadOnlyQueryData`](super::ReadOnlyQueryData).
    pub trait ReadOnly {}
    /// Implemented by every [`QueryFilter`](super::QueryFilter).
    pub trait Filter {}
}

/// A query over a world, returned by [`World::query`] and
/// [`World::query_mut`].
///
/// Iterate over it to visit every matching entity, narrow it down first with
/// [`with`](Self::with), [`without`](Self::without) and
/// [`filter`](Self::filter), or look up a single entity with
/// [`get`](Self::get).
///
/// A query that only reads is `Copy`, so it can be used any number of times.
pub struct Query<'w, D: QueryData, F: QueryFilter = ()> {
    world: &'w World,
    /// `None` if some of `D`'s components are not registered, so that nothing
    /// can match.
    data: Option<D::State>,
    filter: F::State,
}

impl<'w, D: QueryData, F: QueryFilter> Query<'w, D, F> {
    /// Creates a query over `world`.
    ///
    /// # Safety
    ///
    /// - `data`, if any, must come from `world`.
    /// - For `'w`, the caller must hold the access that `D` reports, and `D`
    ///   must not access any component mutably more than once (see
    ///   [`check_access`]).
    pub(crate) unsafe fn new(world: &'w World, data: Option<D::State>) -> Self {
        Self {
            world,
            data,
            filter: F::get_state(world.components()),
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

    /// Only matches entities that also pass the filter `G`.
    #[must_use]
    pub fn filter<G: QueryFilter>(self) -> Query<'w, D, (F, G)> {
        // Filters access no components, so this keeps `new`'s guarantees.
        Query {
            world: self.world,
            data: self.data,
            filter: (self.filter, G::get_state(self.world.components())),
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
    /// let (health, shield) = world.query_mut::<(&mut Health, &mut Shield)>().get(player)?;
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
        match &self.data {
            Some(data) if D::matches(data, archetype) && F::matches(&self.filter, archetype) => {
                // SAFETY: the archetype matches, the state comes from its
                // world, and an alive entity's row is in bounds. `new`'s
                // caller holds the access, and since this consumes the query,
                // the row is fetched only once (unless the query is read-only
                // and was copied).
                Ok(unsafe { D::item(&D::fetch(data, archetype), location.row as usize) })
            }
            _ => Err(QueryError::DoesNotMatch(entity)),
        }
    }
}

impl<'w, D: QueryData, F: QueryFilter> IntoIterator for Query<'w, D, F> {
    type Item = D::Item<'w>;
    type IntoIter = QueryIter<'w, D, F>;

    fn into_iter(self) -> Self::IntoIter {
        let state = self.data.map(|data| (data, self.filter));
        // SAFETY: the state comes from the world whose archetypes are scanned,
        // and `new`'s caller holds the access.
        unsafe { QueryIter::new(state, self.world.archetypes().iter()) }
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
