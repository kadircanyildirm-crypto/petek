//! The world: the container of all entities and their components.

use std::any::TypeId;
use std::error::Error;
use std::fmt;
use std::ptr::NonNull;

use crate::archetype::{ArchetypeId, Archetypes};
use crate::bundle::{Bundle, Bundles};
use crate::component::{Component, Components};
use crate::entity::{Entities, Entity, EntityLocation};

/// Stores entities and their components.
///
/// ```
/// use petek::{Component, World};
///
/// #[derive(Debug, PartialEq)]
/// struct Health(u32);
/// impl Component for Health {}
///
/// let mut world = World::new();
/// let player = world.spawn(Health(100));
/// world.get_mut::<Health>(player).unwrap().0 -= 30;
/// assert_eq!(world.get::<Health>(player), Some(&Health(70)));
/// ```
pub struct World {
    entities: Entities,
    components: Components,
    bundles: Bundles,
    archetypes: Archetypes,
}

impl World {
    /// Creates an empty world.
    pub fn new() -> Self {
        Self {
            entities: Entities::default(),
            components: Components::default(),
            bundles: Bundles::default(),
            archetypes: Archetypes::new(),
        }
    }

    /// The number of alive entities.
    pub fn len(&self) -> u32 {
        self.entities.len()
    }

    /// Returns `true` if there are no alive entities.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if `entity` is alive.
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.contains(entity)
    }

    /// Creates an entity with the components in `bundle`.
    ///
    /// # Panics
    ///
    /// If the bundle contains the same component more than once.
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        let bundle_id = self.bundles.register::<B>(&mut self.components);
        let bundle_info = self.bundles.info(bundle_id);
        let archetype_id = self.archetypes.insert_target(
            ArchetypeId::EMPTY,
            bundle_id,
            &bundle_info.sorted_ids,
            &self.components,
        );
        let archetype = self.archetypes.get_mut(archetype_id);
        archetype.reserve(1);
        let row = u32::try_from(archetype.len()).expect("too many entities");
        let entity = self.entities.alloc(EntityLocation {
            archetype: archetype_id,
            row,
        });
        // SAFETY: the archetype's components are exactly the bundle's, so
        // pushing the bundle and then the entity completes a row.
        unsafe {
            archetype.push_bundle(bundle, &bundle_info.component_ids);
            archetype.push_entity(entity);
        }
        entity
    }

    /// Despawns `entity`, dropping its components.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), NoSuchEntity> {
        let location = self.entities.free(entity).ok_or(NoSuchEntity(entity))?;
        let archetype = self.archetypes.get_mut(location.archetype);
        let row = location.row as usize;
        // SAFETY: an alive entity's row is in bounds, and it is removed from
        // every column right below.
        if let Some(moved) = unsafe { archetype.swap_remove_entity(row) } {
            self.entities.set_location(moved, location);
        }
        // SAFETY: as above. Destructors run only now that the bookkeeping is
        // consistent again.
        unsafe { archetype.drop_from_columns(row, |_| true) };
        Ok(())
    }

    /// Returns a reference to `entity`'s `T` component, if it is alive and has
    /// one.
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let ptr = self.component_ptr::<T>(entity)?;
        // SAFETY: the pointer refers to a valid `T`, and `&self` rules out
        // mutable access for the returned lifetime.
        Some(unsafe { ptr.cast::<T>().as_ref() })
    }

    /// Returns a mutable reference to `entity`'s `T` component, if it is alive
    /// and has one.
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<&mut T> {
        let ptr = self.component_ptr::<T>(entity)?;
        // SAFETY: the pointer refers to a valid `T`, and `&mut self`
        // guarantees exclusive access for the returned lifetime.
        Some(unsafe { ptr.cast::<T>().as_mut() })
    }

    /// Returns a pointer to `entity`'s `T` component, if it is alive and has
    /// one.
    fn component_ptr<T: Component>(&self, entity: Entity) -> Option<NonNull<u8>> {
        let location = self.entities.get(entity)?;
        let id = self.components.id(TypeId::of::<T>())?;
        let archetype = self.archetypes.get(location.archetype);
        let column = archetype.column_index(id)?;
        // SAFETY: an alive entity's row is in bounds of its archetype's
        // columns.
        Some(unsafe {
            archetype
                .column(column)
                .get_unchecked(location.row as usize)
        })
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("entities", &self.len())
            .field("archetypes", &self.archetypes.len())
            .finish_non_exhaustive()
    }
}

/// The error returned when an entity was never spawned or has been despawned.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NoSuchEntity(pub Entity);

impl fmt::Display for NoSuchEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entity {:?} does not exist", self.0)
    }
}

