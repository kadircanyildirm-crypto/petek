//! The world: the container of all entities and their components.

use std::any::TypeId;
use std::error::Error;
use std::fmt;
use std::iter::FusedIterator;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::archetype::{ArchetypeId, Archetypes, TransitionId};
use crate::bundle::{Bundle, Bundles};
use crate::change::{Mut, Tick, Ticks};
use crate::component::{Component, Components};
use crate::entity::{Entities, Entity, EntityLocation};
use crate::query::{Query, QueryData, ReadOnlyQueryData, check_access};
use crate::resource::{Resource, Resources};
use crate::snapshot::Snapshot;

/// Stores entities and their components.
///
/// ```
/// use petek::{Component, World};
///
/// #[derive(Debug, PartialEq)]
/// struct Health(u32);
/// impl Component for Health {}
///
/// let mut world = World::new();
/// let player = world.spawn(Health(100));
/// world.get_mut::<Health>(player).unwrap().0 -= 30;
/// assert_eq!(world.get::<Health>(player), Some(&Health(70)));
/// ```
///
/// An [`Entity`] handle is only meaningful in the world that spawned it.
pub struct World {
    id: WorldId,
    entities: Entities,
    components: Components,
    bundles: Bundles,
    archetypes: Archetypes,
    resources: Resources,
    /// Moves forward every time something is written; see [`Tick`].
    change_tick: Tick,
}

/// Uniquely identifies a world, so that state computed for one world, such as
/// a [`QueryState`](crate::query::QueryState), is never used with another.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct WorldId(usize);

impl WorldId {
    /// Returns an id that no other world has had.
    ///
    /// # Panics
    ///
    /// If the ids have run out, which takes `usize::MAX` worlds.
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        // Unlike `fetch_add`, this never wraps around to an id in use.
        let mut id = NEXT.load(Ordering::Relaxed);
        loop {
            let next = id.checked_add(1).expect("too many worlds");
            match NEXT.compare_exchange_weak(id, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return Self(id),
                Err(current) => id = current,
            }
        }
    }
}

impl World {
    /// Creates an empty world.
    #[must_use]
    pub fn new() -> Self {
        Self {
            id: WorldId::new(),
            entities: Entities::default(),
            components: Components::new(),
            bundles: Bundles::default(),
            archetypes: Archetypes::new(),
            resources: Resources::default(),
            change_tick: Tick::ZERO,
        }
    }

