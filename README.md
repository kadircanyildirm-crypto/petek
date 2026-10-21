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

let velocity = world.remove::<Velocity>(ball).unwrap();
let position = world.get_mut::<Position>(ball).unwrap();
position.x += velocity.x;
position.y += velocity.y;
```

## Soundness

`petek` uses `unsafe` code for its type-erased storage, so it is held to a
strict standard:

- every `unsafe` block carries a `SAFETY` comment (enforced by Clippy);
- the whole test suite runs under [Miri] with strict provenance in CI;
- structural changes run no user code until the world is consistent again,
  so a panicking component destructor cannot cause a double drop or leave an
  entity half-moved.

[Miri]: https://github.com/rust-lang/miri

## Roadmap

- [x] Generational entity ids
- [x] Type-erased column storage
- [x] Archetypes and `World`
- [ ] Queries with filters
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
