# petek

An archetype-based Entity Component System (ECS) for Rust, built for
deterministic simulation.

> **Status:** early development, not ready for use yet.

*Petek* is Turkish for *honeycomb*: entities that have the same set of
components live side by side in the same cell (an *archetype*), packed densely
so that systems can iterate over them quickly.

## Example

```rust
use petek::{Component, World};

struct Position { x: f32, y: f32 }
impl Component for Position {}

struct Velocity { x: f32, y: f32 }
impl Component for Velocity {}

let mut world = World::new();
let ball = world.spawn((Position { x: 0.0, y: 0.0 }, Velocity { x: 1.0, y: 2.0 }));
let wall = world.spawn(Position { x: 5.0, y: 0.0 });

// Move everything that has a velocity.
for (mut position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
    position.x += velocity.x;
    position.y += velocity.y;
}

// Components can be added and removed at any time.
world.remove::<Velocity>(ball).unwrap();
assert!(world.query::<&Velocity>().into_iter().next().is_none());
assert_eq!(world.get::<Position>(wall).unwrap().x, 5.0);
```

Systems are functions whose parameters say what they read and write, and a
schedule runs them in parallel where those allow:

```rust
use petek::query::Query;
use petek::schedule::Schedule;
use petek::system::Res;
use petek::{Component, Resource, World};

struct Height(f32);
impl Component for Height {}

struct Speed(f32);
impl Component for Speed {}

struct Gravity(f32);
impl Resource for Gravity {}

fn fall(gravity: Res<Gravity>, query: Query<&mut Speed>) {
    for mut speed in query {
        speed.0 -= gravity.0;
    }
}

fn movement(query: Query<(&mut Height, &Speed)>) {
    for (mut height, speed) in query {
        height.0 += speed.0;
    }
}

let mut world = World::new();
world.insert_resource(Gravity(1.0));
let ball = world.spawn((Height(10.0), Speed(0.0)));

let mut schedule = Schedule::new();
schedule.add_system(fall).add_system(movement);
schedule.run(&mut world);
assert_eq!(world.get::<Height>(ball).unwrap().0, 9.0);
```

## Determinism

Games with lockstep or rollback networking need every machine to compute the
same world from the same inputs, bit for bit. `petek` makes that a guarantee
rather than an accident: given the same sequence of operations, a world hands
out the same entity handles, iterates in the same order, and records the same
change ticks.

- No result depends on hash-map iteration order or memory addresses:
  entities, archetypes, components and resources are numbered in the order
  they first appear.
- Commands apply in the order they were recorded, and the entities they spawn
  are reserved in that order too.
- A schedule's result is the same on any number of threads as running its
  systems one after another, in the order they were added: systems that
  conflict keep that order, each runs at a tick assigned in that order, and
  two systems that spawn entities never run at the same time. Only interior
  mutability, such as a `Mutex` in a resource, escapes this.
- Change detection counts writes with a 64-bit tick that only moves when the
  world is written to, so it cannot wrap around or drift between machines.
- Restoring a snapshot brings back everything, down to the order archetypes
  were created in and the last run of every system, so simulating again after
  a rollback gives exactly the world that never mispredicted.

```rust
use petek::{Component, World};

#[derive(Clone, Hash)]
struct Position(i32);
impl Component for Position {}

let mut world = World::new();
world.register_clone::<Position>(); // snapshots clone it
world.register_hash::<Position>(); // checksums hash it
let player = world.spawn(Position(0));

// Keep a snapshot, and simulate ahead with a guessed input.
let saved = world.snapshot();
world.get_mut::<Position>(player).unwrap().0 += 1;

// The real input arrives, and it differs: roll back and simulate again.
world.restore(&saved);
world.get_mut::<Position>(player).unwrap().0 -= 1;
assert_eq!(world.get::<Position>(player).unwrap().0, -1);

// Peers compare checksums to catch desyncs.
let checksum: u64 = world.checksum();
```

[`examples/rollback.rs`](crates/petek/examples/rollback.rs) plays a small
game whose remote inputs arrive four frames late: it guesses them, rolls back
whenever a guess was wrong, and ends with the same checksum as a peer that
had every input on time. Run it with `cargo run --example rollback`.

Keeping floating-point math deterministic across platforms remains up to the
game.

## Performance

The [`benches`](benches) directory compares `petek` with
[`hecs`](https://crates.io/crates/hecs) and
[`bevy_ecs`](https://crates.io/crates/bevy_ecs). Medians on an Intel Core
i5-12450H (8 cores, 12 threads), Windows, Rust 1.96; lower is better:

| Benchmark (10 000 entities)           | petek   | hecs    | bevy_ecs |
| ------------------------------------- | ------- | ------- | -------- |
| spawn into a new world                | 451 µs  | 331 µs  | 316 µs   |
| iterate two components, `for_each`    | 25.2 µs | 17.1 µs | 22.1 µs  |
| iterate two components, `for` loop    | 31.4 µs |         |          |
| iterate 20 archetypes, `for_each`     | 314 ns  | 459 ns  | 357 ns   |
| iterate 20 archetypes, `for` loop     | 1056 ns |         |          |
| add and remove a component            | 1.55 ms | 1.61 ms | 1.87 ms  |

`hecs` has no change detection, which costs `petek` and `bevy_ecs` a tick
per write. A `for` loop over a query is slower than `for_each`, most of all
across many archetypes, so `for_each` is the way to write hot loops. What
only `petek` does, on the same 10 000 entities: a snapshot takes 131 µs, a
restore 58 µs, and a checksum 194 µs.

A schedule of eight systems that can all run at once, on 100 000 entities,
takes 2.25 ms on one thread and 1.48 ms on twelve; they are bound by memory
bandwidth more than by computation. On 1 000 entities, waking the other
threads costs more than they save, 50 µs against 19 µs on one thread, so
small worlds are better off with `Schedule::set_threads(1)`.

## Soundness

`petek` uses `unsafe` code for its type-erased storage, so it is held to a
strict standard:

- every `unsafe` block carries a `SAFETY` comment, and every `unsafe fn`
  documents its contract (both enforced by Clippy);
- structural changes run no user code until the world is consistent again,
  so a panicking destructor can neither cause a double drop nor leave an
  entity half-moved;
- the borrow checker keeps queries from overlapping with writes, and a query
  that would alias a mutable reference, such as `(&mut A, &A)`, is rejected;
- the scheduler never runs systems that conflict at the same time, and the
  traits it relies on to know what systems access are sealed;
- CI runs the whole test suite under [Miri] with strict provenance, under both
  the Stacked Borrows and Tree Borrows aliasing models, and on 32-bit and
  big-endian targets.

[ARCHITECTURE.md](ARCHITECTURE.md) explains the design and its invariants.

[Miri]: https://github.com/rust-lang/miri

## Roadmap

- [x] Generational entity ids
- [x] Type-erased column storage
- [x] Archetypes and `World`
- [x] Queries with filters, and cached query state
- [x] Resources, commands and change detection
- [x] Snapshots, rollback and world checksums
- [x] Systems and a deterministic parallel scheduler
- [ ] Events, a rollback demo, benchmarks and derive macros

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