    /// The number of alive entities.
    #[must_use]
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Returns `true` if there are no alive entities.
    #[must_use]
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns `true` if `entity` is alive.
    #[must_use]
    #[inline]
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.contains(entity)
    }

    /// Creates an entity with the components in `bundle`.
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # struct Position(f32, f32);
    /// # impl Component for Position {}
    /// # struct Velocity(f32, f32);
    /// # impl Component for Velocity {}
    /// let mut world = World::new();
    /// let wall = world.spawn(Position(0.0, 0.0));
    /// let ball = world.spawn((Position(0.0, 0.0), Velocity(1.0, 0.0)));
    /// assert!(world.has::<Velocity>(ball) && !world.has::<Velocity>(wall));
    /// ```
    ///
    /// # Panics
    ///
    /// If the bundle contains the same component more than once.
    #[track_caller]
    pub fn spawn<B: Bundle>(&mut self, bundle: B) -> Entity {
        let transition = self.spawn_transition::<B>();
        let tick = self.increment_change_tick();
        // SAFETY: the transition was computed for `B`.
        unsafe { self.spawn_with(transition, bundle, tick) }
    }

    /// Spawns an entity for each bundle in `bundles`.
    ///
    /// This is faster than calling [`spawn`](Self::spawn) in a loop: the
    /// target archetype is looked up once and its storage is reserved up
    /// front. Entities are spawned as the returned iterator is advanced; any
    /// that remain when it is dropped are spawned then.
    ///
    /// ```
    /// # use petek::{Component, Entity, World};
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// let mut world = World::new();
    /// let entities: Vec<Entity> = world.spawn_batch((0..100).map(Health)).collect();
    /// assert_eq!(world.len(), 100);
    /// ```
    ///
    /// # Panics
    ///
    /// If the bundle type contains the same component more than once.
    #[track_caller]
    pub fn spawn_batch<I>(&mut self, bundles: I) -> SpawnBatch<'_, I::IntoIter>
    where
        I: IntoIterator,
        I::Item: Bundle,
    {
        let bundles = bundles.into_iter();
        let transition = self.spawn_transition::<I::Item>();
        let tick = self.increment_change_tick();
        let (_, empty, target) = self.archetypes.split(transition);
        target.unwrap_or(empty).reserve(bundles.size_hint().0);
        SpawnBatch {
            world: self,
            bundles,
            transition,
            tick,
        }
    }

    /// Despawns `entity`, dropping its components.
    ///
    /// # Errors
    ///
    /// Returns [`NoSuchEntity`] if `entity` is not alive.
    pub fn despawn(&mut self, entity: Entity) -> Result<(), NoSuchEntity> {
        self.flush();
        let location = self.entities.free(entity).ok_or(NoSuchEntity(entity))?;
        let archetype = self.archetypes.get_mut(location.archetype);
        let row = location.row as usize;
        // SAFETY: an alive entity's row is in bounds, and it is removed from
        // every column right below.
        if let Some(moved) = unsafe { archetype.swap_remove_entity(row) } {
            self.entities.set_location(moved, location);
        }
        // SAFETY: as above. Destructors run only now that the bookkeeping is
        // consistent again.
        unsafe { archetype.drop_row(row) };
        archetype.debug_assert_consistent();
        Ok(())
    }

    /// Adds the components in `bundle` to `entity`. Components the entity
    /// already has are overwritten, and their old values dropped.
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # #[derive(Debug, PartialEq)]
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// # struct Shield(u32);
    /// # impl Component for Shield {}
    /// let mut world = World::new();
    /// let player = world.spawn(Health(10));
    /// world.insert(player, (Health(20), Shield(5)))?;
    /// assert_eq!(world.get::<Health>(player), Some(&Health(20)));
    /// assert!(world.has::<Shield>(player));
    /// # Ok::<(), petek::NoSuchEntity>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`NoSuchEntity`], and drops the bundle, if `entity` is not
    /// alive.
    ///
    /// # Panics
    ///
    /// If the bundle contains the same component more than once.
    #[track_caller]
    pub fn insert<B: Bundle>(&mut self, entity: Entity, bundle: B) -> Result<(), NoSuchEntity> {
        self.flush();
        let location = self.entities.get(entity).ok_or(NoSuchEntity(entity))?;
        let bundle_id = self.bundles.register::<B>(&mut self.components);
        let transition = self.archetypes.insert_transition(
            location.archetype,
            bundle_id,
            &self.bundles.info(bundle_id).component_ids,
            &self.components,
        );
        let tick = self.increment_change_tick();
        let (transition, src, dst) = self.archetypes.split(transition);
        let src_row = location.row as usize;

        let Some(dst) = dst else {
            // The entity already has all of the bundle's components.
            // SAFETY: an alive entity's row is in bounds, and the transition
            // lists the archetype's columns for `B`'s components.
            unsafe { src.replace_bundle(src_row, bundle, &transition.bundle_columns, tick) };
            return Ok(());
        };

        dst.reserve(1);
        let dst_row = u32::try_from(dst.len()).expect("too many entities in one archetype");
        // Phase 1, part 1: stage the bundle in `dst` without committing it, so
        // that if the bundle panics, nothing has changed yet.
        // SAFETY: the transition lists `dst`'s columns for `B`'s components,
        // and `dst` has room.
        unsafe { dst.stage_bundle(bundle, &transition.bundle_columns) };
        // Phase 1, part 2: move the entity. No user code runs until the world
        // is consistent again.
        // SAFETY: the row is in bounds, the moved columns pair each component
        // of `src` that is not in `B` with its column in `dst`, and `dst` has
        // room. Together with the staged columns, they cover every column of
        // `dst`, so committing completes the new row.
        let moved = unsafe {
            let moved = src.move_row_to(src_row, dst, &transition.moved_columns);
            dst.commit_staged(&transition.bundle_columns, tick);
            moved
        };
        // Overwriting a component changes it but does not add it, so values
        // the bundle replaces pass their `added` tick on to their successors.
        // `move_row_to` left them in place, at `src_row`.
        for &(src_column, dst_column) in &transition.replaced_columns {
            let added = *src.column_mut(src_column).added_tick_mut(src_row);
            *dst.column_mut(dst_column).added_tick_mut(dst_row as usize) = added;
        }
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype: transition.dst,
                row: dst_row,
            },
        );
        if let Some(moved) = moved {
            self.entities.set_location(moved, location);
        }
        // Phase 2: drop the values the bundle replaced.
        // SAFETY: `move_row_to` left `src_row` in exactly the columns that it
        // did not move, which are the replaced ones.
        unsafe { src.drop_replaced(src_row, &transition.replaced_columns) };
        src.debug_assert_consistent();
        dst.debug_assert_consistent();
        Ok(())
    }

    /// Removes the components in `B` from `entity` and returns them.
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # #[derive(Debug, PartialEq)]
    /// # struct Velocity(f32, f32);
    /// # impl Component for Velocity {}
    /// let mut world = World::new();
    /// let ball = world.spawn(Velocity(1.0, 2.0));
    /// assert_eq!(world.remove::<Velocity>(ball)?, Velocity(1.0, 2.0));
    /// assert!(world.remove::<Velocity>(ball).is_err());
    /// # Ok::<(), petek::RemoveError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Leaves the entity untouched and returns
    /// [`RemoveError::NoSuchEntity`] if it is not alive, or
    /// [`RemoveError::MissingComponent`] if it lacks any of the components.
    ///
    /// # Panics
    ///
    /// If the bundle contains the same component more than once.
    #[track_caller]
    pub fn remove<B: Bundle>(&mut self, entity: Entity) -> Result<B, RemoveError> {
        self.flush();
        let location = self.entities.get(entity).ok_or(NoSuchEntity(entity))?;
        let bundle_id = self.bundles.register::<B>(&mut self.components);
        let component_ids = &self.bundles.info(bundle_id).component_ids;
        let Some(transition) = self.archetypes.remove_transition(
            location.archetype,
            bundle_id,
            component_ids,
            &self.components,
        ) else {
            let archetype = self.archetypes.get(location.archetype);
            let missing = component_ids
                .iter()
                .find(|&&id| archetype.column_index(id).is_none())
                .expect("some component is missing");
            return Err(RemoveError::MissingComponent {
                entity,
                component: self.components.info(*missing).name(),
            });
        };
        let (transition, src, dst) = self.archetypes.split(transition);
        let src_row = location.row as usize;

        let Some(dst) = dst else {
            // Only the empty bundle leaves the archetype unchanged.
            // SAFETY: `B` has no components, so there is nothing to take.
            return Ok(unsafe { src.take_bundle(src_row, &transition.bundle_columns) });
        };

        dst.reserve(1);
        let dst_row = u32::try_from(dst.len()).expect("too many entities in one archetype");
        // SAFETY: the row is in bounds, the moved columns pair every column of
        // `dst` with its column in `src`, and `dst` has room, so the new row
        // is complete.
        let moved = unsafe { src.move_row_to(src_row, dst, &transition.moved_columns) };
        self.entities.set_location(
            entity,
            EntityLocation {
                archetype: transition.dst,
                row: dst_row,
            },
        );
        if let Some(moved) = moved {
            self.entities.set_location(moved, location);
        }
        // Taking the bundle completes `src` even if building it panics.
        // SAFETY: `move_row_to` left `src_row` in exactly the columns that it
        // did not move, which are the bundle's.
        let bundle = unsafe { src.take_bundle(src_row, &transition.bundle_columns) };
        src.debug_assert_consistent();
        dst.debug_assert_consistent();
        Ok(bundle)
    }

    /// Returns `true` if `entity` is alive and has a `T` component.
    #[must_use]
    pub fn has<T: Component>(&self, entity: Entity) -> bool {
        self.locate::<T>(entity).is_some()
    }

    /// Returns a reference to `entity`'s `T` component, if it is alive and has
    /// one.
    #[must_use]
    pub fn get<T: Component>(&self, entity: Entity) -> Option<&T> {
        let (archetype, column, row) = self.locate::<T>(entity)?;
        let column = self.archetypes.get(archetype).column(column);
        // SAFETY: `locate` found the column of `T`'s component id, which
        // stores `T`s, and an in-bounds row. `&self` rules out mutation for
        // the returned lifetime.
        Some(unsafe { column.get_unchecked(row).cast::<T>().as_ref() })
    }

    /// Returns a mutable reference to `entity`'s `T` component, if it is alive
    /// and has one.
    ///
    /// Writing through it marks the component as changed; see [`Mut`].
    #[must_use]
    pub fn get_mut<T: Component>(&mut self, entity: Entity) -> Option<Mut<'_, T>> {
        let (archetype, column, row) = self.locate::<T>(entity)?;
        let tick = self.increment_change_tick();
        let column = self.archetypes.get_mut(archetype).column_mut(column);
        // SAFETY: as in `get`, and `&mut self` makes the access exclusive for
        // the returned lifetime.
        let (value, changed) = unsafe { column.get_unchecked_mut(row) };
        // SAFETY: as above.
        let value = unsafe { value.cast::<T>().as_mut() };
        Some(Mut::new(value, changed, tick))
    }

    /// Returns a query that reads the components in `D` of every entity that
    /// has them.
    ///
    /// Any number of read-only queries can be used at the same time. See the
    /// [`query`](crate::query) module for what `D` can be, and how to filter
    /// the query further.
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # struct Health(u32);
    /// # impl Component for Health {}
    /// let mut world = World::new();
    /// world.spawn_batch([Health(10), Health(20)]);
    ///
    /// let total: u32 = world.query::<&Health>().into_iter().map(|health| health.0).sum();
    /// assert_eq!(total, 30);
    /// ```
    #[must_use]
    pub fn query<D: ReadOnlyQueryData>(&self) -> Query<'_, D> {
        let data = D::get_state(&self.components);
        let ticks = one_off_ticks(self.change_tick);
        // SAFETY: the state comes from this world, `D` only reads, and `&self`
        // rules out writes for the query's lifetime.
        unsafe { Query::new(self, data, (), ticks) }
    }

    /// Returns a query that reads or writes the components in `D` of every
    /// entity that has them.
    ///
    /// ```
    /// # use petek::{Component, World};
    /// # struct Position(f32);
    /// # impl Component for Position {}
    /// # struct Velocity(f32);
    /// # impl Component for Velocity {}
    /// let mut world = World::new();
    /// let ball = world.spawn((Position(0.0), Velocity(2.0)));
    ///
    /// for (mut position, velocity) in world.query_mut::<(&mut Position, &Velocity)>() {
    ///     position.0 += velocity.0;
    /// }
    /// assert_eq!(world.get::<Position>(ball).unwrap().0, 2.0);
    /// ```
    ///
    /// # Panics
    ///
    /// If `D` accesses a component mutably and also elsewhere, such as
    /// `(&mut Position, &Position)`, since that would alias a mutable
    /// reference.
    #[must_use]
    #[track_caller]
    pub fn query_mut<D: QueryData>(&mut self) -> Query<'_, D> {
        let data = D::init_state(&mut self.components);
        check_access::<D>(&data, &self.components);
        let tick = self.increment_change_tick();
        let ticks = one_off_ticks(tick);
        // SAFETY: the state comes from this world, `D` accesses no component
        // mutably more than once, and `&mut self` gives the query exclusive
        // access to the world for its lifetime.
        unsafe { Query::new(self, Some(data), (), ticks) }
    }

    /// Despawns every entity, dropping all components.
    ///
    /// Resources are kept. Component types and archetypes stay registered and
    /// keep their storage, so refilling the world is cheap.
    pub fn clear(&mut self) {
        self.flush();
        self.entities.clear();
        self.archetypes.clear_entities();
    }

    /// Inserts a resource, returning the previous resource of the same type,
    /// if any.
    ///
    /// ```
    /// # use petek::{Resource, World};
    /// #[derive(Debug, PartialEq)]
    /// struct Score(u32);
    /// impl Resource for Score {}
    ///
    /// let mut world = World::new();
    /// assert_eq!(world.insert_resource(Score(1)), None);
    /// assert_eq!(world.insert_resource(Score(2)), Some(Score(1)));
    /// ```
    pub fn insert_resource<R: Resource>(&mut self, resource: R) -> Option<R> {
        self.resources.insert(resource)
    }

    /// Removes the resource of type `R` and returns it, if there is one.
    pub fn remove_resource<R: Resource>(&mut self) -> Option<R> {
        self.resources.remove()
    }

    /// Returns `true` if the world holds a resource of type `R`.
    #[must_use]
    pub fn contains_resource<R: Resource>(&self) -> bool {
        self.resource::<R>().is_some()
    }

    /// Returns a reference to the resource of type `R`, if there is one.
    #[must_use]
    pub fn resource<R: Resource>(&self) -> Option<&R> {
        self.resources.get()
    }

    /// Returns a mutable reference to the resource of type `R`, if there is
    /// one.
    #[must_use]
    pub fn resource_mut<R: Resource>(&mut self) -> Option<&mut R> {
        self.resources.get_mut()
    }

    /// Registers the component type `C` for [snapshots](Self::snapshot),
    /// which copy components by cloning them.
    pub fn register_clone<C: Component + Clone>(&mut self) {
        self.components.register_clone::<C>();
    }

    /// Registers the resource type `R` for [snapshots](Self::snapshot),
    /// which copy resources by cloning them.
    pub fn register_resource_clone<R: Resource + Clone>(&mut self) {
        self.resources.register_clone::<R>();
    }

    /// Takes a snapshot of the world, which [`restore`](Self::restore) can
    /// bring it back to later.
    ///
    /// This is what rollback networking needs: simulate ahead with the
    /// inputs you have, and when the real ones arrive late, restore the
    /// snapshot from before they mattered and simulate again.
    ///
    /// ```
    /// use petek::{Component, Resource, World};
    ///
    /// #[derive(Clone, Debug, PartialEq)]
    /// struct Position(i32);
    /// impl Component for Position {}
    ///
    /// #[derive(Clone, Debug, PartialEq)]
    /// struct Frame(u32);
    /// impl Resource for Frame {}
    ///
    /// let mut world = World::new();
    /// world.register_clone::<Position>();
    /// world.register_resource_clone::<Frame>();
    /// let player = world.spawn(Position(0));
    /// world.insert_resource(Frame(0));
    ///
    /// let saved = world.snapshot();
    /// world.get_mut::<Position>(player).unwrap().0 += 5;
    /// world.spawn(Position(10));
    /// world.resource_mut::<Frame>().unwrap().0 += 1;
    ///
    /// world.restore(&saved);
    /// assert_eq!(world.get::<Position>(player), Some(&Position(0)));
    /// assert_eq!(world.len(), 1);
    /// assert_eq!(world.resource::<Frame>(), Some(&Frame(0)));
    /// ```
    ///
    /// The snapshot clones every component and resource in the world, so
    /// their types must be registered first, with
    /// [`register_clone`](Self::register_clone) and
    /// [`register_resource_clone`](Self::register_resource_clone).
    ///
    /// # Panics
    ///
    /// If the world holds a component or resource whose type is not
    /// registered. If cloning one panics, the panic is passed on.
    #[must_use]
    #[track_caller]
    pub fn snapshot(&self) -> Snapshot {
        if let Some(name) = self.archetypes.uncloneable(&self.components) {
            panic!(
                "cannot snapshot a world holding `{name}` components: \
                 register them with `World::register_clone` first"
            );
        }
        if let Some(name) = self.resources.uncloneable() {
            panic!(
                "cannot snapshot a world holding a `{name}` resource: \
                 register it with `World::register_resource_clone` first"
            );
        }
        Snapshot {
            world: self.id,
            entities: self.entities.clone(),
            // SAFETY: queries that write hold the world exclusively, so with
            // `&self` held, no one is writing any component.
            archetypes: unsafe { self.archetypes.snapshot(&self.components) },
            resources: self.resources.snapshot(),
            change_tick: self.change_tick,
        }
    }

    /// Brings the world back to the state it was in when `snapshot` was
    /// taken.
    ///
    /// The world then cannot be told apart from what it was: it has the same
    /// entities, components, resources and ticks, and stores them in the same
    /// order, so the same operations have the same results again. Entity
    /// handles spawned since the snapshot was taken mean nothing anymore, and
    /// may come to refer to other entities.
    ///
    /// What is not part of the world is not rewound, such as the commands in
    /// a [`CommandQueue`](crate::command::CommandQueue), or the last run of a
    /// [`QueryState`](crate::query::QueryState).
    ///
    /// The snapshot is left as it is, so the world can be brought back to it
    /// any number of times.
    ///
    /// # Panics
    ///
    /// If the snapshot was taken of another world. If cloning a component or
    /// resource panics, the panic is passed on, and the world is left
    /// unchanged.
    #[track_caller]
    pub fn restore(&mut self, snapshot: &Snapshot) {
        assert!(
            snapshot.world == self.id,
            "a `Snapshot` was restored into a world other than the one it was taken of",
        );
        // Clone the snapshot's contents first, so that if a clone panics, the
        // world is left unchanged.
        let mut archetypes: Vec<_> = snapshot
            .archetypes
            .iter()
            .map(|archetype| archetype.clone_with(&self.components))
            .collect();
        let mut resources = self.resources.clone_snapshot(&snapshot.resources);
        // Swap them in. No user code runs until the world is consistent
        // again.
        let removed = self.archetypes.restore(&mut archetypes, &self.components);
        self.resources.restore(&mut resources);
        self.entities.clone_from(&snapshot.entities);
        self.change_tick = snapshot.change_tick;
        // Only now drop what the world held before, which runs destructors.
        drop((removed, archetypes, resources));
    }

    /// The world's current tick, which moves forward every time something is
    /// written.
    #[must_use]
    #[inline]
    pub fn change_tick(&self) -> Tick {
        self.change_tick
    }

    /// Moves the world's tick forward, and returns the new tick, at which the
    /// write that follows happens.
    #[inline]
    pub(crate) fn increment_change_tick(&mut self) -> Tick {
        self.change_tick = self.change_tick.next();
        self.change_tick
    }

    /// Makes reserved entities alive, without components, in the order they
    /// were reserved.
    pub(crate) fn flush(&mut self) {
        let pending = self.entities.pending();
        if pending == 0 {
            return;
        }
        let empty = self.archetypes.get_mut(ArchetypeId::EMPTY);
        // Checking and reserving up front keeps the placement from panicking.
        assert!(
            u32::try_from(empty.len() + pending).is_ok(),
            "too many entities in one archetype",
        );
        empty.reserve(pending);
        self.entities.flush(|entity| {
            let row = u32::try_from(empty.len()).expect("checked above");
            // SAFETY: the empty archetype has no columns, so the entity alone
            // completes its row.
            unsafe { empty.push_entity(entity) };
            EntityLocation {
                archetype: ArchetypeId::EMPTY,
                row,
            }
        });
    }

    #[inline]
    pub(crate) fn id(&self) -> WorldId {
        self.id
    }

    #[inline]
    pub(crate) fn entities(&self) -> &Entities {
        &self.entities
    }

    #[inline]
    pub(crate) fn components(&self) -> &Components {
        &self.components
    }

    #[inline]
    pub(crate) fn components_mut(&mut self) -> &mut Components {
        &mut self.components
    }

    #[inline]
    pub(crate) fn archetypes(&self) -> &Archetypes {
        &self.archetypes
    }

    /// Returns the transition that spawns entities with a `B`.
    #[track_caller]
    fn spawn_transition<B: Bundle>(&mut self) -> TransitionId {
        let bundle = self.bundles.register::<B>(&mut self.components);
        self.archetypes.insert_transition(
            ArchetypeId::EMPTY,
            bundle,
            &self.bundles.info(bundle).component_ids,
            &self.components,
        )
    }

    /// Spawns an entity with the components in `bundle`, added at `tick`.
    ///
    /// # Safety
    ///
    /// `transition` must be the result of
    /// [`spawn_transition::<B>`](Self::spawn_transition).
    unsafe fn spawn_with<B: Bundle>(
        &mut self,
        transition: TransitionId,
        bundle: B,
        tick: Tick,
    ) -> Entity {
        self.flush();
        let (transition, empty, target) = self.archetypes.split(transition);
        let archetype = target.unwrap_or(empty);
        archetype.reserve(1);
        let row = u32::try_from(archetype.len()).expect("too many entities in one archetype");
        // Stage the bundle first, so that if it panics, nothing has changed.
        // SAFETY: the transition lists the archetype's columns for `B`'s
        // components, and there is room.
        unsafe { archetype.stage_bundle(bundle, &transition.bundle_columns) };
        let entity = self.entities.alloc(EntityLocation {
            archetype: transition.dst,
            row,
        });
        // SAFETY: the archetype's components are exactly `B`'s, so every
        // column holds a staged value, and committing them and pushing the
        // entity completes the row.
        unsafe {
            archetype.commit_staged(&transition.bundle_columns, tick);
            archetype.push_entity(entity);
        }
        archetype.debug_assert_consistent();
        entity
    }

    /// Returns the archetype, column and row that store `entity`'s `T`.
    #[inline]
    fn locate<T: Component>(&self, entity: Entity) -> Option<(ArchetypeId, usize, usize)> {
        let location = self.entities.get(entity)?;
        let id = self.components.id(TypeId::of::<T>())?;
        let column = self.archetypes.get(location.archetype).column_index(id)?;
        Some((location.archetype, column, location.row as usize))
    }
}