impl Error for NoSuchEntity {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{self, AssertUnwindSafe};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, PartialEq)]
    struct Pos(i32);
    impl Component for Pos {}

    #[derive(Debug, PartialEq)]
    struct Vel(i32);
    impl Component for Vel {}

    #[derive(Debug, PartialEq)]
    struct Name(String);
    impl Component for Name {}

    #[derive(Debug, PartialEq)]
    struct Marker;
    impl Component for Marker {}

    /// Counts its drops in a shared counter.
    struct Tracked(Arc<AtomicUsize>);
    impl Component for Tracked {}

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn world_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<World>();
    }

    #[test]
    fn spawn_and_get() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Vel(2)));
        let b = world.spawn((Name("b".into()), Pos(3)));
        let c = world.spawn(());

        assert_eq!(world.len(), 3);
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(2)));
        assert_eq!(world.get::<Name>(a), None);
        assert_eq!(world.get::<Name>(b), Some(&Name("b".into())));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(3)));
        assert!(world.contains(c));
        assert_eq!(world.get::<Marker>(c), None);
    }

    #[test]
    fn get_mut_modifies_in_place() {
        let mut world = World::new();
        let entity = world.spawn((Pos(1), Marker));
        world.get_mut::<Pos>(entity).unwrap().0 = 10;
        assert_eq!(world.get::<Pos>(entity), Some(&Pos(10)));
        assert_eq!(world.get::<Marker>(entity), Some(&Marker));
    }

    #[test]
    fn despawn_invalidates_the_handle() {
        let mut world = World::new();
        let old = world.spawn(Pos(1));
        assert_eq!(world.despawn(old), Ok(()));
        assert_eq!(world.despawn(old), Err(NoSuchEntity(old)));
        assert!(!world.contains(old));
        assert!(world.is_empty());

        // The new entity reuses the slot, but the old handle must not see it.
        let new = world.spawn(Pos(2));
        assert_eq!(new.index(), old.index());
        assert_eq!(world.get::<Pos>(old), None);
        assert_eq!(world.get::<Pos>(new), Some(&Pos(2)));
    }

    #[test]
    fn despawn_keeps_the_other_rows_intact() {
        let mut world = World::new();
        let entities: Vec<_> = (0..5)
            .map(|i| world.spawn((Pos(i), Name(i.to_string()))))
            .collect();

        // Despawning row 1 moves the last row into its place.
        world.despawn(entities[1]).unwrap();
        for (i, &entity) in entities.iter().enumerate().filter(|&(i, _)| i != 1) {
            assert_eq!(world.get::<Pos>(entity), Some(&Pos(i as i32)));
            assert_eq!(world.get::<Name>(entity), Some(&Name(i.to_string())));
        }
    }

    #[test]
    fn components_are_dropped_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        let a = world.spawn((Pos(0), Tracked(drops.clone())));
        world.spawn(Tracked(drops.clone()));
        world.spawn((Tracked(drops.clone()), Marker));

        world.despawn(a).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(world);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn zero_sized_components() {
        let mut world = World::new();
        let entities: Vec<_> = (0..10).map(|_| world.spawn(Marker)).collect();
        world.despawn(entities[3]).unwrap();
        assert_eq!(world.get::<Marker>(entities[9]), Some(&Marker));
        assert_eq!(world.len(), 9);
    }

    #[test]
    #[should_panic(expected = "more than once")]
    fn spawning_a_bundle_with_duplicates_panics() {
        World::new().spawn((Pos(0), Pos(1)));
    }

    #[test]
    fn panicking_destructor_during_despawn_leaves_the_world_consistent() {
        static COUNTED_DROPS: AtomicUsize = AtomicUsize::new(0);

        struct Bomb {
            armed: bool,
        }
        impl Component for Bomb {}
        impl Drop for Bomb {
            fn drop(&mut self) {
                if self.armed {
                    panic!("boom");
                }
            }
        }

        struct Counted;
        impl Component for Counted {}
        impl Drop for Counted {
            fn drop(&mut self) {
                COUNTED_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut world = World::new();
        // Registration order puts the `Counted` column after the `Bomb` one.
        let a = world.spawn((Pos(1), Bomb { armed: true }, Counted));
        let b = world.spawn((Pos(2), Bomb { armed: false }, Counted));

        let result = panic::catch_unwind(AssertUnwindSafe(|| world.despawn(a)));
        assert!(result.is_err());

        // `a` is gone and its `Counted` was leaked rather than dropped, while
        // `b`, which moved into `a`'s row, is intact.
        assert!(!world.contains(a));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));
        assert_eq!(COUNTED_DROPS.load(Ordering::Relaxed), 0);
        drop(world);
        assert_eq!(COUNTED_DROPS.load(Ordering::Relaxed), 1);
    }
}
