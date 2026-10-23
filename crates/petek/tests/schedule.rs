//! Tests that schedules give the same results however many threads they use.

use std::panic::{self, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};

use petek::command::Commands;
use petek::query::{Added, Changed, Query, With, Without};
use petek::schedule::Schedule;
use petek::system::{Res, ResMut};
use petek::{Component, Entity, Resource, World};

#[derive(Clone, Copy, Debug, Hash, PartialEq)]
struct A(u64);
impl Component for A {}

#[derive(Clone, Copy, Debug, Hash, PartialEq)]
struct B(u64);
impl Component for B {}

#[derive(Clone, Copy, Debug, Hash, PartialEq)]
struct C(u64);
impl Component for C {}

#[derive(Clone, Copy, Debug, Hash, PartialEq)]
struct Seed(u64);
impl Resource for Seed {}

#[derive(Clone, Debug, Hash, PartialEq)]
struct Log(Vec<u64>);
impl Resource for Log {}

#[derive(Clone, Copy, Debug, Hash, PartialEq)]
struct Frame(u64);
impl Resource for Frame {}

/// Mixes `value` into `state`, in a way that depends on the order of calls.
fn mix(state: u64, value: u64) -> u64 {
    (state ^ value)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
        .rotate_left(17)
}

// The systems below do order-sensitive work on overlapping data, so running
// two conflicting ones in the wrong order changes the result.

fn grow_a(seed: Res<Seed>, query: Query<&mut A>) {
    for mut a in query {
        a.0 = mix(a.0, seed.0);
    }
}

fn a_into_b(query: Query<(&A, &mut B)>) {
    for (a, mut b) in query {
        b.0 = mix(b.0, a.0);
    }
}

fn count_changed_a(query: Query<&mut C, Changed<A>>) {
    for mut c in query {
        c.0 += 1;
    }
}

fn reseed(mut seed: ResMut<Seed>, query: Query<&B>) {
    for b in query {
        seed.0 = mix(seed.0, b.0);
    }
}

fn spawn_from_c(query: Query<(Entity, &C)>, frame: Res<Frame>, mut commands: Commands) {
    for (entity, c) in query {
        if (c.0 + frame.0 + u64::from(entity.index())) % 4 == 0 {
            commands.spawn((A(c.0), B(frame.0)));
        }
    }
}

fn flip_a_with_c(query: Query<&mut A, With<C>>) {
    for mut a in query {
        a.0 ^= 0xdead_beef;
    }
}

/// Writes `A` too, but never on the same entities as `flip_a_with_c`, so the
/// two may run at the same time.
fn rotate_a_without_c(query: Query<&mut A, Without<C>>) {
    for mut a in query {
        a.0 = a.0.rotate_left(3);
    }
}

fn log_a(mut log: ResMut<Log>, query: Query<(Entity, &A)>) {
    let mut state = 0;
    for (entity, a) in query {
        state = mix(mix(state, entity.to_bits()), a.0);
    }
    log.0.push(state);
}

fn despawn_some(query: Query<Entity, Without<C>>, frame: Res<Frame>, mut commands: Commands) {
    for entity in query {
        if (u64::from(entity.index()) + frame.0) % 5 == 0 {
            commands.despawn(entity);
        }
    }
}

fn grow_c(seed: Res<Seed>, query: Query<&mut C>) {
    for mut c in query {
        c.0 = c.0.wrapping_mul(3).wrapping_add(seed.0 % 7);
    }
}

fn give_c(world: &mut World) {
    let frame = world.resource::<Frame>().unwrap().0;
    let targets: Vec<Entity> = world
        .query::<Entity>()
        .without::<C>()
        .into_iter()
        .step_by(3)
        .collect();
    for entity in targets {
        world.insert(entity, C(frame)).unwrap();
    }
}

fn log_added_b(mut log: ResMut<Log>, query: Query<&B, Added<B>>) {
    log.0.push(query.iter().map(|b| b.0).fold(0, mix));
}

