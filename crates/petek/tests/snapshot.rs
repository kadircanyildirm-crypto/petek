//! Tests of snapshots, through the public API only.

use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use petek::command::CommandQueue;
use petek::query::{Changed, QueryState};
use petek::{Component, Entity, Resource, Snapshot, Tick, World};

#[derive(Clone, Debug, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Debug, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

#[derive(Clone, Debug, PartialEq)]
struct Shield(u32);
impl Component for Shield {}

#[derive(Clone, Debug, PartialEq)]
struct Tag(String);
impl Component for Tag {}

#[derive(Clone, Debug, PartialEq)]
struct Frame(u32);
impl Resource for Frame {}

#[derive(Clone, Debug, PartialEq)]
struct Alarm(usize);
impl Resource for Alarm {}

/// An entity and its components.
type Row = (
    Entity,
    Option<Pos>,
    Option<Vel>,
    Option<Shield>,
    Option<Tag>,
);

/// Everything about a world that can be observed through its public API,
/// with the entities in the order queries visit them.
#[derive(Debug, PartialEq)]
struct Dump {
    entities: Vec<Row>,
    frame: Option<Frame>,
    alarm: Option<Alarm>,
    tick: Tick,
}

fn dump(world: &World) -> Dump {
    let entities = world
        .query::<(
            Entity,
            Option<&Pos>,
            Option<&Vel>,
            Option<&Shield>,
            Option<&Tag>,
        )>()
        .into_iter()
        .map(|(entity, pos, vel, shield, tag)| {
            (
                entity,
                pos.cloned(),
                vel.cloned(),
                shield.cloned(),
                tag.cloned(),
            )
        })
        .collect();
    Dump {
        entities,
        frame: world.resource::<Frame>().cloned(),
        alarm: world.resource::<Alarm>().cloned(),
        tick: world.change_tick(),
    }
}

/// A world with the common types registered for snapshots.
fn new_world() -> World {
    let mut world = World::new();
    world.register_clone::<Pos>();
    world.register_clone::<Vel>();
    world.register_resource_clone::<Frame>();
    world
}

#[test]
fn restoring_brings_the_world_back() {
    let mut world = new_world();
    let a = world.spawn((Pos(1), Vel(1)));
    let b = world.spawn(Pos(2));
    world.insert_resource(Frame(7));
    let saved = world.snapshot();
    let before = dump(&world);
    assert_eq!(saved.change_tick(), world.change_tick());

    world.get_mut::<Pos>(a).unwrap().0 = 100;
    world.despawn(b).unwrap();
    world.insert(a, Pos(5)).unwrap();
    world.remove::<Vel>(a).unwrap();
    world.spawn_batch((0..10).map(Pos));
    world.remove_resource::<Frame>();
    assert_ne!(dump(&world), before);

    world.restore(&saved);
    assert_eq!(dump(&world), before);
    assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));

    // The snapshot is still there, and can be restored again.
    world.spawn(Pos(3));
    world.resource_mut::<Frame>().unwrap().0 = 0;
    world.restore(&saved);
    assert_eq!(dump(&world), before);
}

#[test]
fn entities_are_handed_out_as_they_were() {
    let mut world = new_world();
    let old = world.spawn(Pos(0));
    world.despawn(old).unwrap();
    let saved = world.snapshot();

    let first = world.spawn(Pos(1));
    let second = world.spawn(Pos(2));
    world.restore(&saved);

    // Handles spawned after the snapshot are gone, and get handed out again,
    // to whatever is spawned in the same order.
    assert!(!world.contains(first) && !world.contains(second));
    assert_eq!(world.spawn(Vel(1)), first);
    assert_eq!(world.spawn(Vel(2)), second);
    assert!(!world.contains(old));
}

#[test]
fn archetypes_created_since_the_snapshot_are_removed() {
    let mut world = new_world();
    world.register_clone::<Shield>();
    world.spawn(Pos(0));
    let saved = world.snapshot();
    let mut shielded = QueryState::<(Entity, &Shield)>::new(&mut world);
    let mut positions = QueryState::<&Pos>::new(&mut world);

    world.spawn((Pos(1), Shield(1)));
    world.spawn(Shield(2));
    assert_eq!(shielded.iter(&world).count(), 2);
    assert_eq!(positions.iter(&world).count(), 2);

    // The archetypes with shields go, and others take their ids. The query
    // states must not mistake them for the old ones.
    world.restore(&saved);
    world.spawn((Pos(3), Vel(3)));
    world.spawn(Vel(4));
    let unshielded = world.spawn(Pos(5));
    assert_eq!(shielded.iter(&world).count(), 0);
    let mut seen: Vec<_> = positions.iter(&world).map(|pos| pos.0).collect();
    seen.sort_unstable();
    assert_eq!(seen, [0, 3, 5]);

    world.insert(unshielded, Shield(5)).unwrap();
    let shields: Vec<_> = shielded.iter(&world).map(|(_, shield)| shield.0).collect();
    assert_eq!(shields, [5]);
}

