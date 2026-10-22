# Architecture

This document describes how `petek` stores its data, how entities move
between tables, and how the `unsafe` code that makes this fast is kept sound.
It is meant to be read before the code.

## Code map

| Module | Contents |
| --- | --- |
| `entity.rs` | `Entity` handles and `Entities`, the allocator that tracks generations and where each entity is stored. |
| `component.rs` | The `Component` trait, `ComponentInfo`, and the per-world `Components` registry. |
| `bundle.rs` | The `Bundle` trait, its implementations for components and tuples, and the per-world `Bundles` registry. |
| `storage/blob_vec.rs` | `BlobVec`, a type-erased `Vec` that backs one component column. |
| `storage/mod.rs` | `drop_each`, which keeps dropping values after a destructor panics. |
| `archetype.rs` | `Archetype` tables, and `Archetypes`: all tables plus the cached `Transition`s between them. |
| `world.rs` | `World`, the public entry point that ties everything together. |
| `query/data.rs` | `QueryData`: what a query fetches, implemented for `&T`, `&mut T`, `Entity`, `Option` and tuples. |
| `query/filter.rs` | `QueryFilter`: `With`, `Without`, `Or` and tuples. |
| `query/iter.rs` | `QueryIter`, which walks the matching archetypes row by row. |
| `query/state.rs` | `QueryState`, which caches the archetypes a query matches. |
| `query/mod.rs` | `Query`, the access check, and the seal on the query traits. |
| `hash.rs` | A fast hasher for the internal lookup tables. |

## Data model

An **entity** is a 32-bit slot index plus a 32-bit, never-zero **generation**.
When an entity is despawned its slot is recycled with a higher generation, so a
stale handle no longer matches and cannot reach the new entity. A slot whose
generation would overflow is retired instead of wrapping around.

A **component** is any `Send + Sync + 'static` type that implements
`Component`. Each world gives the component types it encounters dense
`ComponentId`s.

An **archetype** is a table that stores every entity with exactly one set of
components. It has one column per component and one row per entity:

```text
archetype {Position, Velocity}

              row 0     row 1     row 2
entities      e3        e7        e1
Position      (0, 0)    (4, 2)    (1, 1)    <- BlobVec
Velocity      (1, 0)    (0, 1)    (2, 2)    <- BlobVec
```

Columns are `BlobVec`s: contiguous arrays described only by a `Layout` and a
drop function, so a system iterating over `Position` reads one dense array.
For every alive entity, `Entities` records its **location**: the archetype and
the row it lives in.

## Transitions

Adding components to an entity, or removing them, moves it to another
archetype. The first time a given bundle type is inserted into (or removed
from) an entity of a given archetype, the world works out a `Transition`: the
target archetype, plus which column each component moves from and to, which
columns the bundle fills, and which values it replaces. Each archetype caches
its transitions per bundle type, so later moves need one hash lookup and no
searching at all.

Spawning goes through the same cache: a spawn is an insert into the empty
archetype. `spawn_batch` looks the transition up once for the whole batch.

## Structural changes

Spawning, despawning, inserting and removing all follow the same rule:

> **No user code runs while the world is inconsistent.**

Two kinds of user code can run during a structural change: a component's
destructor, and the bundle that hands its components over. So every change is
ordered in phases:

1. **Stage.** The bundle's components are written into spare capacity
   reserved at the end of their columns, without being counted. If the bundle
   panics here, nothing has changed; the staged values are merely leaked.
2. **Restructure.** Rows are moved with plain memory copies, swap-removal
   fills each hole with the table's last row, staged values are committed, and
   the location of every entity that moved is updated. Nothing here can
   panic or call user code.
3. **Drop.** Only once all tables and locations agree again are replaced or
   removed components dropped (or, for a removal, handed to the caller).

A destructor that panics therefore always observes, and leaves behind, a
consistent world. When several values are dropped at once and one destructor
panics, the rest are still dropped while unwinding, as standard containers do,
and a second panic aborts. Overwriting a component in place moves the new
value in even if the old value's destructor panics.

## Queries

A query is described by two types: its `QueryData`, such as
`(Entity, &mut Position, &Velocity)`, and its `QueryFilter`, such as
`Without<Frozen>`. Both work in three steps:

1. **Once per world**, the query resolves its *state*: the `ComponentId`s of
   its component types.
2. **Once per archetype**, it decides whether the archetype matches, and if so
   prepares a *fetch*: a pointer to the start of each column it reads.
3. **Once per row**, it offsets those pointers by the row to produce the item.

So iterating over a query is, for each matching archetype, a tight loop over
contiguous columns. `QueryIter` overrides `fold`, so that `for_each` and most
iterator adapters run exactly that loop, and computes `size_hint` and `count`
from the archetype sizes alone.

A `QueryState` keeps the state and the list of matching archetypes between
runs. Archetypes are never removed, so on each run it only checks the
archetypes created since the last one.

### Why handing out `&mut T` is sound

- **Within a query**, every row of every matching archetype is visited once,
  so items never overlap, unless the query itself names a component twice.
  Each query reports which components it reads and writes, and a query that
  writes a component it also accesses elsewhere, such as `(&mut A, &A)`, is
  rejected with a panic when it is created.
- **Across queries**, the borrow checker does the work: `World::query` takes
  `&World` and only accepts `ReadOnlyQueryData`, while `World::query_mut`
  takes `&mut World`. Items borrow the world, so no structural change can
  happen while one is alive.
- **Pointers**: a query reaches the world through a shared reference, which
  covers the `BlobVec` headers but not the items, as those live in separate
  allocations. Writing items through the column pointers is therefore fine
  under both Stacked and Tree Borrows, as long as access is exclusive, which
  the points above ensure.
- **States**: component and archetype ids are only meaningful in one world.
  Every world gets a unique id, and a `QueryState` used with another world
  panics.
- **Sealing**: the query traits cannot be implemented outside the crate, since
  the points above rely on every implementation reporting its access
  correctly. `ReadOnlyQueryData` is sealed separately, as `&mut Local`
  implements `QueryData` and would otherwise be eligible.

## Invariants

The `unsafe` code relies on these invariants:

1. Every alive entity's location points to an in-bounds row of its archetype,
   and that row records the same entity.
2. Outside a structural change, every column of an archetype holds exactly as
   many items as the archetype has entities.
3. Column `i` of an archetype stores values of the type of the archetype's
   `i`-th component id; component ids are sorted and unique.
4. Each set of components has at most one archetype.
5. Archetypes are never removed, and their ids never change.

Debug builds assert invariant 2 after every structural change.

## Keeping the unsafe code sound

- Every `unsafe` block states why it is sound in a `SAFETY` comment, and every
  `unsafe fn` documents its contract; Clippy enforces both.
- Type-erased moves copy `MaybeUninit<u8>` rather than `u8`, so padding bytes
  and pointer provenance survive.
- Debug assertions check bounds and other cheap preconditions of the
  `unsafe` functions.
- The test suite includes panicking destructors, a hand-written bundle that
  panics halfway, zero-sized and over-aligned types, and a randomized
  model-based test of spawn, despawn, insert, remove, clear and queries, both
  cached and not.
- Examples that must not compile, such as a read-only query that writes,
  state the error they expect, and CI checks it on nightly.
- CI runs the suite under [Miri] with strict provenance, under both the
  Stacked Borrows and Tree Borrows aliasing models, and on 32-bit and
  big-endian targets.

[Miri]: https://github.com/rust-lang/miri

## Not yet implemented

Resources, deferred commands, change detection, systems and a parallel
scheduler. See the roadmap in the README.
