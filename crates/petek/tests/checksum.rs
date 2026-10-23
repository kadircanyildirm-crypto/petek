//! Tests of checksums, through the public API only.

use petek::query::QueryState;
use petek::{Component, Entity, Resource, World};

#[derive(Clone, Debug, Hash, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Debug, Hash, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

/// Not registered for checksums.
#[derive(Debug, PartialEq)]
struct Sprite(u32);
impl Component for Sprite {}

#[derive(Debug, Hash, PartialEq)]
struct Seed(u64);
impl Resource for Seed {}

fn new_world() -> World {
    let mut world = World::new();
    world.register_hash::<Pos>();
    world.register_hash::<Vel>();
    world.register_resource_hash::<Seed>();
    world
}

/// The checksum of a small world, after `change` is applied to it and its
/// first entity.
fn checksum_after(change: impl FnOnce(&mut World, Entity)) -> u64 {
    let mut world = new_world();
    let first = world.spawn((Pos(1), Vel(1)));
    world.spawn((Pos(2), Sprite(2)));
    world.insert_resource(Seed(1));
    change(&mut world, first);
    world.checksum()
}

#[test]
fn the_same_operations_give_the_same_checksum() {
    let run = || {
        let mut world = new_world();
        let entities: Vec<_> = (0..10).map(|i| world.spawn((Pos(i), Vel(-i)))).collect();
        world.despawn(entities[3]).unwrap();
        world.insert(entities[4], Sprite(1)).unwrap();
        world.insert_resource(Seed(42));
        world.checksum()
    };
    assert_eq!(run(), run());
    assert_eq!(checksum_after(|_, _| {}), checksum_after(|_, _| {}));
}

#[test]
fn registered_components_and_resources_are_covered() {
    let pos = |value| checksum_after(move |world, first| world.insert(first, Pos(value)).unwrap());
    assert_ne!(pos(5), pos(6));
    let vel = |value| checksum_after(move |world, first| world.insert(first, Vel(value)).unwrap());
    assert_ne!(vel(5), vel(6));
    // Inserting a resource does not move the world's tick.
    let seed = |value| {
        checksum_after(move |world, _| {
            world.insert_resource(Seed(value));
        })
    };
    assert_ne!(seed(5), seed(6));
    assert_eq!(seed(1), checksum_after(|_, _| {}));
    // A missing resource differs from any value, even zero.
    let removed = checksum_after(|world, _| {
        world.remove_resource::<Seed>();
    });
    assert_ne!(removed, seed(0));
}

#[test]
fn other_components_are_left_out() {
    let sprite =
        |value| checksum_after(move |world, first| world.insert(first, Sprite(value)).unwrap());
    assert_eq!(sprite(5), sprite(6));
}

#[test]
fn entities_and_their_order_are_covered() {
    // The same entities, stored in a different order: moving an entity out
    // of its archetype and back puts it last.
    let moved = |index: usize| {
        let mut world = new_world();
        let entities = [world.spawn(Pos(1)), world.spawn(Pos(2))];
        world.insert(entities[index], Sprite(0)).unwrap();
        world.remove::<Sprite>(entities[index]).unwrap();
        world.checksum()
    };
    assert_ne!(moved(0), moved(1));

    // No entities, but the next one spawned would differ.
    let freed = |reverse: bool| {
        let mut world = new_world();
        let mut entities = [world.spawn(()), world.spawn(())];
        if reverse {
            entities.reverse();
        }
        for entity in entities {
            world.despawn(entity).unwrap();
        }
        world.checksum()
    };
    assert_ne!(freed(false), freed(true));

    // The same components, on an entity with a different component set.
    let tagged = checksum_after(|world, first| world.insert(first, Sprite(0)).unwrap());
    let untagged = checksum_after(|world, first| world.insert(first, Pos(1)).unwrap());
    assert_ne!(tagged, untagged);
}

#[test]
fn the_tick_is_covered() {
    let read = checksum_after(|world, first| {
        let _ = world.get_mut::<Pos>(first);
    });
    assert_ne!(read, checksum_after(|_, _| {}));
}

#[test]
fn checksums_do_not_depend_on_component_ids() {
    // Using `Vel` before registering anything gives it the first id, so the
    // two worlds store `Pos` and `Vel` in a different order.
    let mut world = World::new();
    let _ = QueryState::<&Vel>::new(&mut world);
    world.register_hash::<Pos>();
    world.register_hash::<Vel>();
    world.register_resource_hash::<Seed>();

    let mut other = new_world();
    for world in [&mut world, &mut other] {
        world.spawn((Pos(1), Vel(2), Sprite(3)));
        world.spawn((Vel(4), Pos(5)));
    }
    assert_eq!(world.checksum(), other.checksum());
}
