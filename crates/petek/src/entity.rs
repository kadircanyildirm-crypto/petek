//! Entity handles and the allocator that hands them out.

use std::fmt;
use std::hash::Hasher;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicIsize, Ordering};

use crate::archetype::ArchetypeId;
use crate::checksum::ChecksumHasher;

/// A lightweight handle to an entity.
///
/// An `Entity` is a slot index paired with a *generation*. Indices are
/// recycled when entities are despawned, but each reuse bumps the slot's
/// generation, so a stale handle to a despawned entity is detected instead of
/// silently referring to whichever entity took its slot.
///
/// A handle is only meaningful in the world that spawned it. In another world
/// it may refer to an unrelated entity, or to none.
///
/// The generation is never zero, which keeps `Option<Entity>` at 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entity {
    index: u32,
    generation: NonZeroU32,
}

impl Entity {
    #[inline]
    pub(crate) const fn new(index: u32, generation: NonZeroU32) -> Self {
        Self { index, generation }
    }

    /// The slot index of this entity. Indices are reused after despawning.
    #[must_use]
    #[inline]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// The generation of this entity's slot: 1 for the first entity to use
    /// the slot, and one more each time the slot is reused.
    #[must_use]
    #[inline]
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// Packs this entity into a `u64`, e.g. for serialization or networking.
    ///
    /// The generation goes in the high 32 bits and the index in the low 32.
    #[must_use]
    #[inline]
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// Unpacks an entity produced by [`to_bits`](Self::to_bits).
    ///
    /// Returns `None` if `bits` has a zero generation, which no entity can
    /// have. Whether the entity is alive can only be checked against its
    /// world, with [`World::contains`](crate::World::contains).
    #[must_use]
    #[inline]
    #[allow(
        clippy::cast_possible_truncation,
        reason = "splitting the bits into two halves"
    )]
    pub const fn from_bits(bits: u64) -> Option<Self> {
        match NonZeroU32::new((bits >> 32) as u32) {
            Some(generation) => Some(Self::new(bits as u32, generation)),
            None => None,
        }
    }
}

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Where an entity's components are stored.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct EntityLocation {
    pub(crate) archetype: ArchetypeId,
    /// The entity's row in the archetype.
    pub(crate) row: u32,
}

/// Allocates entity handles and tracks where each alive entity is stored.
///
/// Entities can also be *reserved* through a shared reference, which lets
/// code that cannot change the world, such as commands recorded during a
/// query, hand out entities that will exist later. Reserved entities become
/// alive when the world [flushes](Self::flush) them, which it does before any
/// other change to its entities.
#[derive(Default)]
pub(crate) struct Entities {
    meta: Vec<EntityMeta>,
    /// Indices of free slots, ready to be reused. Slots are taken from the
    /// end.
    free: Vec<u32>,
    /// How many slots of `free` have not been reserved, counting down as
    /// reservations take them from the end. Once it is zero, each further
    /// step below zero reserves a brand-new slot past the end of `meta`.
    ///
    /// Equal to `free.len()` while no reservations are pending.
    unreserved: AtomicIsize,
    /// Number of alive entities, not counting pending reservations.
    len: usize,
}

#[derive(Clone, Copy)]
struct EntityMeta {
    /// The generation of the entity that lives, or will next live, in this
    /// slot.
    generation: NonZeroU32,
    /// `None` while the slot is free.
    location: Option<EntityLocation>,
}

impl EntityMeta {
    /// Frees the slot, returning whether it can be reused.
    ///
    /// A slot whose generation would overflow is retired for good rather than
    /// wrapped around, so a stale handle can never become valid again.
    fn release(&mut self) -> bool {
        self.location = None;
        match self.generation.checked_add(1) {
            Some(next) => {
                self.generation = next;
                true
            }
            None => false,
        }
    }
}

impl Entities {
    /// Allocates a new entity stored at `location`, reusing a free slot if
    /// there is one.
    ///
    /// No reservations may be pending.
    pub(crate) fn alloc(&mut self, location: EntityLocation) -> Entity {
        debug_assert_eq!(self.pending(), 0, "reserved entities must be flushed first");
        let entity = if let Some(index) = self.free.pop() {
            let meta = &mut self.meta[index as usize];
            meta.location = Some(location);
            Entity::new(index, meta.generation)
        } else {
            let index = u32::try_from(self.meta.len()).expect("too many entities");
            self.meta.push(EntityMeta {
                generation: NonZeroU32::MIN,
                location: Some(location),
            });
            Entity::new(index, NonZeroU32::MIN)
        };
        self.len += 1;
        self.sync_unreserved();
        entity
    }

