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
        archetype.debug_assert_consistent();
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
        archetype.debug_assert_consistent();
        Ok(())
    }

    /// Adds the components in `bundle` to `entity`. Components the entity
    /// already has are overwritten, and their old values dropped.
    ///
    /// # Panics
    ///
    /// If the bundle contains the same component more than once.
    pub fn insert<B: Bundle>(&mut self, entity: Entity, bundle: B) -> Result<(), NoSuchEntity> {
        let location = self.entities.get(entity).ok_or(NoSuchEntity(entity))?;
        let bundle_id = self.bundles.register::<B>(&mut self.components);
        let bundle_info = self.bundles.info(bundle_id);
        let src_id = location.archetype;
        let dst_id = self.archetypes.insert_target(
            src_id,
            bundle_id,
            &bundle_info.sorted_ids,
            &self.components,
        );
        let src_row = location.row as usize;

        if src_id == dst_id {
            // The entity already has all of the bundle's components.
            let archetype = self.archetypes.get_mut(src_id);
            // SAFETY: an alive entity's row is in bounds, and its archetype has
            // a column for every component of the bundle.
            unsafe { archetype.replace_bundle(src_row, bundle, &bundle_info.component_ids) };
            return Ok(());
        }

        let (src, dst) = self.archetypes.get_two_mut(src_id, dst_id);
        let dst_row = u32::try_from(dst.len()).expect("too many entities");
        // First restructure, which runs no user code...
        // SAFETY: the row is in bounds, and `dst` has every component of `src`
        // plus the bundle's. The bundle's components stay behind in `src`, and
        // the bundle itself fills them in on the new row.
        let moved = unsafe {
            let moved = src.move_row_to(src_row, dst, &bundle_info.sorted_ids);
            dst.push_bundle(bundle, &bundle_info.component_ids);
            moved
        };
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype: dst_id,
                row: dst_row,
            },
        );
        if let Some(moved) = moved {
            self.entities.set_location(moved, location);
        }
        // ...then, with everything consistent, drop the values the bundle
        // replaced.
        // SAFETY: `src_row` is still in the columns that were skipped.
        unsafe {
            src.drop_from_columns(src_row, |id| {
                bundle_info.sorted_ids.binary_search(&id).is_ok()
            });
        }
        src.debug_assert_consistent();
        dst.debug_assert_consistent();
        Ok(())
    }

    /// Removes the components in `B` from `entity` and returns them.
    ///
    /// Returns `None`, leaving the entity untouched, if the entity is not alive
    /// or lacks any of the components.
    ///
    /// # Panics
    ///
    /// If the bundle contains the same component more than once.
    pub fn remove<B: Bundle>(&mut self, entity: Entity) -> Option<B> {
        let location = self.entities.get(entity)?;
        let bundle_id = self.bundles.register::<B>(&mut self.components);
        let bundle_info = self.bundles.info(bundle_id);
        let src_id = location.archetype;
        let dst_id = self.archetypes.remove_target(
            src_id,
            bundle_id,
            &bundle_info.sorted_ids,
            &self.components,
        )?;
        let src_row = location.row as usize;

        if src_id == dst_id {
            // Only removing the empty bundle leaves the archetype unchanged.
            // SAFETY: `B` has no components, so the closure is never called.
            return Some(unsafe { B::from_components(&mut || unreachable!()) });
        }

        // No user code runs below, so no step needs to come before another.
        let (src, dst) = self.archetypes.get_two_mut(src_id, dst_id);
        let dst_row = u32::try_from(dst.len()).expect("too many entities");
        // SAFETY: the row is in bounds, and `dst` has exactly the components of
        // `src` that are not in the bundle, so the new row is complete.
        let moved = unsafe { src.move_row_to(src_row, dst, &bundle_info.sorted_ids) };
        // SAFETY: `src_row` is still in the columns of the bundle's
        // components, which `src` has.
        let bundle = unsafe { src.take_bundle(src_row, &bundle_info.component_ids) };
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype: dst_id,
                row: dst_row,
            },
        );
        if let Some(moved) = moved {
            self.entities.set_location(moved, location);
        }
        src.debug_assert_consistent();
        dst.debug_assert_consistent();
        Some(bundle)
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

    #[derive(Clone, Debug, PartialEq)]
    struct Pos(i32);
    impl Component for Pos {}

    #[derive(Clone, Debug, PartialEq)]
    struct Vel(i32);
    impl Component for Vel {}

    #[derive(Clone, Debug, PartialEq)]
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

    /// Panics when dropped while armed.
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

    #[test]
    fn insert_adds_components() {
        let mut world = World::new();
        let a = world.spawn(Pos(1));
        let b = world.spawn(Pos(2));

        assert_eq!(world.insert(a, (Vel(10), Marker)), Ok(()));
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(10)));
        assert_eq!(world.get::<Marker>(a), Some(&Marker));
        // `b` moved into the row `a` left behind.
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));
        assert_eq!(world.get::<Vel>(b), None);
    }

    #[test]
    fn insert_overwrites_existing_components() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        let entity = world.spawn((Pos(1), Tracked(drops.clone())));

        // Every component already exists: overwritten in place.
        world.insert(entity, Tracked(drops.clone())).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        // Some exist and some don't: the entity moves, and the old `Tracked`
        // is dropped while `Pos` is carried over.
        world
            .insert(entity, (Tracked(drops.clone()), Vel(5)))
            .unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(world.get::<Pos>(entity), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(entity), Some(&Vel(5)));

        drop(world);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn insert_into_a_dead_entity_drops_the_bundle() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        let entity = world.spawn(());
        world.despawn(entity).unwrap();

        let result = world.insert(entity, Tracked(drops.clone()));
        assert_eq!(result, Err(NoSuchEntity(entity)));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn remove_returns_the_components() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Vel(2), Name("a".into())));
        let b = world.spawn((Pos(3), Vel(4), Name("b".into())));

        let removed = world.remove::<(Name, Pos)>(a);
        assert_eq!(removed, Some((Name("a".into()), Pos(1))));
        assert_eq!(world.get::<Pos>(a), None);
        assert_eq!(world.get::<Name>(a), None);
        assert_eq!(world.get::<Vel>(a), Some(&Vel(2)));
        assert_eq!(world.get::<Name>(b), Some(&Name("b".into())));
    }

    #[test]
    fn remove_fails_without_changing_anything() {
        let mut world = World::new();
        let entity = world.spawn((Pos(1), Vel(2)));

        assert_eq!(world.remove::<(Pos, Name)>(entity), None);
        assert_eq!(world.get::<Pos>(entity), Some(&Pos(1)));
        assert_eq!(world.remove::<()>(entity), Some(()));

        world.despawn(entity).unwrap();
        assert_eq!(world.remove::<Pos>(entity), None);
    }

    #[test]
    fn archetypes_are_reused() {
        let mut world = World::new();
        let entity = world.spawn(Pos(0));
        world.insert(entity, Vel(0)).unwrap();
        world.remove::<Vel>(entity).unwrap();
        let archetypes = world.archetypes.len();

        for _ in 0..10 {
            world.insert(entity, Vel(0)).unwrap();
            world.remove::<Vel>(entity).unwrap();
        }
        world.spawn((Vel(1), Pos(1)));
        assert_eq!(world.archetypes.len(), archetypes);
    }

    #[test]
    fn panicking_destructor_during_insert_leaves_the_world_consistent() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Bomb { armed: true }));
        let b = world.spawn((Pos(2), Bomb { armed: false }));

        // The armed bomb is replaced while the entity moves to a new
        // archetype, so it goes off after the move is complete.
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            world.insert(a, (Bomb { armed: false }, Vel(1))).unwrap();
        }));
        assert!(result.is_err());
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(1)));
        assert_eq!(world.get::<Bomb>(a).map(|bomb| bomb.armed), Some(false));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));

        // The same, but overwriting in place.
        world.insert(b, Bomb { armed: true }).unwrap();
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            world.insert(b, Bomb { armed: false }).unwrap();
        }));
        assert!(result.is_err());
        assert_eq!(world.get::<Bomb>(b).map(|bomb| bomb.armed), Some(false));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));
    }

    /// Applies a deterministic pseudo-random mix of operations and checks the
    /// world against a straightforward model after each batch.
    #[test]
    fn matches_a_model_under_random_operations() {
        #[derive(Default)]
        struct Expected {
            pos: Option<Pos>,
            vel: Option<Vel>,
            name: Option<Name>,
        }

        fn check(world: &World, alive: &[(Entity, Expected)], dead: &[Entity]) {
            assert_eq!(world.len() as usize, alive.len());
            for (entity, expected) in alive {
                assert_eq!(world.get::<Pos>(*entity), expected.pos.as_ref());
                assert_eq!(world.get::<Vel>(*entity), expected.vel.as_ref());
                assert_eq!(world.get::<Name>(*entity), expected.name.as_ref());
            }
            assert!(dead.iter().all(|&entity| !world.contains(entity)));
        }

        // A tiny xorshift generator keeps the test reproducible without a
        // dependency.
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut random = |n: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % n as u64) as usize
        };

        let mut world = World::new();
        let mut alive: Vec<(Entity, Expected)> = Vec::new();
        let mut dead = Vec::new();
        let steps = if cfg!(miri) { 400 } else { 10_000 };
        for value in 0..steps {
            let target = (!alive.is_empty()).then(|| random(alive.len()));
            match (random(7), target) {
                (0, _) | (_, None) => {
                    let entity = world.spawn((Pos(value), Vel(-value)));
                    let expected = Expected {
                        pos: Some(Pos(value)),
                        vel: Some(Vel(-value)),
                        name: None,
                    };
                    alive.push((entity, expected));
                }
                (1, _) => {
                    let entity = world.spawn(Name(value.to_string()));
                    let expected = Expected {
                        name: Some(Name(value.to_string())),
                        ..Expected::default()
                    };
                    alive.push((entity, expected));
                }
                (2, Some(i)) => {
                    let (entity, _) = alive.swap_remove(i);
                    world.despawn(entity).unwrap();
                    dead.push(entity);
                }
                (3, Some(i)) => {
                    let (entity, expected) = &mut alive[i];
                    world.insert(*entity, Name(value.to_string())).unwrap();
                    expected.name = Some(Name(value.to_string()));
                }
                (4, Some(i)) => {
                    let (entity, expected) = &mut alive[i];
                    world.insert(*entity, (Vel(value), Pos(value))).unwrap();
                    expected.vel = Some(Vel(value));
                    expected.pos = Some(Pos(value));
                }
                (5, Some(i)) => {
                    let (entity, expected) = &mut alive[i];
                    assert_eq!(world.remove::<Pos>(*entity), expected.pos.take());
                }
                (_, Some(i)) => {
                    let (entity, expected) = &mut alive[i];
                    let removed = world.remove::<(Name, Vel)>(*entity);
                    if expected.name.is_some() && expected.vel.is_some() {
                        let expected = (expected.name.take(), expected.vel.take());
                        assert_eq!(removed.map(|(n, v)| (Some(n), Some(v))), Some(expected));
                    } else {
                        assert_eq!(removed, None);
                    }
                }
            }
            if value % 100 == 0 {
                check(&world, &alive, &dead);
            }
        }
        check(&world, &alive, &dead);
    }
}
