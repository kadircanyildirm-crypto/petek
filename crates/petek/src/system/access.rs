//! What systems access, so that systems that would race or alias are never
//! run at the same time.

use crate::component::ComponentId;
use crate::query::Access;
use crate::world::World;

/// Everything a system, or one of its parameters, reads and writes.
///
/// This is only `pub` because the hidden methods of
/// [`SystemParam`](super::SystemParam) mention it.
#[derive(Clone, Default, Debug)]
pub struct SystemAccess {
    /// Sorted by id, each listed once with the strongest access.
    components: Vec<(ComponentId, Access)>,
    /// Sorted by slot, each listed once with the strongest access.
    resources: Vec<(usize, Access)>,
    /// Whether it reserves entities. Two systems that do must not run at the
    /// same time, because which gets which entity would then depend on
    /// timing.
    reserves_entities: bool,
    /// Whether it reads the whole world.
    reads_world: bool,
}

impl SystemAccess {
    pub(crate) fn add_component(&mut self, id: ComponentId, access: Access) {
        add(&mut self.components, id, access);
    }

    pub(crate) fn add_resource(&mut self, slot: usize, access: Access) {
        add(&mut self.resources, slot, access);
    }

    pub(crate) fn reserve_entities(&mut self) {
        self.reserves_entities = true;
    }

    pub(crate) fn read_world(&mut self) {
        self.reads_world = true;
    }

    /// Returns `true` if it writes anything.
    fn writes(&self) -> bool {
        fn writes<K>(list: &[(K, Access)]) -> bool {
            list.iter().any(|(_, access)| *access == Access::Write)
        }
        writes(&self.components) || writes(&self.resources)
    }

    /// Returns `true` if a system with this access must not run at the same
    /// time as one with `other`'s, because one could write what the other
    /// accesses, or both could reserve entities.
    pub(crate) fn conflicts_with(&self, other: &Self) -> bool {
        (self.reserves_entities && other.reserves_entities)
            || (self.reads_world && other.writes())
            || (other.reads_world && self.writes())
            || find_conflict(&self.components, &other.components).is_some()
            || find_conflict(&self.resources, &other.resources).is_some()
    }

    /// Describes how a parameter with `other`'s access conflicts with
    /// parameters with this access, if it does, by naming what both access.
    ///
    /// Unlike [`conflicts_with`](Self::conflicts_with), this lets both
    /// reserve entities, since the parameters of one system are used by one
    /// thread, in a fixed order.
    fn describe_conflict(&self, other: &Self, world: &World) -> Option<String> {
        if let Some(id) = find_conflict(&self.components, &other.components) {
            return Some(format!("`{}`", world.components().info(id).name()));
        }
        if let Some(slot) = find_conflict(&self.resources, &other.resources) {
            return Some(format!("the `{}` resource", world.resources().name(slot)));
        }
        let world_with_writes =
            (self.reads_world && other.writes()) || (other.reads_world && self.writes());
        world_with_writes.then(|| "the world".to_owned())
    }

    /// Adds everything `other` accesses.
    fn extend(&mut self, other: &Self) {
        for &(id, access) in &other.components {
            self.add_component(id, access);
        }
        for &(slot, access) in &other.resources {
            self.add_resource(slot, access);
        }
        self.reserves_entities |= other.reserves_entities;
        self.reads_world |= other.reads_world;
    }
}

/// Adds `key` to a sorted list of accesses, keeping the stronger access if it
/// is already there.
fn add<K: Ord + Copy>(list: &mut Vec<(K, Access)>, key: K, access: Access) {
    match list.binary_search_by_key(&key, |&(listed, _)| listed) {
        Ok(index) if access == Access::Write => list[index].1 = Access::Write,
        Ok(_) => {}
        Err(index) => list.insert(index, (key, access)),
    }
}

/// Returns something both sorted lists access, at least one of them by
/// writing it.
fn find_conflict<K: Ord + Copy>(a: &[(K, Access)], b: &[(K, Access)]) -> Option<K> {
    let (mut a, mut b) = (a.iter().peekable(), b.iter().peekable());
    while let (Some(&&(key_a, access_a)), Some(&&(key_b, access_b))) = (a.peek(), b.peek()) {
        match key_a.cmp(&key_b) {
            std::cmp::Ordering::Less => {
                a.next();
            }
            std::cmp::Ordering::Greater => {
                b.next();
            }
            std::cmp::Ordering::Equal => {
                if access_a == Access::Write || access_b == Access::Write {
                    return Some(key_a);
                }
                a.next();
                b.next();
            }
        }
    }
    None
}