    /// Frees `entity` so its slot can be reused, returning where it was
    /// stored.
    ///
    /// Returns `None`, and does nothing, if `entity` was not alive. No
    /// reservations may be pending.
    pub(crate) fn free(&mut self, entity: Entity) -> Option<EntityLocation> {
        debug_assert_eq!(self.pending(), 0, "reserved entities must be flushed first");
        let location = self.get(entity)?;
        if self.meta[entity.index as usize].release() {
            self.free.push(entity.index);
        }
        self.len -= 1;
        self.sync_unreserved();
        Some(location)
    }

    /// Panics if there are no entities left to hand out, so that callers can
    /// check before [`alloc`](Self::alloc) would.
    pub(crate) fn check_room(&self) {
        assert!(
            !self.free.is_empty() || u32::try_from(self.meta.len()).is_ok(),
            "too many entities",
        );
    }

    /// Gives up every pending reservation: the reserved entities never become
    /// alive, and their slots may be handed out again, with the same
    /// generations.
    pub(crate) fn cancel_reservations(&mut self) {
        self.sync_unreserved();
    }

    /// Frees every alive entity. No reservations may be pending.
    pub(crate) fn clear(&mut self) {
        debug_assert_eq!(self.pending(), 0, "reserved entities must be flushed first");
        for (index, meta) in (0..).zip(&mut self.meta) {
            if meta.location.is_some() && meta.release() {
                self.free.push(index);
            }
        }
        self.len = 0;
        self.sync_unreserved();
    }

    /// Reserves an entity, which becomes alive at the next
    /// [`flush`](Self::flush).
    ///
    /// This only needs a shared reference, and can be called from several
    /// threads at once. Reservations made in the same order get the same
    /// entities.
    ///
    /// # Panics
    ///
    /// If there are no entities left to hand out.
    pub(crate) fn reserve(&self) -> Entity {
        // A compare-and-swap loop rather than `fetch_sub`, so that the count
        // never moves when there is nothing left to hand out, and so can
        // never wrap around to hand out an entity twice. `free` and `meta`
        // cannot change while `&self` is held, so no stronger ordering is
        // needed than what makes each reservation unique.
        let mut unreserved = self.unreserved.load(Ordering::Relaxed);
        loop {
            let entity = self.reservation(unreserved);
            let next = unreserved.checked_sub(1).expect("too many entities");
            match self.unreserved.compare_exchange_weak(
                unreserved,
                next,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return entity,
                Err(current) => unreserved = current,
            }
        }
    }

    /// The entity that a reservation gets while `unreserved` free slots are
    /// left.
    ///
    /// # Panics
    ///
    /// If there are no entities left to hand out.
    fn reservation(&self, unreserved: isize) -> Entity {
        if unreserved > 0 {
            let index = self.free[unreserved.unsigned_abs() - 1];
            Entity::new(index, self.meta[index as usize].generation)
        } else {
            let index = self
                .meta
                .len()
                .checked_add(unreserved.unsigned_abs())
                .and_then(|index| u32::try_from(index).ok())
                .expect("too many entities");
            Entity::new(index, NonZeroU32::MIN)
        }
    }

    /// The number of reserved entities that are not alive yet.
    pub(crate) fn pending(&mut self) -> usize {
        let unreserved = *self.unreserved.get_mut();
        self.free
            .len()
            .checked_add_signed(-unreserved)
            .expect("reservations are counted from `free`")
    }

    /// Makes every reserved entity alive, in the order they were reserved,
    /// stored where `place` puts it.
    ///
    /// `place` must not panic, or some reserved entities may never become
    /// alive. That would not be unsound, but their handles would be useless.
    ///
    /// # Panics
    ///
    /// If there are no entities left to hand out.
    pub(crate) fn flush(&mut self, mut place: impl FnMut(Entity) -> EntityLocation) {
        let unreserved = *self.unreserved.get_mut();
        // Reservations took the free slots from the end, and then new slots.
        let reused = self.free.len().min(unreserved.max(0).unsigned_abs());
        for index in self.free.drain(reused..).rev() {
            let meta = &mut self.meta[index as usize];
            let entity = Entity::new(index, meta.generation);
            meta.location = Some(place(entity));
            self.len += 1;
        }
        for _ in 0..unreserved.min(0).unsigned_abs() {
            let index = u32::try_from(self.meta.len()).expect("too many entities");
            let entity = Entity::new(index, NonZeroU32::MIN);
            let location = place(entity);
            self.meta.push(EntityMeta {
                generation: NonZeroU32::MIN,
                location: Some(location),
            });
            self.len += 1;
        }
        self.sync_unreserved();
    }

    /// Marks every free slot as unreserved again, after `free` has changed.
    fn sync_unreserved(&mut self) {
        *self.unreserved.get_mut() =
            isize::try_from(self.free.len()).expect("free slots fit in memory");
    }

