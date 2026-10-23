//! Resources: unique values that belong to the world rather than to an
//! entity.

use std::any::{Any, TypeId, type_name};
use std::hash::{Hash, Hasher};

use crate::checksum::ChecksumHasher;
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

/// A type-erased resource.
pub(crate) type Value = Box<dyn Any + Send + Sync>;

/// Clones a type-erased resource of the type the function was created for.
type CloneFn = fn(&(dyn Any + Send + Sync)) -> Value;

/// Hashes a type-erased resource of the type the function was created for.
type HashFn = fn(&(dyn Any + Send + Sync), &mut ChecksumHasher);

/// The resources of a world.
///
/// Each resource type gets a slot the first time it is inserted or
/// registered, and keeps it for good, even when the resource is removed, so
/// that snapshots can tell resources apart by slot.
#[derive(Default)]
pub(crate) struct Resources {
    slots: FastHashMap<TypeId, usize>,
    /// The resource in each slot, if there is one. These are all that
    /// snapshots copy: slots, once created, are never removed.
    values: Vec<Option<Value>>,
    /// The type of each slot.
    types: Vec<SlotType>,
    /// The slots of the resource types registered for checksums, in the
    /// order they were registered, which is the order checksums visit them
    /// in.
    hashed: Vec<(usize, HashFn)>,
}

/// The type of resource a slot holds.
struct SlotType {
    /// For diagnostics only.
    name: &'static str,
    /// How to clone the resource, if its type is registered for snapshots.
    clone: Option<CloneFn>,
}

impl Resources {
    /// Inserts `value`, returning the previous value of the same type, if
    /// any.
    pub(crate) fn insert<R: Resource>(&mut self, value: R) -> Option<R> {
        let slot = self.slot_or_insert::<R>();
        self.values[slot].replace(Box::new(value)).map(downcast)
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

    /// Registers `R` for snapshots, which copy it with `Clone`.
    pub(crate) fn register_clone<R: Resource + Clone>(&mut self) {
        let slot = self.slot_or_insert::<R>();
        self.types[slot].clone = Some(clone_resource::<R>);
    }

    /// Registers `R` for checksums, which hash it with `Hash`.
    pub(crate) fn register_hash<R: Resource + Hash>(&mut self) {
        let slot = self.slot_or_insert::<R>();
        if self.hashed.iter().all(|&(hashed, _)| hashed != slot) {
            self.hashed.push((slot, hash_resource::<R>));
        }
    }

    /// Hashes the resources registered for checksums, and whether each is
    /// present.
    pub(crate) fn hash(&self, hasher: &mut ChecksumHasher) {
        for &(slot, hash) in &self.hashed {
            match self.values[slot].as_deref() {
                Some(value) => {
                    hasher.write_u8(1);
                    hash(value, hasher);
                }
                None => hasher.write_u8(0),
            }
        }
    }

    /// The name of a resource present whose type is not registered for
    /// snapshots, if there is one.
    pub(crate) fn uncloneable(&self) -> Option<&'static str> {
        self.values
            .iter()
            .zip(&self.types)
            .find(|(value, ty)| value.is_some() && ty.clone.is_none())
            .map(|(_, ty)| ty.name)
    }

    /// Clones every resource present, for a snapshot.
    ///
    /// # Panics
    ///
    /// If a resource's type is not registered for snapshots.
    pub(crate) fn snapshot(&self) -> Box<[Option<Value>]> {
        self.clone_values(&self.values).collect()
    }

    /// Clones the resources of a snapshot of this world, ready to be swapped
    /// in by [`restore`](Self::restore).
    pub(crate) fn clone_snapshot(&self, snapshot: &[Option<Value>]) -> Vec<Option<Value>> {
        // Slots created since the snapshot was taken were empty back then.
        debug_assert!(snapshot.len() <= self.values.len());
        let empty = self.values.len() - snapshot.len();
        self.clone_values(snapshot)
            .chain(std::iter::repeat_with(|| None).take(empty))
            .collect()
    }

    /// Swaps the resources for those in `values`, which must come from
    /// [`clone_snapshot`](Self::clone_snapshot). Runs no user code: the
    /// resources the world held are left in `values`, to be dropped by the
    /// caller.
    pub(crate) fn restore(&mut self, values: &mut Vec<Option<Value>>) {
        debug_assert_eq!(values.len(), self.values.len());
        std::mem::swap(&mut self.values, values);
    }

    /// Clones `values`, which are resources of this world in slot order.
    fn clone_values<'a>(
        &'a self,
        values: &'a [Option<Value>],
    ) -> impl Iterator<Item = Option<Value>> + 'a {
        values.iter().zip(&self.types).map(|(value, ty)| {
            let value = value.as_deref()?;
            let clone = ty.clone.unwrap_or_else(|| {
                panic!("the `{}` resource is not registered for snapshots", ty.name)
            });
            Some(clone(value))
        })
    }

    fn slot<R: Resource>(&self) -> Option<usize> {
        self.slots.get(&TypeId::of::<R>()).copied()
    }

    /// Returns the slot of `R`, creating it on first use.
    fn slot_or_insert<R: Resource>(&mut self) -> usize {
        let (values, types) = (&mut self.values, &mut self.types);
        *self.slots.entry(TypeId::of::<R>()).or_insert_with(|| {
            values.push(None);
            types.push(SlotType {
                name: type_name::<R>(),
                clone: None,
            });
            values.len() - 1
        })
    }
}

/// Unboxes a resource taken out of the slot for `R`.
fn downcast<R: Resource>(value: Value) -> R {
    *value.downcast().expect("the slot holds an `R`")
}

/// The [`CloneFn`] for `R`.
fn clone_resource<R: Resource + Clone>(value: &(dyn Any + Send + Sync)) -> Value {
    Box::new(
        value
            .downcast_ref::<R>()
            .expect("the slot holds an `R`")
            .clone(),
    )
}

/// The [`HashFn`] for `R`.
fn hash_resource<R: Resource + Hash>(value: &(dyn Any + Send + Sync), hasher: &mut ChecksumHasher) {
    value
        .downcast_ref::<R>()
        .expect("the slot holds an `R`")
        .hash(hasher);
}
