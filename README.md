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

Keeping floating-point math deterministic across platforms remains up to the
game.

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
