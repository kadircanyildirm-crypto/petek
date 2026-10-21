//! Entity handles and the allocator that hands them out.

use std::fmt;
use std::num::NonZeroU32;

use crate::archetype::ArchetypeId;

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
#[derive(Default)]
pub(crate) struct Entities {
    meta: Vec<EntityMeta>,
    /// Indices of free slots, ready to be reused.
    free: Vec<u32>,
    /// Number of alive entities.
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
    pub(crate) fn alloc(&mut self, location: EntityLocation) -> Entity {
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
        entity
    }

    /// Frees `entity` so its slot can be reused, returning where it was
    /// stored.
    ///
    /// Returns `None`, and does nothing, if `entity` was not alive.
    pub(crate) fn free(&mut self, entity: Entity) -> Option<EntityLocation> {
        let location = self.get(entity)?;
        if self.meta[entity.index as usize].release() {
            self.free.push(entity.index);
        }
        self.len -= 1;
        Some(location)
    }

    /// Frees every alive entity.
    pub(crate) fn clear(&mut self) {
        for (index, meta) in (0..).zip(&mut self.meta) {
            if meta.location.is_some() && meta.release() {
                self.free.push(index);
            }
        }
        self.len = 0;
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
}
