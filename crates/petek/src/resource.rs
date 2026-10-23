//! Resources: unique values that belong to the world rather than to an
//! entity.

use std::any::{Any, TypeId, type_name};
use std::cell::UnsafeCell;
use std::hash::{Hash, Hasher};
use std::mem;

use crate::change::{Mut, Tick, Ticks};
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

/// A resource, with the ticks at which it was added and last changed.
pub(crate) struct Stored {
    value: Value,
    added: Tick,
    changed: Tick,
}

impl Stored {
    /// The resource, which must be an `R`.
    pub(crate) fn downcast_ref<R: Resource>(&self) -> &R {
        self.value.downcast_ref().expect("the slot holds an `R`")
    }

    /// The resource, which must be an `R`, marked as changed at
    /// `ticks.this_run` when it is written through.
    pub(crate) fn downcast_mut<R: Resource>(&mut self, ticks: Ticks) -> Mut<'_, R> {
        let value = self.value.downcast_mut().expect("the slot holds an `R`");
        Mut::new(value, self.added, &mut self.changed, ticks)
    }
}

/// The resources of a world.
///
/// Each resource type gets a slot the first time it is inserted or
/// registered, and keeps it for good, even when the resource is removed, so
/// that snapshots can tell resources apart by slot.
///
/// The resources are in `UnsafeCell`s, so that systems running in parallel
/// will be able to each write a different one through a shared reference to
/// the world. Through `&self`, a resource is only ever written by code that
/// has been granted exclusive access to it, so the methods that take `&self`
/// can read the resources.
#[derive(Default)]
pub(crate) struct Resources {
    slots: FastHashMap<TypeId, usize>,
    /// The resource in each slot, if there is one. These are all that
    /// snapshots copy: slots, once created, are never removed.
    values: Vec<UnsafeCell<Option<Stored>>>,
    /// The type of each slot.
    types: Vec<SlotType>,
    /// The slots of the resource types registered for checksums, in the
    /// order they were registered, which is the order checksums visit them
    /// in.
    hashed: Vec<(usize, HashFn)>,
}

// SAFETY: resources are `Send + Sync`, and through a shared reference, the
// `UnsafeCell`s are only written with exclusive access to the resource (see
// the type's documentation).
unsafe impl Sync for Resources {}

/// The type of resource a slot holds.
struct SlotType {
    /// For diagnostics only.
    name: &'static str,
    /// How to clone the resource, if its type is registered for snapshots.
    clone: Option<CloneFn>,
}

impl Resources {
    /// Inserts `value`, returning the previous value of the same type, if
    /// any. The resource counts as changed at `tick`, and as added too unless
    /// it replaces one.
    pub(crate) fn insert<R: Resource>(&mut self, value: R, tick: Tick) -> Option<R> {
        let slot = self.slot_or_insert::<R>();
        match self.values[slot].get_mut() {
            Some(stored) => {
                stored.changed = tick;
                Some(downcast(mem::replace(&mut stored.value, Box::new(value))))
            }
            empty @ None => {
                *empty = Some(Stored {
                    value: Box::new(value),
                    added: tick,
                    changed: tick,
                });
                None
            }
        }
    }

    /// Removes the resource of type `R` and returns it, if there is one.
    pub(crate) fn remove<R: Resource>(&mut self) -> Option<R> {
        let slot = self.slot::<R>()?;
        let stored = self.values[slot].get_mut().take()?;
        Some(downcast(stored.value))
    }

    pub(crate) fn get<R: Resource>(&self) -> Option<&R> {
        Some(self.read(self.slot::<R>()?)?.downcast_ref())
    }

    /// Returns the resource of type `R`, marked as changed at
    /// `ticks.this_run` when it is written through.
    pub(crate) fn get_mut<R: Resource>(&mut self, ticks: Ticks) -> Option<Mut<'_, R>> {
        let slot = self.slot::<R>()?;
        Some(self.values[slot].get_mut().as_mut()?.downcast_mut(ticks))
    }

    /// Returns the resource in `slot`, if there is one.
    #[inline]
    pub(crate) fn read(&self, slot: usize) -> Option<&Stored> {
        // SAFETY: through `&self`, only code with exclusive access to a
        // resource writes it, so no one is writing this one.
        unsafe { (*self.values[slot].get()).as_ref() }
    }

    /// The number of resources present.
    pub(crate) fn len(&self) -> usize {
        (0..self.values.len())
            .filter(|&slot| self.read(slot).is_some())
            .count()
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
            match self.read(slot) {
                Some(stored) => {
                    hasher.write_u8(1);
                    hash(stored.value.as_ref(), hasher);
                }
                None => hasher.write_u8(0),
            }
        }
    }

    /// The name of a resource present whose type is not registered for
    /// snapshots, if there is one.
    pub(crate) fn uncloneable(&self) -> Option<&'static str> {
        (0..self.values.len())
            .find(|&slot| self.read(slot).is_some() && self.types[slot].clone.is_none())
            .map(|slot| self.types[slot].name)
    }

    /// Clones every resource present, with its ticks, for a snapshot.
    ///
    /// # Panics
    ///
    /// If a resource's type is not registered for snapshots.
    pub(crate) fn snapshot(&self) -> Box<[Option<Stored>]> {
        (0..self.values.len())
            .map(|slot| self.clone_stored(slot, self.read(slot)))
            .collect()
    }

    /// Clones the resources of a snapshot of this world, ready to be swapped
    /// in by [`restore`](Self::restore).
    pub(crate) fn clone_snapshot(
        &self,
        snapshot: &[Option<Stored>],
    ) -> Vec<UnsafeCell<Option<Stored>>> {
        // Slots created since the snapshot was taken were empty back then.
        debug_assert!(snapshot.len() <= self.values.len());
        (0..self.values.len())
            .map(|slot| {
                let stored = snapshot.get(slot).and_then(Option::as_ref);
                UnsafeCell::new(self.clone_stored(slot, stored))
            })
            .collect()
    }

    /// Swaps the resources for those in `values`, which must come from
    /// [`clone_snapshot`](Self::clone_snapshot). Runs no user code: the
    /// resources the world held are left in `values`, to be dropped by the
    /// caller.
    pub(crate) fn restore(&mut self, values: &mut Vec<UnsafeCell<Option<Stored>>>) {
        debug_assert_eq!(values.len(), self.values.len());
        mem::swap(&mut self.values, values);
    }

    /// Clones a resource of the type of `slot`, with its ticks.
    fn clone_stored(&self, slot: usize, stored: Option<&Stored>) -> Option<Stored> {
        let stored = stored?;
        let ty = &self.types[slot];
        let clone = ty.clone.unwrap_or_else(|| {
            panic!("the `{}` resource is not registered for snapshots", ty.name)
        });
        Some(Stored {
            value: clone(stored.value.as_ref()),
            added: stored.added,
            changed: stored.changed,
        })
    }

    fn slot<R: Resource>(&self) -> Option<usize> {
        self.slots.get(&TypeId::of::<R>()).copied()
    }

    /// Returns the slot of `R`, creating it on first use.
    fn slot_or_insert<R: Resource>(&mut self) -> usize {
        let (values, types) = (&mut self.values, &mut self.types);
        *self.slots.entry(TypeId::of::<R>()).or_insert_with(|| {
            values.push(UnsafeCell::new(None));
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
