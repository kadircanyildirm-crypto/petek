# petek

An archetype-based Entity Component System (ECS) for Rust.

> **Status:** early development, not ready for use yet.

*Petek* is Turkish for *honeycomb*: entities that have the same set of
components live side by side in the same cell (an *archetype*), packed densely
so that systems can iterate over them quickly.

## Roadmap

- [x] Generational entity ids
- [ ] Type-erased column storage
- [ ] Archetypes and `World`
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