    /// Returns where `entity` is stored, or `None` if it is not alive.
    #[inline]
    pub(crate) fn get(&self, entity: Entity) -> Option<EntityLocation> {
        let meta = self.meta.get(entity.index as usize)?;
        if meta.generation == entity.generation {
            meta.location
        } else {
            None
        }
    }

    /// Returns `true` if `entity` is alive.
    #[inline]
    pub(crate) fn contains(&self, entity: Entity) -> bool {
        self.get(entity).is_some()
    }

    /// Records that the alive `entity` is now stored at `location`.
    #[inline]
    pub(crate) fn set_location(&mut self, entity: Entity, location: EntityLocation) {
        debug_assert!(self.contains(entity));
        self.meta[entity.index as usize].location = Some(location);
    }

    /// The number of alive entities.
    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.len
    }

    /// Hashes what decides the entities handed out from now on: the
    /// generation of every slot, the free slots in order, and the pending
    /// reservations. Which entities are alive is up to the archetypes.
    pub(crate) fn hash(&self, hasher: &mut ChecksumHasher) {
        // Integer slices would hash as bytes in memory, which depend on the
        // platform's byte order, so every integer is written on its own.
        hasher.write_usize(self.meta.len());
        for meta in &self.meta {
            hasher.write_u32(meta.generation.get());
        }
        hasher.write_usize(self.free.len());
        for &index in &self.free {
            hasher.write_u32(index);
        }
        hasher.write_isize(self.unreserved.load(Ordering::Relaxed));
    }
}

/// Copies the allocator, with its pending reservations, for snapshots.
///
/// Reservations being made at the same time, through shared references on
/// other threads, may or may not be included. Either way the copy is
/// consistent, since reservations only move the count of unreserved slots.
impl Clone for Entities {
    fn clone(&self) -> Self {
        Self {
            meta: self.meta.clone(),
            free: self.free.clone(),
            unreserved: AtomicIsize::new(self.unreserved.load(Ordering::Relaxed)),
            len: self.len,
        }
    }

