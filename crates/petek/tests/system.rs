//! Tests of systems, through the public API only.

use std::panic;
use std::sync::{Arc, Mutex};

use petek::command::Commands;
use petek::query::{Added, Changed, Query, With};
use petek::schedule::Schedule;
use petek::system::{Res, ResMut};
use petek::{Component, Entity, Resource, World};

#[derive(Clone, Debug, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Debug, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

#[derive(Clone, Debug, PartialEq)]
struct Frame(u32);
impl Resource for Frame {}

#[derive(Clone, Debug, PartialEq)]
struct Log(Vec<String>);
impl Resource for Log {}

#[derive(Debug, PartialEq)]
struct Tag(u32);
impl Component for Tag {}

/// A resource no world in these tests holds.
struct Missing;
impl Resource for Missing {}

/// Runs `system` once, alone in a schedule.
fn run_once<M>(world: &mut World, system: impl petek::system::IntoSystem<M>) {
    let mut schedule = Schedule::new();
    schedule.add_system(system);
    schedule.run(world);
}

#[test]
fn systems_get_what_they_ask_for() {
    fn system(
        frame: Res<Frame>,
        mut log: ResMut<Log>,
        missing: Option<Res<Missing>>,
        query: Query<(Entity, &mut Pos, &Vel)>,
        mut commands: Commands,
    ) {
        assert!(missing.is_none());
        for (entity, mut pos, vel) in query {
            pos.0 += vel.0;
            log.0.push(format!("{entity} moved in frame {}", frame.0));
            commands.insert(entity, Tag(frame.0));
        }
    }

    let mut world = World::new();
    world.insert_resource(Frame(7));
    world.insert_resource(Log(Vec::new()));
    let moving = world.spawn((Pos(0), Vel(2)));
    world.spawn(Pos(5));

    run_once(&mut world, system);
    assert_eq!(world.get::<Pos>(moving), Some(&Pos(2)));
    assert_eq!(
        world.resource::<Log>().unwrap().0,
        [format!("{moving} moved in frame 7")]
    );
    // The commands were applied once the system finished.
    assert_eq!(world.get::<Tag>(moving), Some(&Tag(7)));
}

#[test]
fn systems_can_read_the_whole_world_or_take_it_exclusively() {
    let mut world = World::new();
    let entity = world.spawn(Pos(1));
    let seen = Arc::new(Mutex::new(Vec::new()));

    let mut schedule = Schedule::new();
    let reader = seen.clone();
    schedule
        .add_system(move |world: &World| reader.lock().unwrap().push(world.len()))
        .add_system(move |world: &mut World| {
            world.spawn(Pos(2));
            world.get_mut::<Pos>(entity).unwrap().0 = 10;
        });
    schedule.run(&mut world);
    schedule.run(&mut world);
    assert_eq!(*seen.lock().unwrap(), [1, 2]);
    assert_eq!(world.get::<Pos>(entity), Some(&Pos(10)));
}

#[test]
#[should_panic(expected = "parameters that both access `system::Pos`")]
fn parameters_that_alias_are_rejected() {
    run_once(&mut World::new(), |_: Query<&mut Pos>, _: Query<&Pos>| {});
}

#[test]
#[should_panic(expected = "parameters that both access the `system::Frame` resource")]
fn resource_parameters_that_alias_are_rejected() {
    run_once(&mut World::new(), |_: Res<Frame>, _: ResMut<Frame>| {});
}

#[test]
#[should_panic(expected = "parameters that both access the world")]
fn the_world_cannot_be_read_while_written() {
    run_once(&mut World::new(), |_: &World, _: Query<&mut Pos>| {});
}

#[test]
fn a_filter_reading_ticks_conflicts_with_a_write() {
    // `Changed<Pos>` reads the ticks a `Query<&mut Pos>` writes.
    let result = panic::catch_unwind(|| {
        run_once(
            &mut World::new(),
            |_: Query<&mut Pos>, _: Query<Entity, Changed<Pos>>| {},
        );
    });
    assert!(result.is_err());
    // Within one query, it is allowed: rows are checked before they are
    // handed out.
    run_once(&mut World::new(), |_: Query<&mut Pos, Changed<Pos>>| {});
}

#[test]
#[should_panic(expected = "needs a `system::Frame` resource, which the world does not hold")]
fn missing_resources_are_reported() {
    run_once(&mut World::new(), |_: Res<Frame>| {});
}

#[test]
fn change_detection_compares_with_the_last_run_of_each_system() {
    let mut world = World::new();
    let [a, b] = [0, 1].map(|i| world.spawn(Pos(i)));
    world.insert_resource(Frame(0));
    world.insert_resource(Log(Vec::new()));

    let mut schedule = Schedule::new();
    schedule
        .add_system(|query: Query<&mut Pos>, frame: Res<Frame>| {
            // Writes `a` on odd frames, and never `b`. Writing the same value
            // still counts.
            for mut pos in query {
                if frame.0 % 2 == 1 && pos.0 == 0 {
                    pos.0 = 0;
                }
            }
        })
        .add_system(
            |changed: Query<Entity, Changed<Pos>>,
             added: Query<Entity, Added<Pos>>,
             frame: Res<Frame>,
             mut log: ResMut<Log>| {
                let changed: Vec<_> = changed.iter().collect();
                let added: Vec<_> = added.iter().collect();
                log.0.push(format!(
                    "{}: changed {changed:?}, added {added:?}, frame changed {}",
                    frame.0,
                    frame.is_changed(),
                ));
            },
        )
        .add_system(|mut frame: ResMut<Frame>| {
            // Reading through `ResMut` does not count as a change.
            if frame.0 < 2 {
                frame.0 += 1;
            }
        });
    for _ in 0..4 {
        schedule.run(&mut world);
    }

    let log = &world.resource::<Log>().unwrap().0;
    assert_eq!(
        log,
        &[
            // Everything is new on a system's first run.
            format!("0: changed [{a}, {b}], added [{a}, {b}], frame changed true"),
            format!("1: changed [{a}], added [], frame changed true"),
            "2: changed [], added [], frame changed true".to_owned(),
            "2: changed [], added [], frame changed false".to_owned(),
        ]
    );
}

