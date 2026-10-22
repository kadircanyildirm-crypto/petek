//! Tests of `World` through the public API only.

use std::any::type_name;
use std::ptr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use petek::{Component, Entity, World};

#[derive(Clone, Debug, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Debug, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

#[derive(Clone, Debug, PartialEq)]
struct Name(String);
impl Component for Name {}

/// Counts its drops in a shared counter.
struct Tracked(Arc<AtomicUsize>);
impl Component for Tracked {}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn spawn_batch_spawns_as_it_goes_and_finishes_when_dropped() {
    let mut world = World::new();
    let mut batch = world.spawn_batch((0..10).map(Pos));
    assert_eq!(batch.len(), 10);
    let first: Vec<Entity> = batch.by_ref().take(3).collect();
    assert_eq!(batch.len(), 7);
    drop(batch);

    assert_eq!(world.len(), 10);
    assert_eq!(world.get::<Pos>(first[2]), Some(&Pos(2)));
}

#[test]
fn spawn_batch_joins_an_existing_archetype() {
    let mut world = World::new();
    let before = world.spawn((Pos(-1), Vel(-1)));
    let entities: Vec<_> = world
        .spawn_batch((0..100).map(|i| (Vel(i), Pos(i))))
        .collect();

    assert_eq!(world.get::<Pos>(before), Some(&Pos(-1)));
    for (i, entity) in (0..).zip(entities) {
        assert_eq!(world.get::<Vel>(entity), Some(&Vel(i)));
    }
}

#[test]
fn clear_despawns_everything_and_invalidates_handles() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut world = World::new();
    let entities: Vec<_> = (0..10)
        .map(|i| world.spawn((Pos(i), Tracked(drops.clone()))))
        .collect();
    world.spawn(Name("loner".into()));

    world.clear();
    assert!(world.is_empty());
    assert_eq!(drops.load(Ordering::Relaxed), 10);
    assert!(entities.iter().all(|&entity| !world.contains(entity)));

    let new = world.spawn(Pos(42));
    assert!(!entities.contains(&new));
    assert_eq!(world.get::<Pos>(new), Some(&Pos(42)));
}

#[test]
fn entities_without_components_can_gain_and_lose_them() {
    let mut world = World::new();
    let entity = world.spawn(());
    world.insert(entity, (Pos(1), Name("x".into()))).unwrap();
    assert_eq!(world.remove::<Pos>(entity), Ok(Pos(1)));
    assert_eq!(world.remove::<Name>(entity), Ok(Name("x".into())));
    assert!(world.contains(entity));

    world.insert(entity, ()).unwrap();
    world.despawn(entity).unwrap();
    assert!(world.is_empty());
}