    fn clone_from(&mut self, source: &Self) {
        self.meta.clone_from(&source.meta);
        self.free.clone_from(&source.free);
        *self.unreserved.get_mut() = source.unreserved.load(Ordering::Relaxed);
        self.len = source.len;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERE: EntityLocation = EntityLocation {
        archetype: ArchetypeId::EMPTY,
        row: 0,
    };

    #[test]
    fn option_entity_has_a_niche() {
        assert_eq!(size_of::<Entity>(), 8);
        assert_eq!(size_of::<Option<Entity>>(), 8);
    }

    #[test]
    fn bits_round_trip() {
        let entity = Entity::new(42, NonZeroU32::new(7).unwrap());
        assert_eq!(entity.to_bits(), 0x0000_0007_0000_002a);
        assert_eq!(Entity::from_bits(entity.to_bits()), Some(entity));
        assert_eq!(Entity::from_bits(42), None);
        assert_eq!(entity.to_string(), "42v7");
    }

    #[test]
    fn alloc_hands_out_distinct_entities() {
        let mut entities = Entities::default();
        let a = entities.alloc(HERE);
        let b = entities.alloc(HERE);
        assert_ne!(a, b);
        assert_eq!(a.generation(), 1);
        assert!(entities.contains(a));
        assert!(entities.contains(b));
        assert_eq!(entities.len(), 2);
    }

    #[test]
    fn reused_slot_gets_a_new_generation() {
        let mut entities = Entities::default();
        let old = entities.alloc(HERE);
        assert!(entities.free(old).is_some());
        let new = entities.alloc(HERE);

        assert_eq!(new.index(), old.index());
        assert_eq!(new.generation(), old.generation() + 1);
        assert!(!entities.contains(old));
        assert!(entities.contains(new));
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn freeing_a_dead_entity_does_nothing() {
        let mut entities = Entities::default();
        let entity = entities.alloc(HERE);
        assert!(entities.free(entity).is_some());
        assert!(entities.free(entity).is_none());
        assert_eq!(entities.len(), 0);

        let never_allocated = Entity::new(100, NonZeroU32::MIN);
        assert!(entities.free(never_allocated).is_none());
    }

    #[test]
    fn forged_handle_to_a_free_slot_is_not_alive() {
        let mut entities = Entities::default();
        let entity = entities.alloc(HERE);
        entities.free(entity);
        // This handle carries the generation the slot will have when reused.
        let forged = Entity::new(entity.index(), entity.generation.checked_add(1).unwrap());
        assert!(!entities.contains(forged));
    }

    #[test]
    fn exhausted_slot_is_retired() {
        let mut entities = Entities::default();
        let first = entities.alloc(HERE);
        entities.meta[first.index() as usize].generation = NonZeroU32::MAX;
        let last_generation = Entity::new(first.index(), NonZeroU32::MAX);

        assert!(entities.free(last_generation).is_some());
        assert!(!entities.contains(last_generation));
        assert_ne!(entities.alloc(HERE).index(), first.index());
    }

    #[test]
    fn tracks_locations() {
        let mut entities = Entities::default();
        let entity = entities.alloc(HERE);
        assert_eq!(entities.get(entity), Some(HERE));

        let there = EntityLocation { row: 5, ..HERE };
        entities.set_location(entity, there);
        assert_eq!(entities.get(entity), Some(there));
        assert_eq!(entities.free(entity), Some(there));
        assert_eq!(entities.get(entity), None);
    }

    #[test]
    fn clear_frees_every_entity() {
        let mut entities = Entities::default();
        let a = entities.alloc(HERE);
        let b = entities.alloc(HERE);
        entities.free(a);
        entities.clear();

        assert_eq!(entities.len(), 0);
        assert!(!entities.contains(b));
        // Both slots are reused, each exactly once, with new generations.
        let mut reused = [entities.alloc(HERE), entities.alloc(HERE)];
        reused.sort();
        assert_eq!(reused.map(Entity::index), [a.index(), b.index()]);
        assert!(reused.iter().all(|entity| entity.generation() == 2));
        assert_eq!(entities.alloc(HERE).index(), 2);
    }

    /// Flushes the reservations, recording each placed entity.
    fn flush(entities: &mut Entities) -> Vec<Entity> {
        let mut placed = Vec::new();
        entities.flush(|entity| {
            placed.push(entity);
            HERE
        });
        placed
    }

    #[test]
    fn reservations_reuse_free_slots_then_add_new_ones() {
        let mut entities = Entities::default();
        let [a, b, c] = [(); 3].map(|()| entities.alloc(HERE));
        entities.free(a);
        entities.free(c);

        // Free slots are taken from the end of the free list, like `alloc`.
        let reserved = [(); 3].map(|()| entities.reserve());
        assert_eq!(reserved.map(Entity::index), [c.index(), a.index(), 3]);
        assert_eq!(reserved.map(Entity::generation), [2, 2, 1]);
        assert!(reserved.iter().all(|&entity| !entities.contains(entity)));
        assert_eq!(entities.pending(), 3);

        assert_eq!(flush(&mut entities), reserved);
        assert!(reserved.iter().all(|&entity| entities.contains(entity)));
        assert!(entities.contains(b));
        assert_eq!(entities.len(), 4);
        assert_eq!(entities.pending(), 0);
        assert_eq!(flush(&mut entities), Vec::<Entity>::new());

        // Allocation carries on where the reservations left off.
        assert_eq!(entities.alloc(HERE).index(), 4);
    }

    #[test]
    fn reservations_are_deterministic() {
        let history = || {
            let mut entities = Entities::default();
            let all: Vec<_> = (0..8).map(|_| entities.alloc(HERE)).collect();
            for &entity in all.iter().step_by(3) {
                entities.free(entity);
            }
            let reserved: Vec<_> = (0..5).map(|_| entities.reserve()).collect();
            (reserved, flush(&mut entities))
        };
        assert_eq!(history(), history());
    }

    #[test]
    fn copies_hand_out_the_same_entities() {
        let mut entities = Entities::default();
        let all: Vec<_> = (0..4).map(|_| entities.alloc(HERE)).collect();
        entities.free(all[1]);
        let pending = entities.reserve();

        let mut copy = entities.clone();
        assert_eq!(copy.pending(), 1);
        assert_eq!(flush(&mut copy), [pending]);
        assert_eq!(flush(&mut entities), [pending]);
        assert_eq!(copy.alloc(HERE), entities.alloc(HERE));

        // Copying over an allocator replaces everything it knew.
        let mut other = Entities::default();
        other.alloc(HERE);
        other.reserve();
        other.clone_from(&entities);
        assert_eq!(other.pending(), 0);
        assert_eq!(other.len(), entities.len());
        assert_eq!(other.reserve(), entities.reserve());
    }

    #[test]
    fn concurrent_reservations_are_distinct() {
        let mut entities = Entities::default();
        let all: Vec<_> = (0..50).map(|_| entities.alloc(HERE)).collect();
        for &entity in &all[..25] {
            entities.free(entity);
        }

        let per_thread = if cfg!(miri) { 10 } else { 1000 };
        let mut reserved: Vec<Entity> = std::thread::scope(|scope| {
            let threads: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        (0..per_thread)
                            .map(|_| entities.reserve())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            threads
                .into_iter()
                .flat_map(|thread| thread.join().unwrap())
                .collect()
        });
        reserved.sort();
        reserved.dedup();
        assert_eq!(reserved.len(), 4 * per_thread);

        let mut placed = flush(&mut entities);
        placed.sort();
        assert_eq!(placed, reserved);
        assert_eq!(entities.len(), 25 + 4 * per_thread);
    }
}