/// What a system's parameters access, collected as they are initialized.
///
/// This is only `pub` because the hidden methods of
/// [`SystemParam`](super::SystemParam) mention it.
#[derive(Debug)]
pub struct SystemMeta {
    name: &'static str,
    access: SystemAccess,
}

impl SystemMeta {
    pub(crate) fn new(name: &'static str) -> Self {
        Self {
            name,
            access: SystemAccess::default(),
        }
    }

    #[inline]
    pub(crate) fn name(&self) -> &'static str {
        self.name
    }

    pub(crate) fn access(&self) -> &SystemAccess {
        &self.access
    }

    /// Records what one more parameter of the system accesses.
    ///
    /// # Panics
    ///
    /// If the parameter conflicts with an earlier one: if one of them writes
    /// what the other accesses, using both at once could alias a mutable
    /// reference.
    #[track_caller]
    pub(crate) fn add_param(&mut self, access: &SystemAccess, world: &World) {
        if let Some(what) = self.access.describe_conflict(access, world) {
            panic!(
                "system `{}` has parameters that both access {what}, and at least one writes it",
                self.name,
            );
        }
        self.access.extend(access);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component::{Component, ComponentInfo};
    use crate::resource::Resource;

    struct A;
    impl Component for A {}

    struct B;
    impl Component for B {}

    struct Score;
    impl Resource for Score {}

    fn ids(world: &mut World) -> (ComponentId, ComponentId, usize) {
        let components = world.components_mut();
        let a = components.register(ComponentInfo::of::<A>());
        let b = components.register(ComponentInfo::of::<B>());
        (a, b, world.resources_mut().slot_or_insert::<Score>())
    }

    fn access(f: impl FnOnce(&mut SystemAccess)) -> SystemAccess {
        let mut access = SystemAccess::default();
        f(&mut access);
        access
    }

    #[test]
    fn writes_conflict_with_any_other_access() {
        let mut world = World::new();
        let (a, b, score) = ids(&mut world);
        let read_a = access(|x| x.add_component(a, Access::Read));
        let write_a = access(|x| x.add_component(a, Access::Write));
        let write_b = access(|x| x.add_component(b, Access::Write));
        let read_score = access(|x| x.add_resource(score, Access::Read));
        let write_score = access(|x| x.add_resource(score, Access::Write));

        assert!(!read_a.conflicts_with(&read_a));
        assert!(read_a.conflicts_with(&write_a) && write_a.conflicts_with(&read_a));
        assert!(write_a.conflicts_with(&write_a));
        assert!(!write_a.conflicts_with(&write_b));
        assert!(!read_score.conflicts_with(&read_score));
        assert!(read_score.conflicts_with(&write_score));
        assert!(!write_score.conflicts_with(&write_a));
    }

    #[test]
    fn the_strongest_access_is_kept() {
        let mut world = World::new();
        let (a, _, _) = ids(&mut world);
        let both = access(|x| {
            x.add_component(a, Access::Write);
            x.add_component(a, Access::Read);
        });
        let read_a = access(|x| x.add_component(a, Access::Read));
        assert!(both.conflicts_with(&read_a));
    }

    #[test]
    fn reading_the_world_conflicts_with_writes_and_reservations() {
        let mut world = World::new();
        let (a, _, score) = ids(&mut world);
        let world_access = access(|x| {
            x.read_world();
            x.reserve_entities();
        });
        let read_a = access(|x| x.add_component(a, Access::Read));
        let write_score = access(|x| x.add_resource(score, Access::Write));
        let commands = access(SystemAccess::reserve_entities);

        assert!(!world_access.conflicts_with(&read_a));
        assert!(world_access.conflicts_with(&write_score));
        assert!(write_score.conflicts_with(&world_access));
        assert!(world_access.conflicts_with(&commands));
        assert!(commands.conflicts_with(&commands));
    }

    #[test]
    #[should_panic(expected = "system `test` has parameters that both access the `")]
    fn conflicting_parameters_are_rejected() {
        let mut world = World::new();
        let (_, _, score) = ids(&mut world);
        let mut meta = SystemMeta::new("test");
        meta.add_param(&access(|x| x.add_resource(score, Access::Read)), &world);
        meta.add_param(&access(|x| x.add_resource(score, Access::Write)), &world);
    }

    #[test]
    fn parameters_of_one_system_may_all_reserve_entities() {
        let world = World::new();
        let mut meta = SystemMeta::new("test");
        meta.add_param(&access(SystemAccess::reserve_entities), &world);
        meta.add_param(&access(SystemAccess::reserve_entities), &world);
        assert!(meta.access().reserves_entities);
    }
}
