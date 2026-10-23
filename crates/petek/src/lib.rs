//! An archetype-based Entity Component System, built for deterministic
//! simulation.
//!
//! Entities are lightweight ids, and components are plain Rust types attached
//! to them. Entities with the same set of components are stored together in
//! an *archetype*, with one contiguous column per component.
//!
//! Given the same sequence of operations, a world always ends up the same, down
//! to its entity handles, iteration order and change ticks, as lockstep and
//! rollback networking require. [`World::snapshot`] and [`World::restore`]
//! roll a world back, and [`World::checksum`] lets peers detect desyncs.
//!
//! ```
//! use petek::{Component, World};
//!
//! #[derive(Debug, PartialEq)]
//! struct Position {
//!     x: f32,
//!     y: f32,
//! }
//! impl Component for Position {}
//!
//! #[derive(Debug, PartialEq)]
//! struct Velocity {
//!     x: f32,
//!     y: f32,
//! }
//! impl Component for Velocity {}
//!
//! let mut world = World::new();
//! let ball = world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 1.0, y: 2.0 }));
//!
//! // Components can be added and removed at any time.
//! let velocity = world.remove::<Velocity>(ball)?;
//! let mut position = world.get_mut::<Position>(ball).unwrap();
//! position.x += velocity.x;
//! position.y += velocity.y;
//!
//! assert_eq!(world.get::<Position>(ball), Some(&Position { x: 1.0, y: 2.0 }));
//! assert!(!world.has::<Velocity>(ball));
//! # Ok::<(), petek::RemoveError>(())
//! ```
//!
//! [Queries](query) visit every entity that has a given set of components,
//! and [systems](system) are functions of them, which a
//! [schedule](schedule::Schedule) runs in parallel:
//!
//! ```
//! # use petek::{Component, World};
//! # struct Position { x: f32, y: f32 }
//! # impl Component for Position {}
//! # struct Velocity { x: f32, y: f32 }
//! # impl Component for Velocity {}
//! use petek::query::Query;
//! use petek::schedule::Schedule;
//!
//! fn movement(query: Query<(&mut Position, &Velocity)>) {
//!     for (mut position, velocity) in query {
//!         position.x += velocity.x;
//!         position.y += velocity.y;
//!     }
//! }
//!
//! # let mut world = World::new();
//! let mut schedule = Schedule::new();
//! schedule.add_system(movement);
//! schedule.run(&mut world);
//! ```
//!
//! `ARCHITECTURE.md` in the repository explains how the crate works inside.

mod archetype;
mod bundle;
mod change;
mod checksum;
pub mod command;
mod component;
mod entity;
mod hash;
mod pool;
pub mod query;
mod resource;
pub mod schedule;
mod snapshot;
mod storage;
pub mod system;
mod world;

pub use bundle::Bundle;
pub use change::{Mut, Tick};
pub use component::{Component, ComponentInfo};
pub use entity::Entity;
pub use resource::Resource;
pub use snapshot::Snapshot;
pub use world::{NoSuchEntity, RemoveError, SpawnBatch, World};

// Checks that the examples in the README compile and run.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;
