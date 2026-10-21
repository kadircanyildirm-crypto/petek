//! An archetype-based Entity Component System.
//!
//! **Work in progress:** the public API is not usable yet.

// Until `World` lands, these internals are only used by their unit tests.
#[cfg_attr(not(test), allow(dead_code))]
mod bundle;
#[cfg_attr(not(test), allow(dead_code))]
mod component;
#[cfg_attr(not(test), allow(dead_code))]
mod entity;
#[cfg_attr(not(test), allow(dead_code))]
mod storage;

pub use bundle::Bundle;
pub use component::{Component, ComponentInfo};
pub use entity::Entity;