#[test]
fn unusual_component_types() {
    static FLAG_DROPS: AtomicUsize = AtomicUsize::new(0);

    #[derive(Debug, PartialEq)]
    #[repr(align(128))]
    struct Aligned(u8);
    impl Component for Aligned {}

    /// A zero-sized component with a destructor.
    struct Flag;
    impl Component for Flag {}

    impl Drop for Flag {
        fn drop(&mut self) {
            FLAG_DROPS.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut world = World::new();
    let entities: Vec<_> = (0..20).map(|i| world.spawn((Aligned(i), Flag))).collect();
    for (i, &entity) in (0..).zip(&entities) {
        let aligned = world.get::<Aligned>(entity).unwrap();
        assert_eq!(aligned, &Aligned(i));
        assert_eq!(ptr::from_ref(aligned).addr() % 128, 0);
    }

    drop(world.remove::<Flag>(entities[0]).unwrap());
    world.despawn(entities[1]).unwrap();
    assert_eq!(FLAG_DROPS.load(Ordering::Relaxed), 2);
    drop(world);
    assert_eq!(FLAG_DROPS.load(Ordering::Relaxed), 20);
}

#[test]
fn large_and_nested_bundles() {
    macro_rules! components {
        ($($name:ident),*) => {$(
            #[derive(Debug, PartialEq)]
            struct $name(u8);
            impl Component for $name {}
        )*};
    }
    components!(C0, C1, C2, C3, C4, C5, C6, C7, C8, C9, C10, C11);

    let mut world = World::new();
    let entity = world.spawn((
        C0(0),
        C1(1),
        C2(2),
        C3(3),
        C4(4),
        C5(5),
        C6(6),
        C7(7),
        C8(8),
        C9(9),
        C10(10),
        C11(11),
    ));
    assert_eq!(world.get::<C11>(entity), Some(&C11(11)));

    let nested = world.remove::<((C0, C1), (C2, (C3, C4)))>(entity).unwrap();
    assert_eq!(nested, ((C0(0), C1(1)), (C2(2), (C3(3), C4(4)))));
    assert!(!world.has::<C3>(entity));
    assert_eq!(world.get::<C5>(entity), Some(&C5(5)));
}

#[test]
fn errors_describe_the_problem() {
    let mut world = World::new();
    let entity = world.spawn(Pos(0));

    let error = world.remove::<Vel>(entity).unwrap_err();
    let expected = format!("entity {entity} has no `{}` component", type_name::<Vel>());
    assert_eq!(error.to_string(), expected);

    world.despawn(entity).unwrap();
    let error = world.despawn(entity).unwrap_err();
    assert_eq!(error.to_string(), format!("entity {entity} does not exist"));
}

#[test]
fn forged_and_stale_handles_are_rejected() {
    let mut world = World::new();
    let entity = world.spawn(Pos(0));
    // The same slot, one generation ahead.
    let forged = Entity::from_bits(entity.to_bits() + 0x1_0000_0000).unwrap();

    assert!(!world.contains(forged));
    assert_eq!(world.get::<Pos>(forged), None);
    assert!(world.insert(forged, Vel(0)).is_err());
    assert!(world.remove::<Pos>(forged).is_err());
    assert!(world.despawn(forged).is_err());
    assert_eq!(world.get::<Pos>(entity), Some(&Pos(0)));
}

/// The components the model expects an entity to have.
#[derive(Default)]
struct Expected {
    pos: Option<Pos>,
    vel: Option<Vel>,
    name: Option<Name>,
}

/// Checks the world against the model.
fn check(world: &World, alive: &[(Entity, Expected)], dead: &[Entity]) {
    assert_eq!(world.len(), alive.len());
    for (entity, expected) in alive {
        assert_eq!(world.get::<Pos>(*entity), expected.pos.as_ref());
        assert_eq!(world.get::<Vel>(*entity), expected.vel.as_ref());
        assert_eq!(world.get::<Name>(*entity), expected.name.as_ref());
    }
    assert!(dead.iter().all(|&entity| !world.contains(entity)));

    // A query visits every entity that has the components exactly once.
    let mut queried: Vec<_> = world
        .query::<(Entity, &Pos, Option<&Name>)>()
        .without::<Vel>()
        .into_iter()
        .map(|(entity, pos, name)| (entity, pos.0, name.cloned()))
        .collect();
    queried.sort_by_key(|&(entity, ..)| entity);
    let mut expected: Vec<_> = alive
        .iter()
        .filter(|(_, expected)| expected.vel.is_none())
        .filter_map(|(entity, expected)| {
            let pos = expected.pos.as_ref()?;
            Some((*entity, pos.0, expected.name.clone()))
        })
        .collect();
    expected.sort_by_key(|&(entity, ..)| entity);
    assert_eq!(queried, expected);
}

/// Applies a reproducible pseudo-random mix of operations and checks the
/// world against a straightforward model after every batch of them.
#[test]
fn matches_a_model_under_random_operations() {
    // A tiny xorshift generator keeps the test reproducible without a
    // dependency.
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut random = |n: usize| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        usize::try_from(state % n as u64).unwrap()
    };

    let mut world = World::new();
    let mut alive: Vec<(Entity, Expected)> = Vec::new();
    let mut dead = Vec::new();
    let steps = if cfg!(miri) { 400 } else { 10_000 };
    for value in 0..steps {
        let target = (!alive.is_empty()).then(|| random(alive.len()));
        match (random(8), target) {
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
                assert_eq!(world.remove::<Pos>(*entity).ok(), expected.pos.take());
            }
            (6, Some(_)) if value % 1000 == 999 => {
                world.clear();
                dead.extend(alive.drain(..).map(|(entity, _)| entity));
            }
            (6, Some(_)) => {
                for (pos, vel) in world.query_mut::<(&mut Pos, &Vel)>() {
                    pos.0 = pos.0.wrapping_add(vel.0);
                }
                for (_, expected) in &mut alive {
                    if let (Some(pos), Some(vel)) = (&mut expected.pos, &expected.vel) {
                        pos.0 = pos.0.wrapping_add(vel.0);
                    }
                }
            }
            (_, Some(i)) => {
                let (entity, expected) = &mut alive[i];
                let removed = world.remove::<(Name, Vel)>(*entity).ok();
                if expected.name.is_some() && expected.vel.is_some() {
                    let expected = (expected.name.take(), expected.vel.take());
                    assert_eq!(
                        removed.map(|(name, vel)| (Some(name), Some(vel))),
                        Some(expected)
                    );
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
