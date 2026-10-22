//! Tests of resources, through the public API only.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use petek::{Component, Resource, World};

#[derive(Debug, PartialEq)]
struct Score(u32);
impl Resource for Score {}

#[derive(Debug, PartialEq)]
struct Gravity(i32);
impl Resource for Gravity {}

/// Counts its drops in a shared counter.
struct Tracked(Arc<AtomicUsize>);
impl Resource for Tracked {}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn resources_are_inserted_read_and_removed() {
    let mut world = World::new();
    assert!(!world.contains_resource::<Score>());
    assert_eq!(world.resource::<Score>(), None);
    assert_eq!(world.resource_mut::<Score>(), None);
    assert_eq!(world.remove_resource::<Score>(), None);

    assert_eq!(world.insert_resource(Score(1)), None);
    world.insert_resource(Gravity(-10));
    world.resource_mut::<Score>().unwrap().0 += 1;
    assert_eq!(world.resource::<Score>(), Some(&Score(2)));
    assert_eq!(world.resource::<Gravity>(), Some(&Gravity(-10)));

    assert_eq!(world.insert_resource(Score(5)), Some(Score(2)));
    assert_eq!(world.remove_resource::<Score>(), Some(Score(5)));
    assert!(!world.contains_resource::<Score>());
    assert!(world.contains_resource::<Gravity>());

    // A removed resource can come back.
    world.insert_resource(Score(7));
    assert_eq!(world.resource::<Score>(), Some(&Score(7)));
}

#[test]
fn resources_are_separate_from_entities() {
    #[derive(Debug, PartialEq)]
    struct Both(u32);
    impl Component for Both {}
    impl Resource for Both {}

    let mut world = World::new();
    let entity = world.spawn(Both(1));
    world.insert_resource(Both(2));
    assert_eq!(world.get::<Both>(entity), Some(&Both(1)));
    assert_eq!(world.resource::<Both>(), Some(&Both(2)));

    // Clearing the entities keeps the resources.
    world.clear();
    assert!(world.is_empty());
    assert_eq!(world.resource::<Both>(), Some(&Both(2)));
}

#[test]
fn resources_are_dropped_exactly_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut world = World::new();

    world.insert_resource(Tracked(drops.clone()));
    drop(world.insert_resource(Tracked(drops.clone())));
    assert_eq!(drops.load(Ordering::Relaxed), 1);

    drop(world.remove_resource::<Tracked>());
    assert_eq!(drops.load(Ordering::Relaxed), 2);

    world.insert_resource(Tracked(drops.clone()));
    drop(world);
    assert_eq!(drops.load(Ordering::Relaxed), 3);
}