fn log_world(world: &World, mut ignored: Commands) {
    // Reading the whole world, alongside commands that record nothing.
    let _ = &mut ignored;
    assert!(world.len() < 10_000);
}

fn next_frame(mut frame: ResMut<Frame>) {
    frame.0 += 1;
}

fn new_world() -> World {
    let mut world = World::new();
    world.register_hash::<A>();
    world.register_hash::<B>();
    world.register_hash::<C>();
    world.register_resource_hash::<Seed>();
    world.register_resource_hash::<Log>();
    world.register_resource_hash::<Frame>();
    world.insert_resource(Seed(7));
    world.insert_resource(Log(Vec::new()));
    world.insert_resource(Frame(0));
    for i in 0..30 {
        match i % 3 {
            0 => world.spawn((A(i), B(i))),
            1 => world.spawn((A(i), C(i))),
            _ => world.spawn((A(i), B(i), C(i))),
        };
    }
    world
}

fn new_schedule(threads: usize) -> Schedule {
    let mut schedule = Schedule::new();
    schedule
        .add_system(grow_a)
        .add_system(a_into_b)
        .add_system(count_changed_a)
        .add_system(reseed)
        .add_system(spawn_from_c)
        .add_system(flip_a_with_c)
        .add_system(rotate_a_without_c)
        .add_system(log_a)
        .add_system(despawn_some)
        .add_system(grow_c)
        .add_system(give_c)
        .add_system(log_added_b)
        .add_system(log_world)
        .add_system(next_frame)
        .set_threads(threads);
    schedule
}

/// An entity and its components.
type Row = (Entity, Option<A>, Option<B>, Option<C>);

/// Everything about a world that can be observed, in query order.
fn dump(world: &World) -> (Vec<Row>, u64, petek::Tick) {
    let entities = world
        .query::<(Entity, Option<&A>, Option<&B>, Option<&C>)>()
        .into_iter()
        .map(|(entity, a, b, c)| (entity, a.copied(), b.copied(), c.copied()))
        .collect();
    (entities, world.checksum(), world.change_tick())
}

#[test]
fn any_number_of_threads_gives_the_same_world() {
    let frames = if cfg!(miri) { 4 } else { 25 };
    let mut runs: Vec<_> = [1, 2, 3, 8]
        .into_iter()
        .map(|threads| (new_world(), new_schedule(threads)))
        .collect();
    for _ in 0..frames {
        let mut dumps = runs.iter_mut().map(|(world, schedule)| {
            schedule.run(world);
            dump(world)
        });
        let sequential = dumps.next().unwrap();
        for parallel in dumps {
            assert_eq!(parallel, sequential);
        }
    }
    let (world, _) = &runs[0];
    assert!(
        !world.is_empty(),
        "the systems should not despawn everything"
    );
}

#[test]
fn systems_that_only_read_do_not_change_the_checksum() {
    fn draw(query: Query<(&A, Option<&B>)>, seed: Res<Seed>, world: &World) {
        let _ = (query.iter().count(), seed.0, world.len());
    }

    let frames = if cfg!(miri) { 3 } else { 10 };
    let (mut headless, mut simulation) = (new_world(), new_schedule(2));
    let (mut drawn, mut drawn_simulation) = (new_world(), new_schedule(2));
    let mut drawing = Schedule::new();
    drawing.add_system(draw).add_system(draw);
    for _ in 0..frames {
        simulation.run(&mut headless);
        drawn_simulation.run(&mut drawn);
        drawing.run(&mut drawn);
    }

    // Drawing moved the tick, but the simulation came out the same.
    let (headless, drawn) = (dump(&headless), dump(&drawn));
    assert_ne!(headless.2, drawn.2);
    assert_eq!((headless.0, headless.1), (drawn.0, drawn.1));
}