/// The ticks of a one-off query: it has never run before, so everything counts
/// as added and changed.
fn one_off_ticks(this_run: Tick) -> Ticks {
    Ticks {
        last_run: Tick::ZERO,
        this_run,
    }
}

impl Default for World {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for World {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("World")
            .field("entities", &self.len())
            .field("archetypes", &self.archetypes.len())
            .field("resources", &self.resources.len())
            .finish_non_exhaustive()
    }
}

/// An iterator that spawns entities, returned by [`World::spawn_batch`].
///
/// Entities it has not spawned yet are spawned when it is dropped.
pub struct SpawnBatch<'w, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    world: &'w mut World,
    bundles: I,
    transition: TransitionId,
    /// The tick at which the whole batch counts as added.
    tick: Tick,
}

impl<I> Iterator for SpawnBatch<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    type Item = Entity;

    #[inline]
    fn next(&mut self) -> Option<Entity> {
        let bundle = self.bundles.next()?;
        // SAFETY: `spawn_batch` computed the transition for `I::Item`.
        Some(unsafe { self.world.spawn_with(self.transition, bundle, self.tick) })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.bundles.size_hint()
    }
}

impl<I> ExactSizeIterator for SpawnBatch<'_, I>
where
    I: ExactSizeIterator,
    I::Item: Bundle,
{
}

