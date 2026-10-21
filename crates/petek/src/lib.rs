//! An archetype-based Entity Component System.
//!
//! **Work in progress:** the public API is not usable yet.

mod archetype;
mod bundle;
mod component;
mod entity;
// Until `World::insert` lands, parts of the storage are only used by tests.
#[cfg_attr(not(test), allow(dead_code))]
mod storage;
mod world;

pub use bundle::Bundle;
pub use component::{Component, ComponentInfo};
pub use entity::Entity;
pub use world::{NoSuchEntity, World};
