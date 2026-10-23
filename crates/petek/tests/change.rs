//! Tests of change detection, through the public API only.

use petek::query::{Added, Changed, Or, QueryError, QueryState, With};
use petek::{Component, Entity, World};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

/// The entities a state sees on its next run, sorted.
fn seen<F: petek::query::QueryFilter>(
    state: &mut QueryState<Entity, F>,
    world: &World,
) -> Vec<Entity> {
    let mut entities: Vec<_> = state.iter(world).collect();
    entities.sort_unstable();
    entities
}

#[test]
fn added_sees_new_components_once() {
    let mut world = World::new();
    let a = world.spawn(Pos(0));
    let mut added = QueryState::<Entity, Added<Pos>>::new(&mut world);

    // A new state has never run, so everything counts as added.
    assert_eq!(seen(&mut added, &world), [a]);
    assert_eq!(seen(&mut added, &world), []);

    let b = world.spawn(Pos(1));
    let c = world.spawn(Vel(2));
    world.insert(c, Pos(2)).unwrap();
    // Overwriting a component changes it, but does not add it, whether the
    // entity stays in its archetype or moves to another.
    world.insert(a, Pos(3)).unwrap();
    world.insert(a, (Pos(4), Vel(4))).unwrap();
    assert_eq!(seen(&mut added, &world), [b, c]);
    assert_eq!(seen(&mut added, &world), []);
}

#[test]
fn changed_sees_writes_but_not_reads() {
    let mut world = World::new();
    let [a, b, c] = [0, 1, 2].map(|i| world.spawn(Pos(i)));
    let mut changed = QueryState::<Entity, Changed<Pos>>::new(&mut world);
    assert_eq!(seen(&mut changed, &world).len(), 3);

    for (entity, mut pos) in world.query_mut::<(Entity, &mut Pos)>() {
        if entity == a {
            pos.0 += 10;
        } else {
            assert!(pos.0 < 10); // a read through `Mut` is not a write
        }
    }
    world.get_mut::<Pos>(b).unwrap().0 += 10;
    let _ = world.get_mut::<Pos>(c).unwrap().0;
    assert_eq!(seen(&mut changed, &world), [a, b]);
    assert_eq!(seen(&mut changed, &world), []);

    *world.get_mut::<Pos>(c).unwrap().into_inner() = Pos(5);
    world.insert(a, Pos(6)).unwrap();
    assert_eq!(seen(&mut changed, &world), [a, c]);
}

#[test]
fn a_query_does_not_see_its_own_writes() {
    let mut world = World::new();
    world.spawn_batch((0..3).map(Pos));
    let mut state = QueryState::<&mut Pos, Changed<Pos>>::new(&mut world);

    for mut pos in state.iter_mut(&mut world) {
        pos.0 += 1;
    }
    assert_eq!(state.iter_mut(&mut world).count(), 0);

    // Someone else's write is seen.
    let mut other = QueryState::<&mut Pos>::new(&mut world);
    other
        .iter_mut(&mut world)
        .take(1)
        .for_each(|mut pos| pos.0 += 1);
    assert_eq!(state.iter_mut(&mut world).count(), 1);
}

#[test]
fn mut_knows_what_happened_since_the_last_run() {
    let mut world = World::new();
    let old = world.spawn(Pos(0));
    let changed = world.spawn(Pos(1));
    let mut state = QueryState::<(Entity, &mut Pos)>::new(&mut world);
    let _ = state.iter_mut(&mut world).count();

    let new = world.spawn(Pos(2));
    world.get_mut::<Pos>(changed).unwrap().0 = 3;
    for (entity, mut pos) in state.iter_mut(&mut world) {
        assert_eq!(pos.is_added(), entity == new, "{entity}");
        assert_eq!(pos.is_changed(), entity != old, "{entity}");
        // Writing through it counts.
        pos.0 += 1;
        assert!(pos.is_changed());
    }

    // To `World::get_mut`, as to any one-off query, everything is new.
    let pos = world.get_mut::<Pos>(old).unwrap();
    assert!(pos.is_added() && pos.is_changed());
}