/// A run of a schedule whose last system panics if `fail` is set, followed
/// by more runs that do not: how many entities with a `C` each run sees.
fn runs_after_a_panic(threads: usize, fail: &'static AtomicBool) -> (World, Vec<usize>) {
    let mut world = World::new();
    world.spawn_batch((0..3).map(|i| (A(i), C(i))));
    world.insert_resource(Log(Vec::new()));

    let mut schedule = Schedule::new();
    schedule
        .add_system(|query: Query<&mut C, Changed<C>>, mut log: ResMut<Log>| {
            // Writes every row it sees as changed, which it must not see as
            // changed again.
            let mut count = 0;
            for mut c in query {
                c.0 += 1;
                count += 1;
            }
            log.0.push(count);
        })
        .add_system(|mut commands: Commands| {
            commands.spawn(B(0));
        })
        .add_system(move |query: Query<&C>, log: Res<Log>| {
            // Runs after both systems above, which it conflicts with.
            assert!(!fail.swap(false, Ordering::Relaxed), "boom");
            let _ = (query.iter().count(), log.0.len());
        })
        .set_threads(threads);
    let result = panic::catch_unwind(AssertUnwindSafe(|| schedule.run(&mut world)));
    assert!(result.is_err());
    for _ in 0..2 {
        schedule.run(&mut world);
    }
    let seen = world.resource::<Log>().unwrap().0.iter();
    let seen = seen.map(|&count| usize::try_from(count).unwrap()).collect();
    (world, seen)
}

#[test]
fn systems_that_finished_before_a_panic_remember_running() {
    static FAIL: [AtomicBool; 2] = [AtomicBool::new(true), AtomicBool::new(true)];
    for (threads, fail) in [1, 4].into_iter().zip(&FAIL) {
        let (_, seen) = runs_after_a_panic(threads, fail);
        assert_eq!(seen, [3, 0, 0], "{threads} threads");
    }
}

#[test]
fn commands_of_a_run_that_panicked_are_dropped() {
    static FAIL: [AtomicBool; 2] = [AtomicBool::new(true), AtomicBool::new(true)];
    for (threads, fail) in [1, 4].into_iter().zip(&FAIL) {
        let (world, _) = runs_after_a_panic(threads, fail);
        // Two runs finished, each spawning one `B`; the one that panicked
        // spawned none.
        assert_eq!(world.query::<&B>().iter().count(), 2, "{threads} threads");
    }
}

#[test]
fn a_run_that_panicked_gives_up_its_reservations() {
    static FAIL: AtomicBool = AtomicBool::new(true);

    let mut world = World::new();
    world.register_clone::<B>();
    let mut schedule = Schedule::new();
    schedule
        .add_system(|mut commands: Commands| {
            commands.spawn(B(1));
        })
        .add_system(|query: Query<&B>| {
            assert!(!FAIL.swap(false, Ordering::Relaxed), "boom");
            let _ = query.iter().count();
        });
    let result = panic::catch_unwind(AssertUnwindSafe(|| schedule.run(&mut world)));
    assert!(result.is_err());

    // Nothing of that run is left: no reservation keeps the world from being
    // snapshotted, and no entity without components comes alive.
    let _ = world.snapshot();
    schedule.run(&mut world);
    assert_eq!(world.len(), 1);
    assert_eq!(world.query::<&B>().iter().count(), 1);
}

#[test]
fn a_panicking_system_does_not_hang_the_schedule() {
    static FAIL: AtomicBool = AtomicBool::new(true);

    fn fragile(query: Query<&A>) {
        assert!(!FAIL.load(Ordering::Relaxed), "boom");
        let _ = query.iter().count();
    }

    let mut world = new_world();
    let mut schedule = new_schedule(4);
    schedule.add_system(fragile);
    let result = panic::catch_unwind(AssertUnwindSafe(|| schedule.run(&mut world)));
    assert!(result.is_err());

    // The world is still consistent, and the schedule still runs.
    FAIL.store(false, Ordering::Relaxed);
    schedule.run(&mut world);
    assert!(world.query::<&A>().iter().count() > 0);
}
