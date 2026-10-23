# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `World`, with `spawn`, `spawn_batch`, `despawn`, `insert`, `remove`, `get`,
  `get_mut`, `has`, `contains`, `len` and `clear`.
- The `Component` trait, and the `Bundle` trait implemented for components
  and for nested tuples of up to 12 bundles.
- `Entity` handles with generations, convertible to and from `u64`.
- The `NoSuchEntity` and `RemoveError` error types.
- Queries: `World::query` for read-only queries and `World::query_mut` for
  queries that write, over `&T`, `&mut T`, `Entity`, `Option` and nested
  tuples, with the `With`, `Without` and `Or` filters. `Query::get` and
  `Query::get_mut` fetch a single entity. Query iterators are `Send` and
  `Sync`, and exact-size unless they filter on changes.
- `QueryState`, which caches the archetypes a query matches between runs.
- The `QueryError` error type.
- Resources: unique values stored in the world, outside any entity.
- Commands: a `CommandQueue` records spawns, inserts, removals, despawns and
  resource changes to apply later, for example after a query is done, and
  `Query::commands` lets them be recorded while any query runs. Spawning
  through commands hands out the new entity right away, and
  `CommandQueue::clear` drops commands without applying them.
- Change detection: `Added` and `Changed` query filters, and the `Mut` smart
  pointer that marks a component as changed when it is written through.
  Queries for `&mut T` and `World::get_mut` now yield `Mut<T>`.
- Snapshots, for rollback: `World::snapshot` copies the world into a
  `Snapshot`, and `World::restore` brings the world back to it exactly. The
  component and resource types it holds must be registered with
  `World::register_clone` and `World::register_resource_clone`. `QueryState`
  is now `Clone`, so that it can be saved along with a snapshot.
- Checksums, for detecting desyncs: `World::checksum` hashes the entities and
  the component and resource types registered with `World::register_hash`
  and `World::register_resource_hash`, leaving out ticks.
- Change detection for resources, and `Mut::is_added` and `Mut::is_changed`.
  `World::resource_mut` now returns a `Mut<R>`.
- Systems: functions that take `Query`, `Res`, `ResMut`, `Commands` or
  `&World` parameters, or `&mut World`. Systems compare changes with their
  own last run, which snapshots rewind.
- `Schedule`, which runs systems in parallel with the same result as running
  them one after another, in the order they were added.