#[test]
fn ticks_follow_components_between_archetypes() {
    let mut world = World::new();
    let a = world.spawn(Pos(0));
    let b = world.spawn(Pos(1));
    let mut changed = QueryState::<Entity, Changed<Pos>>::new(&mut world);
    let mut added_vel = QueryState::<Entity, Added<Vel>>::new(&mut world);
    seen(&mut changed, &world);

    world.get_mut::<Pos>(a).unwrap().0 = 10;
    // Both move to another archetype, carrying their ticks along.
    world.insert(a, Vel(0)).unwrap();
    world.insert(b, Vel(1)).unwrap();
    assert_eq!(seen(&mut changed, &world), [a]);
    assert_eq!(seen(&mut added_vel, &world), [a, b]);

    world.remove::<Vel>(a).unwrap();
    world.get_mut::<Pos>(b).unwrap().0 = 11;
    world.remove::<Vel>(b).unwrap();
    assert_eq!(seen(&mut changed, &world), [b]);
}

#[test]
fn reused_rows_and_slots_start_fresh() {
    let mut world = World::new();
    let old = world.spawn(Pos(0));
    let mut added = QueryState::<Entity, Added<Pos>>::new(&mut world);
    let mut changed = QueryState::<Entity, Changed<Pos>>::new(&mut world);
    seen(&mut added, &world);
    seen(&mut changed, &world);

    world.despawn(old).unwrap();
    let new = world.spawn(Pos(1));
    assert_eq!(new.index(), old.index());
    assert_eq!(seen(&mut added, &world), [new]);
    assert_eq!(seen(&mut changed, &world), [new]);
}

#[test]
fn change_filters_combine() {
    let mut world = World::new();
    let a = world.spawn((Pos(0), Vel(0)));
    let b = world.spawn((Pos(1), Vel(1)));
    let c = world.spawn(Pos(2));
    let mut either = QueryState::<Entity, Or<(Changed<Pos>, Changed<Vel>)>>::new(&mut world);
    let mut both = QueryState::<Entity, (Changed<Pos>, With<Vel>)>::new(&mut world);
    seen(&mut either, &world);
    seen(&mut both, &world);

    world.get_mut::<Vel>(a).unwrap().0 = 1;
    world.get_mut::<Pos>(c).unwrap().0 = 3;
    world.get_mut::<Pos>(b).unwrap().0 = 4;
    assert_eq!(seen(&mut either, &world), [a, b, c]);
    assert_eq!(seen(&mut both, &world), [b]);
}

#[test]
fn counts_are_only_bounds_with_change_filters() {
    let mut world = World::new();
    let entities: Vec<_> = world.spawn_batch((0..10).map(Pos)).collect();
    let mut changed = QueryState::<&Pos, Changed<Pos>>::new(&mut world);
    let _ = changed.iter(&world).count();

    for &entity in &entities[..3] {
        world.get_mut::<Pos>(entity).unwrap().0 += 1;
    }
    let iter = changed.iter(&world);
    assert_eq!(iter.size_hint(), (0, Some(10)));
    assert_eq!(iter.count(), 3);
}

#[test]
fn single_entities_are_checked_against_change_filters() {
    let mut world = World::new();
    let [a, b] = [0, 1].map(|i| world.spawn(Pos(i)));
    let mut changed = QueryState::<&Pos, Changed<Pos>>::new(&mut world);
    let _ = changed.iter(&world).count();

    world.get_mut::<Pos>(a).unwrap().0 = 5;
    assert_eq!(changed.get(&world, a), Ok(&Pos(5)));
    assert_eq!(changed.get(&world, b), Err(QueryError::DoesNotMatch(b)));
    // Fetching single entities does not count as a run.
    assert_eq!(changed.iter(&world).count(), 1);
}

#[test]
fn one_off_queries_see_everything_as_changed() {
    let mut world = World::new();
    world.spawn_batch((0..4).map(Pos));
    assert_eq!(
        world
            .query::<&Pos>()
            .filter::<Changed<Pos>>()
            .iter()
            .count(),
        4
    );
    assert_eq!(
        world.query::<&Pos>().filter::<Added<Pos>>().iter().count(),
        4
    );
}

#[test]
fn ticks_are_deterministic() {
    fn run() -> (petek::Tick, Vec<Entity>) {
        let mut world = World::new();
        let mut changed = QueryState::<Entity, Changed<Pos>>::new(&mut world);
        let entities: Vec<_> = (0..6).map(|i| world.spawn((Pos(i), Vel(i)))).collect();
        seen(&mut changed, &world);
        for (i, mut pos) in world.query_mut::<&mut Pos>().into_iter().enumerate() {
            if i % 2 == 0 {
                pos.0 += 1;
            }
        }
        world.despawn(entities[0]).unwrap();
        (world.change_tick(), seen(&mut changed, &world))
    }
    assert_eq!(run(), run());
}