#[test]
fn snapshots_from_abandoned_timelines_can_be_restored() {
    let mut world = new_world();
    world.register_clone::<Tag>();
    let start = world.snapshot();

    world.spawn((Pos(1), Tag("first".into())));
    let first = world.snapshot();
    let first_dump = dump(&world);

    // A different history, where other archetypes take the same ids.
    world.restore(&start);
    world.spawn((Vel(2), Tag("second".into())));
    world.spawn(Pos(2));
    let second = world.snapshot();
    let second_dump = dump(&world);

    let mut tags = QueryState::<&Tag>::new(&mut world);
    for _ in 0..2 {
        world.restore(&first);
        assert_eq!(dump(&world), first_dump);
        assert_eq!(tags.iter(&world).next(), Some(&Tag("first".into())));
        world.restore(&second);
        assert_eq!(dump(&world), second_dump);
        assert_eq!(tags.iter(&world).next(), Some(&Tag("second".into())));
    }
}

/// An input to [`step`].
#[derive(Clone, Copy, Debug)]
enum Input {
    Spawn(i32),
    GiveShield,
    TagLast,
    SoundAlarm,
    DespawnFirst,
    Idle,
}

/// One frame of a small simulation, driven by `input`.
fn step(world: &mut World, movement: &mut QueryState<(&mut Pos, &Vel)>, input: Input) {
    for (mut pos, vel) in movement.iter_mut(world) {
        pos.0 += vel.0;
    }
    match input {
        Input::Spawn(x) => {
            world.spawn((Pos(x), Vel(1)));
        }
        Input::GiveShield => {
            let target = world
                .query::<Entity>()
                .without::<Shield>()
                .into_iter()
                .next();
            if let Some(entity) = target {
                world.insert(entity, Shield(3)).unwrap();
            }
        }
        Input::TagLast => {
            let target = world.query::<Entity>().into_iter().last();
            if let Some(entity) = target {
                world.insert(entity, Tag(format!("{entity}"))).unwrap();
            }
        }
        Input::SoundAlarm => {
            world.insert_resource(Alarm(world.len()));
        }
        Input::DespawnFirst => {
            let target = world.query::<Entity>().into_iter().next();
            if let Some(entity) = target {
                world.despawn(entity).unwrap();
            }
        }
        Input::Idle => {}
    }
    world.resource_mut::<Frame>().unwrap().0 += 1;
}

#[test]
fn rolling_back_and_replaying_matches_never_having_mispredicted() {
    use Input::{DespawnFirst, GiveShield, Idle, SoundAlarm, Spawn, TagLast};
    let inputs = [
        Spawn(1),
        Spawn(2),
        Spawn(3),
        Idle,
        Spawn(4),
        // The rollback happens from here,
        DespawnFirst,
        Idle,
        TagLast,
        GiveShield,
        SoundAlarm,
        // up to here.
        Spawn(5),
        GiveShield,
        TagLast,
        DespawnFirst,
        SoundAlarm,
    ];
    // The wrong guesses use the shield, tag and alarm types first, and
    // create archetypes the real inputs never do.
    let guesses = [GiveShield, SoundAlarm, Spawn(9), TagLast, GiveShield];

    let start = || {
        let mut world = new_world();
        world.insert_resource(Frame(0));
        let movement = QueryState::new(&mut world);
        (world, movement)
    };

    // One world gets every input on time.
    let (mut on_time, mut movement) = start();
    for &input in &inputs {
        step(&mut on_time, &mut movement, input);
    }

    // The other guesses five inputs wrong, and rolls back when they arrive.
    let (mut world, mut movement) = start();
    for &input in &inputs[..5] {
        step(&mut world, &mut movement, input);
    }
    let saved = world.snapshot();
    for guess in guesses {
        step(&mut world, &mut movement, guess);
    }
    world.restore(&saved);
    for &input in &inputs[5..] {
        step(&mut world, &mut movement, input);
    }

    assert_eq!(dump(&world), dump(&on_time));
    // Spawning still hands out the same entities.
    assert_eq!(world.spawn(Pos(0)), on_time.spawn(Pos(0)));
}

