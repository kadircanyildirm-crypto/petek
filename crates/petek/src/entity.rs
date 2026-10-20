//! Entity handles and the allocator that hands them out.

use std::fmt;
use std::num::NonZeroU32;

/// A lightweight handle to an entity.
///
/// An `Entity` is a slot index paired with a *generation*. Indices are
/// recycled when entities are despawned, but each reuse bumps the slot's
/// generation, so a stale handle to a despawned entity is detected instead of
/// silently referring to whichever entity took its slot.
///
/// The generation is never zero, which lets `Option<Entity>` stay 8 bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Entity {
    index: u32,
    generation: NonZeroU32,
}

impl Entity {
    pub(crate) const fn new(index: u32, generation: NonZeroU32) -> Self {
        Self { index, generation }
    }

    /// The slot index of this entity. Indices are reused after despawning.
    pub const fn index(self) -> u32 {
        self.index
    }

    /// How many times this entity's slot has been reused, starting at 1.
    pub const fn generation(self) -> u32 {
        self.generation.get()
    }

    /// Packs this entity into a `u64`, e.g. for serialization or networking.
    ///
    /// The generation goes in the high 32 bits and the index in the low 32.
    pub const fn to_bits(self) -> u64 {
        ((self.generation.get() as u64) << 32) | self.index as u64
    }

    /// Unpacks an entity produced by [`to_bits`](Self::to_bits).
    ///
    /// Returns `None` if `bits` has a zero generation, which no entity can
    /// have.
    pub const fn from_bits(bits: u64) -> Option<Self> {
        match NonZeroU32::new((bits >> 32) as u32) {
            Some(generation) => Some(Self::new(bits as u32, generation)),
            None => None,
        }
    }
}

impl fmt::Debug for Entity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.index, self.generation)
    }
}

/// Allocates entity handles and tracks which of them are alive.
#[derive(Default)]
pub(crate) struct Entities {
    meta: Vec<EntityMeta>,
    /// Indices of free slots, ready to be reused.
    free: Vec<u32>,
    /// Number of alive entities.
    len: u32,
}

#[derive(Clone, Copy)]
struct EntityMeta {
    /// The generation of the entity that lives, or will next live, in this
    /// slot.
    generation: NonZeroU32,
    alive: bool,
}

impl Entities {
    /// Allocates a new entity, reusing a free slot if there is one.
    pub(crate) fn alloc(&mut self) -> Entity {
        let entity = match self.free.pop() {
            Some(index) => {
                let meta = &mut self.meta[index as usize];
                meta.alive = true;
                Entity::new(index, meta.generation)
            }
            None => {
                let index = u32::try_from(self.meta.len()).expect("too many entities");
                self.meta.push(EntityMeta {
                    generation: NonZeroU32::MIN,
                    alive: true,
                });
                Entity::new(index, NonZeroU32::MIN)
            }
        };
        self.len += 1;
        entity
    }

    /// Frees `entity` so its slot can be reused.
    ///
    /// Returns `false`, and does nothing, if `entity` was not alive.
    pub(crate) fn free(&mut self, entity: Entity) -> bool {
        if !self.contains(entity) {
            return false;
        }
        let meta = &mut self.meta[entity.index as usize];
        meta.alive = false;
        self.len -= 1;
        // A slot whose generation would overflow is retired for good rather
        // than wrapped around, so a stale handle can never become valid again.
        if let Some(next) = meta.generation.checked_add(1) {
            meta.generation = next;
            self.free.push(entity.index);
        }
        true
    }

    /// Returns `true` if `entity` is alive.
    pub(crate) fn contains(&self, entity: Entity) -> bool {
        self.meta
            .get(entity.index as usize)
            .is_some_and(|meta| meta.alive && meta.generation == entity.generation)
    }

    /// The number of alive entities.
    pub(crate) fn len(&self) -> u32 {
        self.len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn option_entity_has_a_niche() {
        assert_eq!(size_of::<Entity>(), 8);
        assert_eq!(size_of::<Option<Entity>>(), 8);
    }

    #[test]
    fn bits_round_trip() {
        let entity = Entity::new(42, NonZeroU32::new(7).unwrap());
        assert_eq!(entity.to_bits(), (7 << 32) | 42);
        assert_eq!(Entity::from_bits(entity.to_bits()), Some(entity));
        assert_eq!(Entity::from_bits(42), None);
    }

    #[test]
    fn alloc_hands_out_distinct_entities() {
        let mut entities = Entities::default();
        let a = entities.alloc();
        let b = entities.alloc();
        assert_ne!(a, b);
        assert!(entities.contains(a));
        assert!(entities.contains(b));
        assert_eq!(entities.len(), 2);
    }

    #[test]
    fn reused_slot_gets_a_new_generation() {
        let mut entities = Entities::default();
        let old = entities.alloc();
        assert!(entities.free(old));
        let new = entities.alloc();

        assert_eq!(new.index(), old.index());
        assert_eq!(new.generation(), old.generation() + 1);
        assert!(!entities.contains(old));
        assert!(entities.contains(new));
        assert_eq!(entities.len(), 1);
    }

    #[test]
    fn freeing_a_dead_entity_does_nothing() {
        let mut entities = Entities::default();
        let entity = entities.alloc();
        assert!(entities.free(entity));
        assert!(!entities.free(entity));
        assert_eq!(entities.len(), 0);

        let never_allocated = Entity::new(100, NonZeroU32::MIN);
        assert!(!entities.free(never_allocated));
    }

    #[test]
    fn forged_handle_to_a_free_slot_is_not_alive() {
        let mut entities = Entities::default();
        let entity = entities.alloc();
        entities.free(entity);
        // This handle carries the generation the slot will have when reused.
        let forged = Entity::new(entity.index(), entity.generation.checked_add(1).unwrap());
        assert!(!entities.contains(forged));
    }

    #[test]
    fn exhausted_slot_is_retired() {
        let mut entities = Entities::default();
        let first = entities.alloc();
        entities.meta[first.index() as usize].generation = NonZeroU32::MAX;
        let last_generation = Entity::new(first.index(), NonZeroU32::MAX);

        assert!(entities.free(last_generation));
        assert!(!entities.contains(last_generation));
        assert_ne!(entities.alloc().index(), first.index());
    }
}
