//! Snapshots: copies of a world that it can be brought back to.

use std::fmt;

use crate::archetype::ArchetypeSnapshot;
use crate::change::Tick;
use crate::entity::Entities;
use crate::resource::Stored;
use crate::world::WorldId;

/// A copy of a world, taken by [`World::snapshot`](crate::World::snapshot),
/// that the world can be brought back to with
/// [`World::restore`](crate::World::restore).
///
/// It holds clones of every component and resource of the world, so its
/// memory use is about that of the world. A snapshot belongs to the world it
/// was taken of, and restoring it into another world panics.
pub struct Snapshot {
    pub(crate) world: WorldId,
    pub(crate) entities: Entities,
    pub(crate) archetypes: Box<[ArchetypeSnapshot]>,
    pub(crate) resources: Box<[Option<Stored>]>,
    pub(crate) change_tick: Tick,
    pub(crate) system_ticks: Box<[Tick]>,
}

impl Snapshot {
    /// The world's tick when the snapshot was taken.
    #[must_use]
    pub fn change_tick(&self) -> Tick {
        self.change_tick
    }
}

impl fmt::Debug for Snapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Snapshot")
            .field("entities", &self.entities.len())
            .field("archetypes", &self.archetypes.len())
            .field("change_tick", &self.change_tick)
            .finish_non_exhaustive()
    }
}
