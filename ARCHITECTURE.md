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
| `storage/blob_vec.rs` | `BlobVec`, a type-erased `Vec` that holds the values of one component column. |
| `storage/column.rs` | `Column`: a `BlobVec` plus the tick at which each value was added and last changed. |
| `storage/mod.rs` | `drop_each`, which keeps dropping values after a destructor panics. |
| `archetype.rs` | `Archetype` tables, and `Archetypes`: all tables plus the cached `Transition`s between them. |
| `world.rs` | `World`, the public entry point that ties everything together. |
| `change.rs` | `Tick`s, and `Mut`, which marks a component as changed when written through. |
| `resource.rs` | The `Resource` trait and the per-world resource storage. |
| `command.rs` | `CommandQueue` and `Commands`, which record changes to apply later. |
| `snapshot.rs` | `Snapshot`, the copy of a world that `World::snapshot` takes and `World::restore` brings back. |
| `checksum.rs` | The platform-independent hasher behind `World::checksum`. |
| `system/param.rs` | `SystemParam`: what a system can ask for, with `Res` and `ResMut`. |
| `system/access.rs` | `SystemAccess`: what a system reads and writes, and when two conflict. |
| `system/mod.rs` | The `System` trait, and the function and exclusive systems behind `IntoSystem`. |
| `schedule.rs` | `Schedule`, which plans and runs systems on several threads. |
| `query/data.rs` | `QueryData`: what a query fetches, implemented for `&T`, `&mut T`, `Entity`, `Option` and tuples. |
| `query/filter.rs` | `QueryFilter`: `With`, `Without`, `Added`, `Changed`, `Or` and tuples. |
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
Position      (0, 0)    (4, 2)    (1, 1)    <- Column
Velocity      (1, 0)    (0, 1)    (2, 2)    <- Column
```

Each column keeps its values in a `BlobVec`: a contiguous array described only
by a `Layout` and a drop function, so a system iterating over `Position` reads
one dense array. Next to the values, in separate arrays, it keeps the tick at
which each was added and last changed (see [Change detection](#change-detection)),
so queries that ignore them never touch them.
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
runs, and on each run only checks the archetypes created since the last one.
Archetypes are only ever removed by restoring a snapshot, which changes the
world's *archetype epoch*; a `QueryState` that sees a new epoch checks every
archetype again.

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

## Change detection

Every world has a 64-bit **tick** that moves forward each time it is written
to: each spawn, insert, `get_mut`, mutable query run and system run gets a new
tick. A column records, for each value, the tick at which it was added and the
tick at which it last changed, and the ticks move along with the value when
its entity changes archetype. Resources keep the same two ticks.

Queries for `&mut T` yield a `Mut<T>`, which stamps the component with the
query's tick only when it is written through. A `QueryState` remembers the
tick of its last run, and the `Added<T>` and `Changed<T>` filters let through
the rows whose tick is newer. So a query sees what others wrote since it last
ran, but not its own writes. A one-off query has no last run, so to it every
component counts as added and changed; it cannot use these filters, which
only make sense in a query's type. In a system, queries and resources
compare with the system's last run instead, which the world keeps (see
[Systems](#systems)).

Unlike the other filters, these decide row by row, so a query that uses them
only knows an upper bound on its length. The `ArchetypeFilter` marker trait
tells the two kinds apart, and only queries whose filters are all archetypal
are `ExactSizeIterator`.

The ticks are stored in `UnsafeCell`s, since a query writes them through a
shared reference to the world. That is sound for the same reasons as writing
the values themselves (see below): a query only writes the ticks of components
it has exclusive access to. A filter reads a row's tick before the query
yields that row, never after, so a query can both write `T` and filter on
`Changed<T>`.

These filters can only be part of a query's type, never added with
`Query::filter`, which only takes filters that read nothing. A system's
access is worked out from the types of its parameters, so a filter added at
run time could read ticks that a system running alongside is writing.

## Commands and reserved entities

Structural changes need `&mut World`, so they cannot happen while a query is
running. A `CommandQueue` records them instead, as boxed closures, and applies
them later in the order they were recorded.

`Commands::spawn` returns the new entity right away, through a shared
reference to the entity allocator. `Entities` hands out such reservations by
counting an atomic cursor down with a compare-and-swap loop: first over the
free slots, from the end of the free list, then past the last slot ever used.
Each caller gets a different count, the loop refuses to move the count past
the last entity it can hand out, and the free list cannot change while the
allocator is shared, so no lock is needed. Every structural change starts by
**flushing**: making the reserved entities alive, without components, in the
order they were reserved.

Since `Commands` only borrow the entity allocator, which queries never write,
`Query::commands` hands them out alongside any query, even one that writes.
Queries handed to systems refuse to, with a panic: systems may run in
parallel, and the scheduler only keeps systems that say they reserve
entities from doing so at the same time.
A command queue remembers which world it reserved entities in, and panics if
it is applied to another while it holds commands.

## Systems

A system is a function whose parameters implement `SystemParam`: queries,
`Res` and `ResMut`, their `Option` forms, `Commands`, and `&World`. Each
parameter keeps a state between runs, such as a `QueryState` or a command
queue, and reports what it accesses as a `SystemAccess`: components and
resources it reads or writes, whether it reserves entities, and whether it
reads the whole world. Query filters count as reading the components whose
ticks they compare.

When a system first runs, its parameters' accesses are checked against each
other, and a system with two that conflict panics: using both at once could
alias a mutable reference. Two parameters conflict when one writes what the
other accesses, much like two queries; parameters of one system may both
reserve entities, since one thread uses them in a fixed order.

Turning a function into a system takes a trick, the same as in Bevy: a
function `fn(Query<&A>)` is generic over the lifetimes it borrows, so
`SystemParamFunction` requires both `FnMut(P)`, which fixes the parameter
types `P`, and `FnMut` of their items for any lifetime of the world. A
function that takes `&mut World` instead is an *exclusive* system.

`System` and `SystemParam` are sealed. The scheduler trusts what systems
report they access: a system that left something out could race with the
systems running alongside it.

### Scheduling

A `Schedule` splits its systems into phases at each exclusive system. In a
phase, systems run on a shared world, and each waits for every earlier
system of the phase that it conflicts with. That gives the same result as
running the systems one after another, in the order they were added, since
systems that do not conflict touch nothing in common:

- **Ticks** are assigned up front, one per system in that order, and the
  world's tick moves past them before the phase starts. A system's ticks do
  not depend on which finishes first, and a panicking system leaves the
  world's tick ahead of every tick in the world.
- **Entities** reserved by systems running at the same time would be handed
  out in an order that depends on timing. So reserving entities counts as a
  conflict, and systems with `Commands`, or with `&World`, which can make a
  command queue, never run at the same time.
- **Commands** are recorded into each system's own queue, and applied in the
  order the systems were added, before each exclusive system and at the end.

A phase runs on scoped threads that take systems from a shared queue as they
become ready, under one mutex, and wait on a condition variable otherwise.
Each system sits behind a mutex of its own, so the threads can share the list
while each system is only ever run by one of them. A panicking system is
caught, stops the others from starting, and is passed on once those running
have finished.

Writes through a shared world are sound because the scheduler only runs a
system alongside systems it does not conflict with. Resources sit in
`UnsafeCell`s, like ticks, so that a system with `ResMut` can write one
through the shared world.

The tick of each system's last run is kept in the world, not in the system,
so that snapshots copy it and restoring one rewinds systems' change
detection along with everything else.

## Snapshots and rollback

`World::snapshot` copies everything a world holds: the entity allocator, with
its free list, every archetype's entities and columns, ticks included, the
resources, and the world's tick. Components and resources are copied with
`Clone`, through a function registered per type with `register_clone` or
`register_resource_clone`, and a world holding a type that is not registered
cannot be snapshotted. Type-erased cloning goes through the same kind of
function pointer as dropping; if a clone panics, the clones made so far are
dropped.

A world with pending entity reservations cannot be snapshotted either. The
commands that would fill the reserved entities live in a `CommandQueue`,
outside the world, so restoring such a snapshot would bring the reservations
back without them, and no resimulation could reproduce what followed.
Snapshots are meant to be taken between frames, once the queues are
applied.

`World::restore` makes the world indistinguishable from the one the snapshot
was taken of, which is what lets a rollback be resimulated deterministically:

- **Archetypes** are compared with the snapshot's, in order. Those that match
  are kept; from the first that differs on, they are removed, and the
  snapshot's are created in their place. Keeping archetypes created since the
  snapshot would change the order queries visit archetypes in, and so the
  results of a resimulation. Since a removed archetype's id may be reused by
  another, removing any changes the archetype epoch, and the cached
  transitions are forgotten.
- **Component types and resource slots** are not rolled back. Their ids only
  show in the order of an archetype's columns, and so in the order in which
  one entity's components are cloned or dropped. Checksums do not depend on
  them (see below), and registering every component type at startup, in the
  same order on every peer, fixes them anyway.
- **Contents**: the snapshot's columns are swapped into archetypes with the
  same component ids, which keeps invariant 3 below. Snapshots can be taken of
  a world after a rollback, and restored in any order.

Restoring follows the same rule as structural changes. Everything is cloned
from the snapshot first, so that a panicking clone leaves the world unchanged;
then it is swapped in, running no user code; and only then are the replaced
components and resources dropped.

What is not part of the world is not rewound: a `QueryState` still remembers
running at a tick the world may not have reached again, so its change filters
can miss changes, and a `CommandQueue` keeps its commands. `QueryState` is
`Clone`, so that it can be saved along with a snapshot.

## Checksums

`World::checksum` hashes the entity allocator, every archetype's entities in
order, and the components and resources whose types are registered with
`register_hash` or `register_resource_hash`. Peers that compare checksums can
tell that their worlds diverged.

Ticks are left out. Change detection only ever compares ticks with each
other, so code that moves the world's tick without writing anything, such as
systems that read the world to draw it, cannot affect the simulation; leaving
the ticks out keeps it from affecting the checksum too, so that a peer that
draws agrees with one that does not.

Two worlds that cannot be told apart can still have different component ids,
for example when one of them used a component type for the first time in
frames it then rolled back. So rather than visiting an archetype's columns in
order, the checksum visits the registered types in the order they were
registered, and marks each with its position in that order.

The hasher works on 64-bit words made from integer values rather than their
bytes in memory, so that a field hashes the same whatever the platform's byte
order or pointer width; a test pins the checksum of a whole world, and CI runs
it on 32-bit and big-endian targets. The standard library undoes that for
slices, arrays and `Vec`s of integers, which it hashes as their bytes in
memory, so components that hold those must hash their elements one at a time
when peers run on different platforms.

## Determinism

Given the same sequence of operations, a world ends up bit-for-bit the same:
the same entity handles, the same rows in the same archetypes, and the same
change ticks, whether systems ran on one thread or many. That holds because
nothing depends on hash-map iteration order, addresses or timing:

- component types, bundles, archetypes and resources are numbered in the order
  they are first seen, and hash maps are only used for lookups, never iterated
  over;
- entity slots are recycled from a stack, and reservations are numbered in the
  order they are made;
- queries visit archetypes in the order they were created, and rows in table
  order, which only depends on the history of the world;
- ticks only move when the world is written to, and schedules assign them
  to systems in the order the systems were added;
- schedules never run two systems that reserve entities at the same time.

Reserving entities from several threads at once is safe, but which thread
gets which entity then depends on timing, which is why schedules serialize
the systems that do.

## Invariants

The `unsafe` code relies on these invariants:

1. Every alive entity's location points to an in-bounds row of its archetype,
   and that row records the same entity.
2. Outside a structural change, every column of an archetype holds exactly as
   many values, and as many of each kind of tick, as the archetype has
   entities.
3. Column `i` of an archetype stores values of the type of the archetype's
   `i`-th component id; component ids are sorted and unique.
4. Each set of components has at most one archetype.
5. Archetypes are only removed by restoring a snapshot, which changes the
   archetype epoch. While an archetype exists, its id does not change.
6. While entity reservations are pending, the free list does not change: every
   structural change flushes them first.
7. While safe code holds a shared reference to the world, components,
   resources and their ticks are only written by queries and systems that
   hold exclusive access to them: writing takes `&mut World`, a query made
   from one, or a system parameter, and schedules never run a system that
   writes alongside one that reads the same thing, or the whole world.

Debug builds assert invariant 2 after every structural change.

## Keeping the unsafe code sound

- Every `unsafe` block states why it is sound in a `SAFETY` comment, and every
  `unsafe fn` documents its contract; Clippy enforces both.
- Type-erased moves copy `MaybeUninit<u8>` rather than `u8`, so padding bytes
  and pointer provenance survive.
- Debug assertions check bounds and other cheap preconditions of the
  `unsafe` functions.
- The test suite includes panicking destructors and clones, a hand-written
  bundle that panics halfway, zero-sized and over-aligned types, and a
  randomized model-based test of spawn, despawn, insert, remove, clear,
  snapshots, restores and queries, both cached and not.
- A schedule of order-sensitive systems runs on 1, 2, 3 and 8 threads, and
  the worlds are compared after every frame; the test fails when the
  scheduler ignores conflicts.
- Examples that must not compile, such as a read-only query that writes,
  state the error they expect, and CI checks it on nightly.
- CI runs the suite under [Miri] with strict provenance, under both the
  Stacked Borrows and Tree Borrows aliasing models, and on 32-bit and
  big-endian targets.

[Miri]: https://github.com/rust-lang/miri

## Not yet implemented

Events, and systems ordered other than by when they were added. See the
roadmap in the README.
