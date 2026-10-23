//! What systems access, so that systems that would race or alias are never
//! run at the same time.

use std::cmp::Ordering;

use crate::component::ComponentId;
use crate::query::{Access, Presence};
use crate::world::World;

/// Everything a system, or one of its parameters, reads and writes.
///
/// This is only `pub` because the hidden methods of
/// [`SystemParam`](super::SystemParam) mention it.
#[derive(Clone, Default, Debug)]
pub struct SystemAccess {
    /// What each query accesses. Queries are kept apart, since two that
    /// never match the same archetype never access the same rows.
    queries: Vec<QueryAccess>,
    /// Sorted by slot, each listed once with the strongest access.
    resources: Vec<(usize, Access)>,
    /// Whether it reserves entities. Two systems that do must not run at the
    /// same time, because which gets which entity would then depend on
    /// timing.
    reserves_entities: bool,
    /// Whether it reads the whole world.
    reads_world: bool,
}

/// What one query accesses, and what the archetypes it matches have.
#[derive(Clone, Default, Debug)]
pub(crate) struct QueryAccess {
    /// Sorted by id, each listed once with the strongest access.
    components: Vec<(ComponentId, Access)>,
    /// Components every archetype the query matches has, sorted.
    required: Vec<ComponentId>,
    /// Components no archetype the query matches has, sorted.
    excluded: Vec<ComponentId>,
}

impl QueryAccess {
    pub(crate) fn add_component(&mut self, id: ComponentId, access: Access) {
        add(&mut self.components, id, access);
    }

    pub(crate) fn add_presence(&mut self, id: ComponentId, presence: Presence) {
        let list = match presence {
            Presence::Required => &mut self.required,
            Presence::Excluded => &mut self.excluded,
        };
        if let Err(index) = list.binary_search(&id) {
            list.insert(index, id);
        }
    }

    fn writes(&self) -> bool {
        writes(&self.components)
    }

    /// Returns `true` if no archetype can match both queries, because one
    /// requires a component the other excludes.
    fn is_disjoint(&self, other: &Self) -> bool {
        shares(&self.required, &other.excluded) || shares(&self.excluded, &other.required)
    }

    /// Returns a component both queries access, at least one of them by
    /// writing it, on entities both may match.
    fn conflict(&self, other: &Self) -> Option<ComponentId> {
        if self.is_disjoint(other) {
            return None;
        }
        find_conflict(&self.components, &other.components)
    }
}

impl SystemAccess {
    pub(crate) fn add_query(&mut self, query: QueryAccess) {
        self.queries.push(query);
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

    /// Returns `true` if it reads the whole world.
    pub(crate) fn reads_world(&self) -> bool {
        self.reads_world
    }

    /// Returns `true` if it writes anything.
    fn writes(&self) -> bool {
        self.queries.iter().any(QueryAccess::writes) || writes(&self.resources)
    }

    /// Returns a component that a query of each accesses, at least one of
    /// them by writing it, on entities both queries may match.
    fn component_conflict(&self, other: &Self) -> Option<ComponentId> {
        self.queries
            .iter()
            .find_map(|query| other.queries.iter().find_map(|other| query.conflict(other)))
    }

    /// Returns `true` if a system with this access must not run at the same
    /// time as one with `other`'s, because one could write what the other
    /// accesses, or both could reserve entities.
    pub(crate) fn conflicts_with(&self, other: &Self) -> bool {
        (self.reserves_entities && other.reserves_entities)
            || (self.reads_world && other.writes())
            || (other.reads_world && self.writes())
            || self.component_conflict(other).is_some()
            || find_conflict(&self.resources, &other.resources).is_some()
    }

    /// Describes how a parameter with `other`'s access conflicts with
    /// parameters with this access, if it does, by naming what both access.
    ///
    /// Unlike [`conflicts_with`](Self::conflicts_with), this lets both
    /// reserve entities, since the parameters of one system are used by one
    /// thread, in a fixed order.
    fn describe_conflict(&self, other: &Self, world: &World) -> Option<String> {
        if let Some(id) = self.component_conflict(other) {
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
        self.queries.extend(other.queries.iter().cloned());
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

/// Returns `true` if a list of accesses writes anything.
fn writes<K>(list: &[(K, Access)]) -> bool {
    list.iter().any(|(_, access)| *access == Access::Write)
}

/// Returns `true` if two sorted lists have an element in common.
fn shares(a: &[ComponentId], b: &[ComponentId]) -> bool {
    a.iter().any(|id| b.binary_search(id).is_ok())
}

/// Returns something both sorted lists access, at least one of them by
/// writing it.
fn find_conflict<K: Ord + Copy>(a: &[(K, Access)], b: &[(K, Access)]) -> Option<K> {
    let (mut a, mut b) = (a.iter().peekable(), b.iter().peekable());
    while let (Some(&&(key_a, access_a)), Some(&&(key_b, access_b))) = (a.peek(), b.peek()) {
        match key_a.cmp(&key_b) {
            Ordering::Less => {
                a.next();
            }
            Ordering::Greater => {
                b.next();
            }
            Ordering::Equal => {
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

    /// The access of a system with a single query.
    fn query(f: impl FnOnce(&mut QueryAccess)) -> SystemAccess {
        let mut query = QueryAccess::default();
        f(&mut query);
        access(|access| access.add_query(query))
    }

    #[test]
    fn writes_conflict_with_any_other_access() {
        let mut world = World::new();
        let (a, b, score) = ids(&mut world);
        let read_a = query(|x| x.add_component(a, Access::Read));
        let write_a = query(|x| x.add_component(a, Access::Write));
        let write_b = query(|x| x.add_component(b, Access::Write));
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
        let both = query(|x| {
            x.add_component(a, Access::Write);
            x.add_component(a, Access::Read);
        });
        let read_a = query(|x| x.add_component(a, Access::Read));
        assert!(both.conflicts_with(&read_a));
    }

    #[test]
    fn queries_that_match_no_archetype_in_common_do_not_conflict() {
        let mut world = World::new();
        let (a, b, _) = ids(&mut world);
        let write_a = |presence: Option<Presence>| {
            query(|x| {
                x.add_component(a, Access::Write);
                x.add_presence(a, Presence::Required);
                if let Some(presence) = presence {
                    x.add_presence(b, presence);
                }
            })
        };
        let with_b = write_a(Some(Presence::Required));
        let without_b = write_a(Some(Presence::Excluded));
        let any = write_a(None);

        assert!(!with_b.conflicts_with(&without_b));
        assert!(!without_b.conflicts_with(&with_b));
        assert!(with_b.conflicts_with(&any) && without_b.conflicts_with(&any));
        assert!(with_b.conflicts_with(&with_b));

        // A system conflicts if any of its queries does.
        let both = access(|x| x.extend(&with_b));
        let both = access(|x| {
            x.extend(&both);
            x.extend(&any);
        });
        assert!(both.conflicts_with(&without_b));
    }

    #[test]
    fn reading_the_world_conflicts_with_writes_and_reservations() {
        let mut world = World::new();
        let (a, _, score) = ids(&mut world);
        let world_access = access(|x| {
            x.read_world();
            x.reserve_entities();
        });
        let read_a = query(|x| x.add_component(a, Access::Read));
        let write_a = query(|x| x.add_component(a, Access::Write));
        let write_score = access(|x| x.add_resource(score, Access::Write));
        let commands = access(SystemAccess::reserve_entities);

        assert!(!world_access.conflicts_with(&read_a));
        assert!(world_access.conflicts_with(&write_a));
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
