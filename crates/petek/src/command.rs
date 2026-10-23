//! Commands: changes to a world that are recorded now and applied later.

use std::fmt;

use crate::bundle::Bundle;
use crate::entity::{Entities, Entity};
use crate::resource::Resource;
use crate::world::{World, WorldId};

/// A recorded change to a world.
type Command = Box<dyn FnOnce(&mut World) + Send>;

/// A list of changes to apply to a world later, in the order they were
/// recorded.
///
/// Structural changes, such as spawning entities or adding components, need
/// exclusive access to the world, so they cannot be made while a query is
/// running. [`Commands`] record them instead, to be applied once it is done.
/// They come from [`CommandQueue::commands`] alongside read-only queries, or
/// from [`Query::commands`](crate::query::Query::commands) alongside any
/// query:
///
/// ```
/// use petek::command::CommandQueue;
/// use petek::{Component, Entity, World};
///
/// struct Health(u32);
/// impl Component for Health {}
///
/// struct Corpse;
/// impl Component for Corpse {}
///
/// let mut world = World::new();
/// let alive = world.spawn(Health(10));
/// let dead = world.spawn(Health(0));
///
/// let mut queue = CommandQueue::new();
/// let query = world.query_mut::<(Entity, &mut Health)>();
/// let mut commands = query.commands(&mut queue);
/// for (entity, mut health) in query {
///     health.0 = health.0.saturating_sub(5);
///     if health.0 == 0 {
///         commands.despawn(entity);
///         commands.spawn(Corpse);
///     }
/// }
/// queue.apply(&mut world);
///
/// assert!(world.contains(alive) && !world.contains(dead));
/// assert_eq!(world.query::<&Corpse>().iter().count(), 1);
/// ```
///
/// Applying the same commands to the same world always has the same result,
/// down to the entity handles that are handed out, as long as the entities
/// are reserved in the same order. Reserving entities from several threads at
/// once is safe, but which thread gets which entity then depends on timing.
#[derive(Default)]
pub struct CommandQueue {
    commands: Vec<Command>,
    /// The world that [`Commands`] reserved entities in, if any.
    world: Option<WorldId>,
}

impl CommandQueue {
    /// Creates an empty queue.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns a [`Commands`] that records into this queue, and reserves the
    /// entities it spawns in `world`.
    ///
    /// This borrows the world, so it cannot be used alongside a query that
    /// writes; see [`Query::commands`](crate::query::Query::commands) for
    /// that.
    ///
    /// # Panics
    ///
    /// If the queue holds commands recorded for another world.
    #[track_caller]
    pub fn commands<'a>(&'a mut self, world: &'a World) -> Commands<'a> {
        self.check_world(world);
        self.world = Some(world.id());
        Commands {
            queue: self,
            entities: world.entities(),
        }
    }

    /// Records a command: a function to call with the world when the queue is
    /// applied.
    pub fn push(&mut self, command: impl FnOnce(&mut World) + Send + 'static) {
        self.commands.push(Box::new(command));
    }

    /// The number of recorded commands.
    #[must_use]
    pub fn len(&self) -> usize {
        self.commands.len()
    }

    /// Returns `true` if no commands are recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.commands.is_empty()
    }

    /// Drops the recorded commands without applying them.
    ///
    /// Entities that [`Commands::spawn`] reserved still become alive, without
    /// components, at the world's next structural change, unless the world
    /// is [restored](World::restore) to a snapshot from before.
    pub fn clear(&mut self) {
        self.commands.clear();
        self.world = None;
    }

    /// Applies the recorded commands to `world`, in the order they were
    /// recorded, and empties the queue.
    ///
    /// Entities reserved by [`Commands::spawn`] become alive first, even if
    /// the queue is empty.
    ///
    /// If a command panics, the commands after it are dropped without being
    /// applied, and the queue is left empty.
    ///
    /// # Panics
    ///
    /// If the queue holds commands recorded for another world, since the
    /// entities they reserved belong to that world.
    #[track_caller]
    pub fn apply(&mut self, world: &mut World) {
        self.check_world(world);
        self.world = None;
        world.flush();
        for command in self.commands.drain(..) {
            command(world);
        }
    }

    /// Panics if the queue holds commands recorded for a world other than
    /// `world`: they may refer to entities reserved in that world.
    #[track_caller]
    fn check_world(&self, world: &World) {
        assert!(
            self.is_empty() || self.world.is_none_or(|id| id == world.id()),
            "a `CommandQueue` was used with a world other than the one its commands were recorded for",
        );
    }
}

impl fmt::Debug for CommandQueue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandQueue")
            .field("len", &self.len())
            .finish()
    }
}

/// Records changes to a world into a [`CommandQueue`], to apply later.
///
/// Commands that target an entity which no longer exists when they are
/// applied do nothing, and so does removing components an entity lacks.
pub struct Commands<'a> {
    queue: &'a mut CommandQueue,
    /// Used only to reserve entities. Queries never write the entity
    /// allocator, which is why commands can be used alongside any query.
    entities: &'a Entities,
}

impl Commands<'_> {
    /// Records spawning an entity with the components in `bundle`, and
    /// returns that entity.
    ///
    /// The entity is reserved right away, so it can be used in later
    /// commands. It becomes alive, without components, at the world's next
    /// structural change, and gets its components when the queue is applied.
    pub fn spawn<B: Bundle + Send>(&mut self, bundle: B) -> Entity {
        let entity = self.spawn_empty();
        self.insert(entity, bundle);
        entity
    }

    /// Records spawning an entity without components, and returns that
    /// entity.
    ///
    /// The entity is reserved right away, so it can be used in later
    /// commands, but it only becomes alive when the queue is applied, or when
    /// the world next changes structurally. If the queue is dropped instead,
    /// it becomes alive without components all the same.
    pub fn spawn_empty(&mut self) -> Entity {
        self.entities.reserve()
    }

    /// Records adding the components in `bundle` to `entity`, overwriting
    /// those it already has.
    pub fn insert<B: Bundle + Send>(&mut self, entity: Entity, bundle: B) {
        self.queue.push(move |world| {
            let _ = world.insert(entity, bundle);
        });
    }

    /// Records removing the components in `B` from `entity`, if it has all of
    /// them, and dropping them.
    pub fn remove<B: Bundle>(&mut self, entity: Entity) {
        self.queue.push(move |world| {
            let _ = world.remove::<B>(entity);
        });
    }

    /// Records despawning `entity`.
    pub fn despawn(&mut self, entity: Entity) {
        self.queue.push(move |world| {
            let _ = world.despawn(entity);
        });
    }

    /// Records inserting a resource, replacing any of the same type.
    pub fn insert_resource<R: Resource>(&mut self, resource: R) {
        self.queue.push(move |world| {
            world.insert_resource(resource);
        });
    }

    /// Records removing the resource of type `R`, if there is one.
    pub fn remove_resource<R: Resource>(&mut self) {
        self.queue.push(|world| {
            world.remove_resource::<R>();
        });
    }

    /// Records a custom command: a function to call with the world when the
    /// queue is applied.
    pub fn add(&mut self, command: impl FnOnce(&mut World) + Send + 'static) {
        self.queue.push(command);
    }
}

impl fmt::Debug for Commands<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Commands")
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}
