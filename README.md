# petek

An archetype-based Entity Component System (ECS) for Rust.

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
for (position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
    position.x += velocity.x;
    position.y += velocity.y;
}

// Components can be added and removed at any time.
world.remove::<Velocity>(ball).unwrap();
assert!(world.query::<&Velocity>().into_iter().next().is_none());
assert_eq!(world.get::<Position>(wall).unwrap().x, 5.0);
```

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
- [ ] Resources, commands and change detection
- [ ] Systems and a parallel scheduler
- [ ] Events, examples and benchmarks

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in the work by you, as defined in the Apache-2.0 license, shall be
dual licensed as above, without any additional terms or conditions.