#[test]
fn systems_do_not_see_their_own_writes_as_changes() {
    let mut world = World::new();
    world.spawn_batch((0..3).map(Pos));
    world.insert_resource(Log(Vec::new()));

    let mut schedule = Schedule::new();
    schedule.add_system(
        |query: Query<&mut Pos, Changed<Pos>>, mut log: ResMut<Log>| {
            let mut count = 0;
            for mut pos in query {
                pos.0 += 1;
                count += 1;
            }
            log.0.push(count.to_string());
        },
    );
    for _ in 0..3 {
        schedule.run(&mut world);
    }
    assert_eq!(world.resource::<Log>().unwrap().0, ["3", "0", "0"]);
}

#[test]
fn restoring_a_snapshot_rewinds_systems_change_detection() {
    fn new_world() -> World {
        let mut world = World::new();
        world.register_clone::<Pos>();
        world.register_resource_clone::<Frame>();
        world.register_resource_clone::<Log>();
        world.spawn_batch((0..4).map(Pos));
        world.insert_resource(Frame(0));
        world.insert_resource(Log(Vec::new()));
        world
    }

    fn new_schedule() -> Schedule {
        let mut schedule = Schedule::new();
        schedule
            .add_system(|query: Query<&mut Pos>, frame: Res<Frame>| {
                // Each frame writes a different entity.
                let target = frame.0 % 4;
                for (i, mut pos) in (0..).zip(query) {
                    if i == target {
                        pos.0 += 10;
                    }
                }
            })
            .add_system(|changed: Query<&Pos, Changed<Pos>>, mut log: ResMut<Log>| {
                let seen: Vec<_> = changed.iter().map(|pos| pos.0).collect();
                log.0.push(format!("{seen:?}"));
            })
            .add_system(|mut frame: ResMut<Frame>| frame.0 += 1);
        schedule
    }

    let (mut on_time, mut schedule) = (new_world(), new_schedule());
    for _ in 0..6 {
        schedule.run(&mut on_time);
    }

    // The same, but with three frames run, rolled back and run again.
    let (mut world, mut schedule) = (new_world(), new_schedule());
    for _ in 0..2 {
        schedule.run(&mut world);
    }
    let saved = world.snapshot();
    for _ in 0..3 {
        schedule.run(&mut world);
    }
    world.restore(&saved);
    for _ in 0..4 {
        schedule.run(&mut world);
    }

    assert_eq!(world.resource::<Log>(), on_time.resource::<Log>());
    assert_eq!(world.checksum(), on_time.checksum());
    let positions = |world: &World| {
        world
            .query::<&Pos>()
            .iter()
            .map(|pos| pos.0)
            .collect::<Vec<_>>()
    };
    assert_eq!(positions(&world), positions(&on_time));
}

#[test]
fn commands_apply_before_exclusive_systems_and_at_the_end() {
    struct Marker;
    impl Component for Marker {}

    let mut world = World::new();
    world.insert_resource(Log(Vec::new()));
    let mut schedule = Schedule::new();
    schedule
        .add_system(|mut commands: Commands| {
            commands.spawn(Pos(1));
        })
        .add_system(|world: &mut World| {
            let count = world.query::<&Pos>().iter().count();
            world
                .resource_mut::<Log>()
                .unwrap()
                .0
                .push(count.to_string());
        })
        .add_system(|mut commands: Commands| {
            commands.spawn((Pos(2), Marker));
        });
    schedule.run(&mut world);
    assert_eq!(world.resource::<Log>().unwrap().0, ["1"]);
    assert_eq!(world.query::<&Pos>().with::<Marker>().iter().count(), 1);
}

#[test]
fn the_order_of_conflicting_systems_is_kept() {
    let mut world = World::new();
    world.insert_resource(Log(Vec::new()));
    let mut schedule = Schedule::new();
    for name in ["a", "b", "c", "d"] {
        schedule.add_system(move |mut log: ResMut<Log>| log.0.push(name.to_owned()));
    }
    schedule.set_threads(4).run(&mut world);
    assert_eq!(world.resource::<Log>().unwrap().0, ["a", "b", "c", "d"]);
}

#[test]
#[should_panic(expected = "run with a world other than the one it first ran with")]
fn schedules_belong_to_one_world() {
    let mut schedule = Schedule::new();
    schedule.add_system(|_: Query<&Pos, With<Vel>>| {});
    schedule.run(&mut World::new());
    schedule.run(&mut World::new());
}
