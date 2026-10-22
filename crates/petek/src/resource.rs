//! Resources: unique values that belong to the world rather than to an
//! entity.

use std::any::{Any, TypeId};

use crate::hash::FastHashMap;

/// A value of which a world holds at most one, outside of any entity, such as
/// the elapsed time or a random number generator.
///
/// ```
/// use petek::{Resource, World};
///
/// struct Gravity(f32);
/// impl Resource for Gravity {}
///
/// let mut world = World::new();
/// world.insert_resource(Gravity(-9.8));
/// world.resource_mut::<Gravity>().unwrap().0 *= 2.0;
/// assert_eq!(world.resource::<Gravity>().unwrap().0, -19.6);
/// ```
///
/// Like components, resources must be `Send + Sync`, so that worlds can be
/// shared between threads:
///
/// ```compile_fail,E0277
/// use std::rc::Rc;
///
/// struct Shared(Rc<u32>);
///
/// impl petek::Resource for Shared {}
/// ```
pub trait Resource: Send + Sync + 'static {}

/// The resources of a world.
///
/// Each resource type gets a slot the first time it is inserted, and keeps it
/// when it is removed. Slots are numbered in the order of first insertion, so
/// that visiting the resources in slot order is deterministic.
#[derive(Default)]
pub(crate) struct Resources {
    slots: FastHashMap<TypeId, usize>,
    values: Vec<Option<Box<dyn Any + Send + Sync>>>,
}

impl Resources {
    /// Inserts `value`, returning the previous value of the same type, if
    /// any.
    pub(crate) fn insert<R: Resource>(&mut self, value: R) -> Option<R> {
        let values = &mut self.values;
        let slot = *self.slots.entry(TypeId::of::<R>()).or_insert_with(|| {
            values.push(None);
            values.len() - 1
        });
        values[slot].replace(Box::new(value)).map(downcast)
    }

    /// Removes the resource of type `R` and returns it, if there is one.
    pub(crate) fn remove<R: Resource>(&mut self) -> Option<R> {
        let slot = self.slot::<R>()?;
        self.values[slot].take().map(downcast)
    }

    pub(crate) fn get<R: Resource>(&self) -> Option<&R> {
        let value = self.values[self.slot::<R>()?].as_deref()?;
        Some(value.downcast_ref().expect("the slot holds an `R`"))
    }

    pub(crate) fn get_mut<R: Resource>(&mut self) -> Option<&mut R> {
        let slot = self.slot::<R>()?;
        let value = self.values[slot].as_deref_mut()?;
        Some(value.downcast_mut().expect("the slot holds an `R`"))
    }

    /// The number of resources present.
    pub(crate) fn len(&self) -> usize {
        self.values.iter().filter(|value| value.is_some()).count()
    }

    fn slot<R: Resource>(&self) -> Option<usize> {
        self.slots.get(&TypeId::of::<R>()).copied()
    }
}

/// Unboxes a resource taken out of the slot for `R`.
fn downcast<R: Resource>(value: Box<dyn Any + Send + Sync>) -> R {
    *value.downcast().expect("the slot holds an `R`")
}
