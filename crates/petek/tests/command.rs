//! Tests of commands, through the public API only.

use std::panic::{self, AssertUnwindSafe};

use petek::command::CommandQueue;
use petek::{Component, Entity, Resource, World};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

#[derive(Debug, PartialEq)]
struct Score(u32);
impl Resource for Score {}

#[test]
fn commands_change_the_world_once_applied() {
    let mut world = World::new();
    let a = world.spawn(Pos(1));
    let b = world.spawn((Pos(2), Vel(2)));

    let mut queue = CommandQueue::new();
    let mut commands = queue.commands(&world);
    for (entity, pos) in world.query::<(Entity, &Pos)>() {
        commands.insert(entity, Vel(pos.0 * 10));
    }
    commands.remove::<Pos>(b);
    let c = commands.spawn((Pos(3), Vel(30)));
    commands.insert_resource(Score(1));
    assert_eq!(queue.len(), 5);

    // Nothing happens until the queue is applied.
    assert!(!world.has::<Vel>(a));
    assert!(!world.contains(c));
    queue.apply(&mut world);
    assert!(queue.is_empty());

    assert_eq!(world.get::<Vel>(a), Some(&Vel(10)));
    assert_eq!(world.get::<Vel>(b), Some(&Vel(20)));
    assert!(!world.has::<Pos>(b));
    assert_eq!(world.get::<Pos>(c), Some(&Pos(3)));
    assert_eq!(world.resource::<Score>(), Some(&Score(1)));
}

#[test]
fn commands_apply_in_order() {
    let mut world = World::new();
    let a = world.spawn(Pos(0));

    let mut queue = CommandQueue::new();
    let mut commands = queue.commands(&world);
    let b = commands.spawn_empty();
    commands.insert(b, Pos(1));
    commands.insert(b, Vel(1));
    commands.remove::<Pos>(b);
    commands.despawn(a);
    // Commands on entities that are gone by then do nothing.
    commands.insert(a, Vel(2));
    commands.remove::<Vel>(a);
    commands.add(move |world| world.get_mut::<Vel>(b).unwrap().0 += 10);
    commands.remove_resource::<Score>();
    queue.apply(&mut world);

    assert!(!world.contains(a));
    assert!(!world.has::<Pos>(b));
    assert_eq!(world.get::<Vel>(b), Some(&Vel(11)));
    assert_eq!(world.len(), 1);
}

#[test]
fn spawned_entities_reuse_free_slots() {
    let mut world = World::new();
    let old: Vec<_> = (0..3).map(Pos).map(|pos| world.spawn(pos)).collect();
    world.despawn(old[1]).unwrap();

    let mut queue = CommandQueue::new();
    let mut commands = queue.commands(&world);
    let reused = commands.spawn(Pos(10));
    let new = commands.spawn(Pos(11));
    queue.apply(&mut world);

    assert_eq!(reused.index(), old[1].index());
    assert_eq!(reused.generation(), old[1].generation() + 1);
    assert!(!world.contains(old[1]));
    assert_eq!(world.get::<Pos>(reused), Some(&Pos(10)));
    assert_eq!(world.get::<Pos>(new), Some(&Pos(11)));
}

#[test]
fn reserved_entities_come_alive_at_the_next_structural_change() {
    let mut world = World::new();
    let mut queue = CommandQueue::new();
    let reserved = queue.commands(&world).spawn(Pos(1));
    drop(queue);

    // The queue is gone, so the entity never gets its components, but it
    // still exists once the world changes.
    assert!(!world.contains(reserved));
    let spawned = world.spawn(Pos(2));
    assert_ne!(spawned, reserved);
    assert!(world.contains(reserved));
    assert!(!world.has::<Pos>(reserved));
    assert_eq!(world.len(), 2);
}

#[test]
fn a_panicking_command_leaves_the_world_consistent() {
    let mut world = World::new();
    let mut queue = CommandQueue::new();
    let mut commands = queue.commands(&world);
    let a = commands.spawn(Pos(1));
    commands.add(|_| panic!("boom"));
    let b = commands.spawn(Pos(2));

    let result = panic::catch_unwind(AssertUnwindSafe(|| queue.apply(&mut world)));
    assert!(result.is_err());
    assert!(queue.is_empty());

    // Commands before the panic were applied. The entity spawned after it
    // exists, as it was reserved, but never got its components.
    assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
    assert!(world.contains(b) && !world.has::<Pos>(b));
}

#[test]
#[should_panic(expected = "world other than the one its commands were recorded for")]
fn queues_belong_to_one_world() {
    let world = World::new();
    let mut queue = CommandQueue::new();
    queue.commands(&world).spawn(Pos(1));
    queue.apply(&mut World::new());
}

#[test]
fn applying_the_same_commands_gives_the_same_world() {
    fn run() -> Vec<(Entity, i32)> {
        let mut world = World::new();
        let entities: Vec<_> = (0..10).map(|i| world.spawn(Pos(i))).collect();
        for &entity in entities.iter().step_by(3) {
            world.despawn(entity).unwrap();
        }

        let mut queue = CommandQueue::new();
        let mut commands = queue.commands(&world);
        for (entity, pos) in world.query::<(Entity, &Pos)>() {
            if pos.0 % 2 == 0 {
                commands.despawn(entity);
                commands.spawn((Pos(pos.0 + 100), Vel(0)));
            }
        }
        queue.apply(&mut world);

        let mut state: Vec<_> = world
            .query::<(Entity, &Pos)>()
            .iter()
            .map(|(entity, pos)| (entity, pos.0))
            .collect();
        state.sort_unstable();
        state
    }

    assert_eq!(run(), run());
}

#[test]
fn commands_work_alongside_a_mutable_query() {
    let mut world = World::new();
    let a = world.spawn((Pos(1), Vel(-1)));
    let b = world.spawn((Pos(5), Vel(-1)));

    let mut queue = CommandQueue::new();
    let mut query = world.query_mut::<(Entity, &mut Pos, &Vel)>();
    let mut commands = query.commands(&mut queue);
    for (entity, mut pos, vel) in &mut query {
        pos.0 += vel.0;
        if pos.0 == 0 {
            commands.despawn(entity);
            let child = commands.spawn(Pos(100));
            commands.insert(child, Vel(0));
        }
    }
    // The query is still usable while the commands wait.
    assert_eq!(query.iter_mut().count(), 2);
    queue.apply(&mut world);

    assert!(!world.contains(a));
    assert_eq!(world.get::<Pos>(b), Some(&Pos(4)));
    let children: Vec<_> = world.query::<(&Pos, &Vel)>().with::<Vel>().iter().collect();
    assert!(children.contains(&(&Pos(100), &Vel(0))));
}

#[test]
fn an_empty_queue_can_move_between_worlds() {
    let first = World::new();
    let mut second = World::new();
    let mut queue = CommandQueue::new();
    // Binds the queue to the first world, but records nothing.
    queue.commands(&first);

    queue.commands(&second).spawn(Pos(1));
    queue.apply(&mut second);
    assert_eq!(second.len(), 1);
}
