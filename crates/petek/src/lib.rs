//! An archetype-based Entity Component System.
//!
//! Entities are lightweight ids, and components are plain Rust types attached
//! to them. Entities with the same set of components are stored together in
//! an *archetype*, with one contiguous column per component.
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
//! [Queries](query) visit every entity that has a given set of components:
//!
//! ```
//! # use petek::{Component, World};
//! # struct Position { x: f32, y: f32 }
//! # impl Component for Position {}
//! # struct Velocity { x: f32, y: f32 }
//! # impl Component for Velocity {}
//! # let mut world = World::new();
//! for (mut position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
//!     position.x += velocity.x;
//!     position.y += velocity.y;
//! }
//! ```
//!
//! `ARCHITECTURE.md` in the repository explains how the crate works inside.
//!
//! **Work in progress:** systems are not implemented yet.

mod archetype;
mod bundle;
mod change;
pub mod command;
mod component;
mod entity;
mod hash;
pub mod query;
mod resource;
mod storage;
mod world;

pub use bundle::Bundle;
pub use change::{Mut, Tick};
pub use component::{Component, ComponentInfo};
pub use entity::Entity;
pub use resource::Resource;
pub use world::{NoSuchEntity, RemoveError, SpawnBatch, World};

// Checks that the examples in the README compile and run.
#[cfg(doctest)]
#[doc = include_str!("../../../README.md")]
struct ReadmeDoctests;