impl<I> FusedIterator for SpawnBatch<'_, I>
where
    I: FusedIterator,
    I::Item: Bundle,
{
}

impl<I> Drop for SpawnBatch<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    fn drop(&mut self) {
        self.by_ref().for_each(drop);
    }
}

impl<I> fmt::Debug for SpawnBatch<'_, I>
where
    I: Iterator,
    I::Item: Bundle,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnBatch").finish_non_exhaustive()
    }
}

/// The error returned when an entity was never spawned or has been despawned.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NoSuchEntity(pub Entity);

impl fmt::Display for NoSuchEntity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "entity {} does not exist", self.0)
    }
}

impl Error for NoSuchEntity {}

/// The error returned by [`World::remove`].
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[non_exhaustive]
pub enum RemoveError {
    /// The entity was never spawned or has been despawned.
    NoSuchEntity(Entity),
    /// The entity lacks a component of the bundle.
    MissingComponent {
        /// The entity the components were to be removed from.
        entity: Entity,
        /// The name of a component the entity lacks, for diagnostics only.
        component: &'static str,
    },
}

impl From<NoSuchEntity> for RemoveError {
    fn from(error: NoSuchEntity) -> Self {
        Self::NoSuchEntity(error.0)
    }
}

impl fmt::Display for RemoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoSuchEntity(entity) => write!(f, "entity {entity} does not exist"),
            Self::MissingComponent { entity, component } => {
                write!(f, "entity {entity} has no `{component}` component")
            }
        }
    }
}

