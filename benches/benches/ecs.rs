//! Compares `petek` with `hecs` and `bevy_ecs` on common workloads, and
//! measures what only `petek` does: snapshots, restores, checksums, and its
//! parallel schedule.
//!
//! Run with `cargo bench` from the `benches` directory.

use std::hash::{Hash, Hasher};
use std::num::NonZeroUsize;
use std::thread;

use bevy_ecs::component::Component;
use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use petek::query::{Query, QueryState};
use petek::schedule::Schedule;

/// How many entities most benchmarks work on.
const ENTITIES: usize = 10_000;

#[derive(Clone, Copy, Component)]
struct Position(f32, f32, f32);

#[derive(Clone, Copy, Component)]
struct Velocity(f32, f32, f32);

#[derive(Clone, Copy, Component)]
struct Marker;

impl petek::Component for Position {}
impl petek::Component for Velocity {}
impl petek::Component for Marker {}

// Floats do not implement `Hash`; a simulation hashes their bits.
impl Hash for Position {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
        self.1.to_bits().hash(state);
        self.2.to_bits().hash(state);
    }
}

impl Hash for Velocity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
        self.1.to_bits().hash(state);
        self.2.to_bits().hash(state);
    }
}

#[allow(clippy::cast_precision_loss, reason = "any value will do")]
fn moving(i: usize) -> (Position, Velocity) {
    let x = i as f32;
    (Position(x, x, x), Velocity(1.0, 0.5, 0.25))
}

fn spawn(c: &mut Criterion) {
    let mut group = c.benchmark_group("spawn");
    group.bench_function("petek", |b| {
        b.iter_with_large_drop(|| {
            let mut world = petek::World::new();
            world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);
            world
        });
    });
    group.bench_function("hecs", |b| {
        b.iter_with_large_drop(|| {
            let mut world = hecs::World::new();
            world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);
            world
        });
    });
    group.bench_function("bevy_ecs", |b| {
        b.iter_with_large_drop(|| {
            let mut world = bevy_ecs::world::World::new();
            world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);
            world
        });
    });
    group.finish();
}

fn iterate(c: &mut Criterion) {
    let mut group = c.benchmark_group("iterate");

    let mut world = petek::World::new();
    world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);
    let mut query = QueryState::<(&mut Position, &Velocity)>::new(&mut world);
    group.bench_function("petek", |b| {
        b.iter(|| {
            for (mut position, velocity) in query.iter_mut(&mut world) {
                position.0 += velocity.0;
                position.1 += velocity.1;
                position.2 += velocity.2;
            }
        });
    });
    group.bench_function("petek_for_each", |b| {
        b.iter(|| {
            query.iter_mut(&mut world).for_each(|(mut position, velocity)| {
                position.0 += velocity.0;
                position.1 += velocity.1;
                position.2 += velocity.2;
            });
        });
    });

    let mut world = hecs::World::new();
    world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);
    group.bench_function("hecs", |b| {
        b.iter(|| {
            for (position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
                position.0 += velocity.0;
                position.1 += velocity.1;
                position.2 += velocity.2;
            }
        });
    });

    let mut world = bevy_ecs::world::World::new();
    world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);
    let mut query = world.query::<(&mut Position, &Velocity)>();
    group.bench_function("bevy_ecs", |b| {
        b.iter(|| {
            for (mut position, velocity) in query.iter_mut(&mut world) {
                position.0 += velocity.0;
                position.1 += velocity.1;
                position.2 += velocity.2;
            }
        });
    });
    group.finish();
}

#[derive(Clone, Copy, Component)]
struct Data(f32);
impl petek::Component for Data {}

/// Declares the marker components that split `Data` into archetypes, and
/// spawns 20 entities of each archetype into a world.
macro_rules! fragments {
    ($($name:ident),*) => {
        $(
            #[derive(Clone, Copy, Component)]
            struct $name;
            impl petek::Component for $name {}
        )*

        macro_rules! spawn_fragments {
            ($world:expr) => {
                for _ in 0..20 {
                    $($world.spawn((Data(1.0), $name));)*
                }
            };
        }
    };
}

fragments!(
    F0, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11, F12, F13, F14, F15, F16, F17, F18, F19
);