#[test]
fn a_cloned_query_state_rolls_back_with_the_world() {
    let mut world = new_world();
    let entities: Vec<_> = (0..4).map(|i| world.spawn(Pos(i))).collect();
    let mut changed = QueryState::<Entity, Changed<Pos>>::new(&mut world);
    assert_eq!(changed.iter(&world).count(), 4);

    let saved = (world.snapshot(), changed.clone());
    world.get_mut::<Pos>(entities[0]).unwrap().0 += 1;
    assert_eq!(changed.iter(&world).collect::<Vec<_>>(), [entities[0]]);

    world.restore(&saved.0);
    changed = saved.1.clone();
    world.get_mut::<Pos>(entities[2]).unwrap().0 += 1;
    assert_eq!(changed.iter(&world).collect::<Vec<_>>(), [entities[2]]);
}

#[test]
fn reserved_entities_are_part_of_the_snapshot() {
    let mut world = new_world();
    let mut queue = CommandQueue::new();
    let reserved = queue.commands(&world).spawn(Pos(1));
    let saved = world.snapshot();

    queue.apply(&mut world);
    assert_eq!(world.get::<Pos>(reserved), Some(&Pos(1)));

    // Back to when the entity was reserved, but its commands not applied.
    world.restore(&saved);
    assert!(!world.contains(reserved));
    world.spawn(Vel(0));
    assert!(world.contains(reserved) && !world.has::<Pos>(reserved));
}

#[test]
#[should_panic(expected = "cannot snapshot a world holding `snapshot::Shield` components")]
fn components_must_be_registered() {
    let mut world = new_world();
    world.spawn((Pos(0), Shield(0)));
    let _ = world.snapshot();
}

#[test]
#[should_panic(expected = "cannot snapshot a world holding a `snapshot::Alarm` resource")]
fn resources_must_be_registered() {
    let mut world = new_world();
    world.insert_resource(Alarm(0));
    let _ = world.snapshot();
}

#[test]
fn only_what_the_world_holds_must_be_registered() {
    let mut world = new_world();
    let entity = world.spawn((Pos(0), Shield(0)));
    world.insert_resource(Alarm(0));
    world.despawn(entity).unwrap();
    world.remove_resource::<Alarm>();

    let saved = world.snapshot();
    world.spawn(Shield(1));
    world.restore(&saved);
    assert!(world.is_empty());
}

#[test]
#[should_panic(expected = "restored into a world other than the one it was taken of")]
fn snapshots_belong_to_one_world() {
    let saved = new_world().snapshot();
    new_world().restore(&saved);
}

/// Counts its drops in a shared counter, and its clones panic while
/// `FAIL_CLONES` is set.
struct Tracked(Arc<AtomicUsize>);
impl Component for Tracked {}
impl Resource for Tracked {}

static FAIL_CLONES: AtomicBool = AtomicBool::new(false);

impl Clone for Tracked {
    fn clone(&self) -> Self {
        assert!(!FAIL_CLONES.load(Ordering::Relaxed), "cloning failed");
        Self(self.0.clone())
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn components_are_dropped_exactly_once_and_failures_change_nothing() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut world = new_world();
    world.register_clone::<Tracked>();
    world.register_resource_clone::<Tracked>();
    for i in 0..5 {
        world.spawn((Pos(i), Tracked(drops.clone())));
    }
    world.insert_resource(Tracked(drops.clone()));
    let saved = world.snapshot();
    assert_eq!(drops.load(Ordering::Relaxed), 0);

    world.spawn(Tracked(drops.clone()));
    world.restore(&saved);
    // The 6 components and 1 resource the world held are gone.
    assert_eq!(drops.load(Ordering::Relaxed), 7);

    // A clone that panics leaves the world as it was, and leaks nothing.
    world.spawn(Tracked(drops.clone()));
    let before = dump(&world);
    FAIL_CLONES.store(true, Ordering::Relaxed);
    let restored = panic::catch_unwind(AssertUnwindSafe(|| world.restore(&saved)));
    let taken = panic::catch_unwind(AssertUnwindSafe(|| drop(world.snapshot())));
    FAIL_CLONES.store(false, Ordering::Relaxed);
    assert!(restored.is_err() && taken.is_err());
    assert_eq!(dump(&world), before);
    assert_eq!(world.len(), 6);
    assert_eq!(drops.load(Ordering::Relaxed), 7);

    drop(world);
    assert_eq!(drops.load(Ordering::Relaxed), 7 + 7);
    drop(saved);
    assert_eq!(drops.load(Ordering::Relaxed), 7 + 7 + 6);
}

#[test]
fn snapshots_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Snapshot>();
}