impl Error for RemoveError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::ComponentInfo;
    use std::panic::{self, AssertUnwindSafe};
    use std::ptr::NonNull;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, PartialEq)]
    struct Pos(i32);
    impl Component for Pos {}

    #[derive(Debug, PartialEq)]
    struct Vel(i32);
    impl Component for Vel {}

    #[derive(Debug, PartialEq)]
    struct Name(String);
    impl Component for Name {}

    #[derive(Debug, PartialEq)]
    struct Marker;
    impl Component for Marker {}

    /// Counts its drops in a shared counter.
    struct Tracked(Arc<AtomicUsize>);
    impl Component for Tracked {}

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Panics when dropped while armed.
    struct Bomb {
        armed: bool,
    }
    impl Component for Bomb {}

    impl Drop for Bomb {
        fn drop(&mut self) {
            assert!(!self.armed, "boom");
        }
    }

    fn catch(f: impl FnOnce()) -> bool {
        panic::catch_unwind(AssertUnwindSafe(f)).is_err()
    }

    #[test]
    fn world_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<World>();
    }

    #[test]
    fn spawn_and_get() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Vel(2)));
        let b = world.spawn((Name("b".into()), Pos(3)));
        let c = world.spawn(());

        assert_eq!(world.len(), 3);
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(2)));
        assert_eq!(world.get::<Name>(a), None);
        assert_eq!(world.get::<Name>(b), Some(&Name("b".into())));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(3)));
        assert!(world.contains(c));
        assert!(!world.has::<Marker>(c));
    }

    #[test]
    fn get_mut_modifies_in_place() {
        let mut world = World::new();
        let entity = world.spawn((Pos(1), Marker));
        world.get_mut::<Pos>(entity).unwrap().0 = 10;
        assert_eq!(world.get::<Pos>(entity), Some(&Pos(10)));
        assert_eq!(world.get::<Marker>(entity), Some(&Marker));
    }

    #[test]
    fn despawn_invalidates_the_handle() {
        let mut world = World::new();
        let old = world.spawn(Pos(1));
        assert_eq!(world.despawn(old), Ok(()));
        assert_eq!(world.despawn(old), Err(NoSuchEntity(old)));
        assert!(!world.contains(old));
        assert!(world.is_empty());

        // The new entity reuses the slot, but the old handle must not see it.
        let new = world.spawn(Pos(2));
        assert_eq!(new.index(), old.index());
        assert_eq!(world.get::<Pos>(old), None);
        assert_eq!(world.get::<Pos>(new), Some(&Pos(2)));
    }

    #[test]
    fn despawn_keeps_the_other_rows_intact() {
        let mut world = World::new();
        let entities: Vec<_> = (0..5)
            .map(|i| world.spawn((Pos(i), Name(i.to_string()))))
            .collect();

        // Despawning row 1 moves the last row into its place.
        world.despawn(entities[1]).unwrap();
        for (i, &entity) in (0..).zip(&entities).filter(|&(i, _)| i != 1) {
            assert_eq!(world.get::<Pos>(entity), Some(&Pos(i)));
            assert_eq!(world.get::<Name>(entity), Some(&Name(i.to_string())));
        }
    }

    #[test]
    fn components_are_dropped_exactly_once() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        let a = world.spawn((Pos(0), Tracked(drops.clone())));
        world.spawn(Tracked(drops.clone()));
        world.spawn((Tracked(drops.clone()), Marker));

        world.despawn(a).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(world);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    #[should_panic(expected = "more than once")]
    fn spawning_a_bundle_with_duplicates_panics() {
        World::new().spawn((Pos(0), Pos(1)));
    }

    #[test]
    fn insert_adds_components() {
        let mut world = World::new();
        let a = world.spawn(Pos(1));
        let b = world.spawn(Pos(2));

        assert_eq!(world.insert(a, (Vel(10), Marker)), Ok(()));
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(10)));
        assert_eq!(world.get::<Marker>(a), Some(&Marker));
        // `b` moved into the row `a` left behind.
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));
        assert_eq!(world.get::<Vel>(b), None);
    }

    #[test]
    fn insert_overwrites_existing_components() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        let entity = world.spawn((Pos(1), Tracked(drops.clone())));

        // Every component already exists: overwritten in place.
        world.insert(entity, Tracked(drops.clone())).unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 1);

        // Some exist and some don't: the entity moves, and the old `Tracked`
        // is dropped while `Pos` is carried over.
        world
            .insert(entity, (Tracked(drops.clone()), Vel(5)))
            .unwrap();
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert_eq!(world.get::<Pos>(entity), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(entity), Some(&Vel(5)));

        drop(world);
        assert_eq!(drops.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn insert_into_a_dead_entity_drops_the_bundle() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        let entity = world.spawn(());
        world.despawn(entity).unwrap();

        let result = world.insert(entity, Tracked(drops.clone()));
        assert_eq!(result, Err(NoSuchEntity(entity)));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn remove_returns_the_components() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Vel(2), Name("a".into())));
        let b = world.spawn((Pos(3), Vel(4), Name("b".into())));

        let removed = world.remove::<(Name, Pos)>(a);
        assert_eq!(removed, Ok((Name("a".into()), Pos(1))));
        assert!(!world.has::<Pos>(a));
        assert!(!world.has::<Name>(a));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(2)));
        assert_eq!(world.get::<Name>(b), Some(&Name("b".into())));
        assert_eq!(world.remove::<()>(a), Ok(()));
    }

    #[test]
    fn remove_reports_errors_without_changing_anything() {
        let mut world = World::new();
        let entity = world.spawn((Pos(1), Vel(2)));

        let error = world.remove::<(Pos, Name)>(entity).unwrap_err();
        assert!(matches!(
            error,
            RemoveError::MissingComponent { component, .. } if component.ends_with("Name"),
        ));
        assert_eq!(world.get::<Pos>(entity), Some(&Pos(1)));

        world.despawn(entity).unwrap();
        assert_eq!(
            world.remove::<Pos>(entity),
            Err(RemoveError::NoSuchEntity(entity))
        );
    }

    #[test]
    fn archetypes_are_reused() {
        let mut world = World::new();
        let entity = world.spawn(Pos(0));
        world.insert(entity, Vel(0)).unwrap();
        world.remove::<Vel>(entity).unwrap();
        let archetypes = world.archetypes.len();

        for _ in 0..10 {
            world.insert(entity, Vel(0)).unwrap();
            world.remove::<Vel>(entity).unwrap();
        }
        world.spawn((Vel(1), Pos(1)));
        assert_eq!(world.archetypes.len(), archetypes);
    }

    #[test]
    fn panicking_destructor_during_despawn_still_drops_the_rest() {
        static COUNTED_DROPS: AtomicUsize = AtomicUsize::new(0);

        struct Counted;
        impl Component for Counted {}

        impl Drop for Counted {
            fn drop(&mut self) {
                COUNTED_DROPS.fetch_add(1, Ordering::Relaxed);
            }
        }

        let mut world = World::new();
        // Registration order puts the `Counted` column after the `Bomb` one.
        let a = world.spawn((Pos(1), Bomb { armed: true }, Counted));
        let b = world.spawn((Pos(2), Bomb { armed: false }, Counted));

        assert!(catch(|| world.despawn(a).unwrap()));
        // `a` is gone, and its `Counted` was dropped after the bomb went off.
        // `b`, which moved into `a`'s row, is intact.
        assert!(!world.contains(a));
        assert_eq!(world.len(), 1);
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));
        assert_eq!(COUNTED_DROPS.load(Ordering::Relaxed), 1);
        drop(world);
        assert_eq!(COUNTED_DROPS.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn panicking_destructor_during_insert_leaves_the_world_consistent() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Bomb { armed: true }));
        let b = world.spawn((Pos(2), Bomb { armed: false }));

        // The armed bomb is replaced while the entity moves to a new
        // archetype, so it goes off after the move is complete.
        assert!(catch(|| world
            .insert(a, (Bomb { armed: false }, Vel(1)))
            .unwrap()));
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert_eq!(world.get::<Vel>(a), Some(&Vel(1)));
        assert_eq!(world.get::<Bomb>(a).map(|bomb| bomb.armed), Some(false));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));

        // The same, but overwriting in place.
        world.insert(b, Bomb { armed: true }).unwrap();
        assert!(catch(|| world.insert(b, Bomb { armed: false }).unwrap()));
        assert_eq!(world.get::<Bomb>(b).map(|bomb| bomb.armed), Some(false));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(2)));
    }

    #[test]
    fn panicking_destructor_during_clear_still_clears_everything() {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut world = World::new();
        world.spawn((Pos(0), Tracked(drops.clone())));
        let a = world.spawn((Bomb { armed: true }, Tracked(drops.clone())));
        world.spawn(Tracked(drops.clone()));

        assert!(catch(|| world.clear()));
        assert!(world.is_empty());
        assert!(!world.contains(a));
        assert_eq!(drops.load(Ordering::Relaxed), 3);

        let b = world.spawn((Pos(1), Tracked(drops.clone())));
        assert_eq!(world.get::<Pos>(b), Some(&Pos(1)));
    }

    /// A hand-written bundle that panics after handing over its components,
    /// and in the middle of being rebuilt. Correct bundles never do this, but
    /// the world must stay consistent anyway.
    struct Unwinding(Vel, Pos);

    // SAFETY: it reports, hands over and takes a `Vel` and then a `Pos`, as
    // far as it gets before panicking.
    unsafe impl Bundle for Unwinding {
        fn component_infos(f: &mut impl FnMut(ComponentInfo)) {
            <(Vel, Pos)>::component_infos(f);
        }

        fn take_components(self, f: &mut impl FnMut(NonNull<u8>)) {
            (self.0, self.1).take_components(f);
            panic!("unwinding after handing over the components");
        }

        unsafe fn from_components(f: &mut impl FnMut() -> NonNull<u8>) -> Self {
            // SAFETY: the caller hands over a `Vel` first.
            let _vel = unsafe { Vel::from_components(f) };
            panic!("unwinding halfway through");
        }
    }

    #[test]
    fn unwinding_bundles_leave_the_world_consistent() {
        let mut world = World::new();
        let a = world.spawn((Pos(1), Name("a".into())));
        let b = world.spawn((Pos(2), Name("b".into())));

        // Nothing is spawned.
        assert!(catch(|| {
            world.spawn(Unwinding(Vel(0), Pos(0)));
        }));
        assert_eq!(world.len(), 2);

        // Nothing is inserted, and nothing moves.
        assert!(catch(|| world
            .insert(a, Unwinding(Vel(3), Pos(3)))
            .unwrap()));
        assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
        assert!(!world.has::<Vel>(a));
        // With a stale location, this would write to freed memory.
        world.get_mut::<Name>(b).unwrap().0.push_str(" grew");
        assert_eq!(world.get::<Name>(b), Some(&Name("b grew".into())));

        // The components are removed, even though the bundle is never built.
        world.insert(a, Vel(4)).unwrap();
        assert!(catch(|| drop(world.remove::<Unwinding>(a))));
        assert!(!world.has::<Vel>(a));
        assert!(!world.has::<Pos>(a));
        assert_eq!(world.get::<Name>(a), Some(&Name("a".into())));
        assert_eq!(world.get::<Name>(b), Some(&Name("b grew".into())));
        assert_eq!(world.len(), 2);
    }
}