fn iterate_fragmented(c: &mut Criterion) {
    let mut group = c.benchmark_group("iterate_fragmented");

    let mut world = petek::World::new();
    spawn_fragments!(world);
    let mut query = QueryState::<&mut Data>::new(&mut world);
    group.bench_function("petek", |b| {
        b.iter(|| {
            for mut data in query.iter_mut(&mut world) {
                data.0 *= 2.0;
            }
        });
    });
    group.bench_function("petek_for_each", |b| {
        b.iter(|| {
            query.iter_mut(&mut world).for_each(|mut data| data.0 *= 2.0);
        });
    });

    let mut world = hecs::World::new();
    spawn_fragments!(world);
    group.bench_function("hecs", |b| {
        b.iter(|| {
            for data in world.query_mut::<&mut Data>() {
                data.0 *= 2.0;
            }
        });
    });

    let mut world = bevy_ecs::world::World::new();
    spawn_fragments!(world);
    let mut query = world.query::<&mut Data>();
    group.bench_function("bevy_ecs", |b| {
        b.iter(|| {
            for mut data in query.iter_mut(&mut world) {
                data.0 *= 2.0;
            }
        });
    });
    group.finish();
}

fn add_remove(c: &mut Criterion) {
    let mut group = c.benchmark_group("add_remove");

    let mut world = petek::World::new();
    let entities: Vec<_> = world.spawn_batch((0..ENTITIES).map(moving)).collect();
    group.bench_function("petek", |b| {
        b.iter(|| {
            for &entity in &entities {
                world.insert(entity, Marker).unwrap();
            }
            for &entity in &entities {
                world.remove::<Marker>(entity).unwrap();
            }
        });
    });

    let mut world = hecs::World::new();
    let entities: Vec<_> = world.spawn_batch((0..ENTITIES).map(moving)).collect();
    group.bench_function("hecs", |b| {
        b.iter(|| {
            for &entity in &entities {
                world.insert_one(entity, Marker).unwrap();
            }
            for &entity in &entities {
                world.remove_one::<Marker>(entity).unwrap();
            }
        });
    });

    let mut world = bevy_ecs::world::World::new();
    let entities: Vec<_> = world.spawn_batch((0..ENTITIES).map(moving)).collect();
    group.bench_function("bevy_ecs", |b| {
        b.iter(|| {
            for &entity in &entities {
                world.entity_mut(entity).insert(Marker);
            }
            for &entity in &entities {
                world.entity_mut(entity).remove::<Marker>();
            }
        });
    });
    group.finish();
}

fn rollback(c: &mut Criterion) {
    let mut group = c.benchmark_group("rollback");
    let mut world = petek::World::new();
    world.register_clone::<Position>();
    world.register_clone::<Velocity>();
    world.register_hash::<Position>();
    world.register_hash::<Velocity>();
    world.spawn_batch((0..ENTITIES).map(moving)).for_each(drop);

    group.bench_function("snapshot", |b| b.iter_with_large_drop(|| world.snapshot()));
    let snapshot = world.snapshot();
    group.bench_function("restore", |b| b.iter(|| world.restore(&snapshot)));
    group.bench_function("checksum", |b| b.iter(|| world.checksum()));
    group.finish();
}

/// Declares components that each get a system of their own, which writes
/// them from the entity's `Position`. The systems never conflict, so a
/// schedule can run them all at once.
macro_rules! independent_systems {
    ($($component:ident => $system:ident),*) => {
        $(
            #[derive(Clone, Copy)]
            struct $component(f32);
            impl petek::Component for $component {}

            fn $system(query: Query<(&mut $component, &Position)>) {
                for (mut value, position) in query {
                    value.0 = (value.0 + position.0).sqrt();
                }
            }
        )*

        fn independent_schedule(threads: usize) -> Schedule {
            let mut schedule = Schedule::new();
            $(schedule.add_system($system);)*
            schedule.set_threads(threads);
            schedule
        }

        fn independent_entity(i: usize) -> (Position, $($component,)*) {
            (moving(i).0, $($component(1.0),)*)
        }
    };
}

independent_systems!(S0 => s0, S1 => s1, S2 => s2, S3 => s3, S4 => s4, S5 => s5, S6 => s6, S7 => s7);

fn schedule(c: &mut Criterion) {
    let mut group = c.benchmark_group("schedule");
    let cores = thread::available_parallelism().map_or(1, NonZeroUsize::get);
    for entities in [1_000, 100_000] {
        for threads in [1, cores] {
            let mut world = petek::World::new();
            world
                .spawn_batch((0..entities).map(independent_entity))
                .for_each(drop);
            let mut schedule = independent_schedule(threads);
            schedule.run(&mut world);
            let id = BenchmarkId::new(format!("{entities} entities"), format!("{threads} threads"));
            group.bench_function(id, |b| b.iter(|| schedule.run(&mut world)));
        }
    }
    group.finish();
}

criterion_group!(
    benches,
    spawn,
    iterate,
    iterate_fragmented,
    add_remove,
    rollback,
    schedule
);
criterion_main!(benches);
