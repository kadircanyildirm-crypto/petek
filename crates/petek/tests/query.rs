//! Tests of queries, through the public API only.

use std::panic::{self, AssertUnwindSafe};
use std::ptr;

use petek::query::{Or, QueryError, QueryState, With, Without};
use petek::{Component, Entity, World};

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pos(i32);
impl Component for Pos {}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Vel(i32);
impl Component for Vel {}

#[derive(Clone, Debug, PartialEq)]
struct Name(&'static str);
impl Component for Name {}

struct Frozen;
impl Component for Frozen {}

fn sorted<T: Ord>(mut items: Vec<T>) -> Vec<T> {
    items.sort();
    items
}

/// Spawns a few entities spread over several archetypes.
fn sample() -> (World, [Entity; 5]) {
    let mut world = World::new();
    let entities = [
        world.spawn(Pos(1)),
        world.spawn((Pos(2), Vel(20))),
        world.spawn((Vel(30), Pos(3), Name("c"))),
        world.spawn((Vel(40), Frozen)),
        world.spawn(()),
    ];
    (world, entities)
}

#[test]
fn queries_visit_every_matching_entity() {
    let (world, [a, b, c, ..]) = sample();

    let positions = world.query::<(Entity, &Pos)>().into_iter();
    let positions = sorted(positions.map(|(entity, pos)| (entity, pos.0)).collect());
    assert_eq!(positions, [(a, 1), (b, 2), (c, 3)]);

    let both = world.query::<(&Pos, &Vel)>().into_iter();
    let both = sorted(both.map(|(pos, vel)| (pos.0, vel.0)).collect());
    assert_eq!(both, [(2, 20), (3, 30)]);

    assert_eq!(world.query::<Entity>().into_iter().count(), 5);
    assert_eq!(world.query::<()>().into_iter().count(), 5);
}

#[test]
fn mutable_queries_write_through() {
    let (mut world, [a, b, c, d, _]) = sample();

    for (mut pos, vel) in world.query_mut::<(&mut Pos, &Vel)>() {
        pos.0 += vel.0;
    }
    world
        .query_mut::<&mut Vel>()
        .into_iter()
        .for_each(|mut vel| vel.0 = -vel.0);

    assert_eq!(world.get::<Pos>(a), Some(&Pos(1)));
    assert_eq!(world.get::<Pos>(b), Some(&Pos(22)));
    assert_eq!(world.get::<Pos>(c), Some(&Pos(33)));
    assert_eq!(world.get::<Vel>(d), Some(&Vel(-40)));
}

#[test]
fn optional_components_match_every_entity() {
    let (mut world, [a, b, ..]) = sample();

    let items = world.query::<(Entity, Option<&Vel>)>().into_iter();
    let items = sorted(
        items
            .map(|(entity, vel)| (entity, vel.map(|vel| vel.0)))
            .collect(),
    );
    assert_eq!(items.len(), 5);
    assert_eq!(&items[..2], [(a, None), (b, Some(20))]);

    for (mut pos, vel) in world.query_mut::<(&mut Pos, Option<&mut Vel>)>() {
        match vel {
            Some(mut vel) => vel.0 += pos.0,
            None => pos.0 = 0,
        }
    }
    assert_eq!(world.get::<Pos>(a), Some(&Pos(0)));
    assert_eq!(world.get::<Vel>(b), Some(&Vel(22)));
}

#[test]
fn filters_narrow_down_the_matches() {
    fn entities(query: impl IntoIterator<Item = Entity>) -> Vec<Entity> {
        sorted(query.into_iter().collect())
    }

    let (mut world, [a, b, c, d, _]) = sample();
    let query = world.query::<Entity>();
    assert_eq!(entities(query.with::<Vel>()), [b, c, d]);
    assert_eq!(entities(query.with::<Vel>().without::<Frozen>()), [b, c]);
    assert_eq!(
        entities(query.filter::<Or<(With<Name>, With<Frozen>)>>()),
        [c, d]
    );
    assert_eq!(entities(query.filter::<(With<Pos>, Without<Vel>)>()), [a]);

    // A filter does not borrow its component, so it can be combined with a
    // mutable borrow of the same component.
    for mut pos in world
        .query_mut::<&mut Pos>()
        .with::<Pos>()
        .without::<Name>()
    {
        pos.0 *= 10;
    }
    assert_eq!(world.get::<Pos>(b), Some(&Pos(20)));
    assert_eq!(world.get::<Pos>(c), Some(&Pos(3)));
}

#[test]
fn query_data_nests() {
    let (world, [_, _, c, ..]) = sample();

    let items: Vec<_> = world
        .query::<((Entity, &Pos), (Option<&Name>, ()))>()
        .with::<Name>()
        .into_iter()
        .collect();
    assert_eq!(items, [((c, &Pos(3)), (Some(&Name("c")), ()))]);
}

#[test]
fn get_fetches_one_entity() {
    let (mut world, [a, b, ..]) = sample();

    let (mut pos, mut vel) = world.query_mut::<(&mut Pos, &mut Vel)>().get(b).unwrap();
    std::mem::swap(&mut pos.0, &mut vel.0);
    assert_eq!(world.get::<Pos>(b), Some(&Pos(20)));

    let query = world.query::<&Pos>().without::<Vel>();
    assert_eq!(query.get(a), Ok(&Pos(1)));
    assert_eq!(query.get(b), Err(QueryError::DoesNotMatch(b)));

    world.despawn(a).unwrap();
    let error = world.query::<&Pos>().get(a).unwrap_err();
    assert_eq!(error, QueryError::NoSuchEntity(a));
    assert_eq!(error.to_string(), format!("entity {a} does not exist"));
    assert_eq!(
        QueryError::DoesNotMatch(b).to_string(),
        format!("entity {b} does not match the query")
    );
}

#[test]
fn read_only_queries_can_overlap() {
    let (world, _) = sample();

    // Read-only queries are `Copy`, and any number can run at once.
    let positions = world.query::<&Pos>();
    let mut pairs = 0;
    for a in positions {
        for b in positions {
            pairs += usize::from(ptr::eq(a, b));
        }
    }
    assert_eq!(pairs, 3);

    // Reading a component twice in one query is fine.
    assert!(
        world
            .query::<(&Pos, &Pos)>()
            .into_iter()
            .all(|(a, b)| ptr::eq(a, b))
    );
}

#[test]
fn conflicting_access_is_rejected() {
    fn panic_message(f: impl FnOnce()) -> String {
        let payload = panic::catch_unwind(AssertUnwindSafe(f)).unwrap_err();
        payload
            .downcast::<String>()
            .map(|message| *message)
            .unwrap()
    }

    let (mut world, _) = sample();
    let message = panic_message(|| drop(world.query_mut::<(&mut Pos, &Pos)>()));
    assert!(message.contains("accesses"), "{message}");
    assert!(message.contains("Pos`"), "{message}");

    assert!(
        panic::catch_unwind(AssertUnwindSafe(|| {
            drop(world.query_mut::<(&Vel, (&mut Pos, Option<&mut Pos>))>());
        }))
        .is_err()
    );
    assert!(
        panic::catch_unwind(AssertUnwindSafe(|| {
            drop(world.query_mut::<(Option<&Pos>, &mut Pos)>());
        }))
        .is_err()
    );

    // The world is unaffected, and disjoint access is fine.
    for (mut pos, vel) in world.query_mut::<(&mut Pos, &mut Vel)>() {
        pos.0 += vel.0;
    }
}

#[test]
fn size_hint_and_count_are_exact() {
    let (mut world, [a, ..]) = sample();
    world.spawn_batch((0..10).map(|i| (Pos(i), Vel(i))));
    world.despawn(a).unwrap(); // leaves an empty archetype behind

    let query = world.query::<&Pos>();
    let mut iter = query.into_iter();
    assert_eq!(iter.size_hint(), (12, Some(12)));
    for left in (0..12).rev() {
        iter.next().unwrap();
        assert_eq!(iter.size_hint(), (left, Some(left)));
    }
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next(), None);

    // `count` adds up the archetype sizes instead of visiting each item.
    assert_eq!(query.into_iter().count(), 12);
    let mut iter = query.into_iter();
    iter.next();
    assert_eq!(iter.count(), 11);

    // `fold`, which `for_each` and `sum` use, visits the same items as `next`.
    let mut iter = world.query::<&Pos>().into_iter();
    let first = iter.next().unwrap().0;
    let rest: i32 = iter.map(|pos| pos.0).sum();
    let all: i32 = world.query::<&Pos>().into_iter().map(|pos| pos.0).sum();
    assert_eq!(first + rest, all);
    assert_eq!(all, 2 + 3 + (0..10).sum::<i32>());
}

#[test]
fn unregistered_components_match_nothing() {
    struct Unknown;
    impl Component for Unknown {}

    let (world, [a, ..]) = sample();
    assert_eq!(world.query::<&Unknown>().into_iter().count(), 0);
    assert_eq!(
        world.query::<(&Pos, &Unknown)>().get(a).err(),
        Some(QueryError::DoesNotMatch(a))
    );
    assert_eq!(
        world
            .query::<Entity>()
            .with::<Unknown>()
            .into_iter()
            .count(),
        0
    );
    assert_eq!(
        world
            .query::<Entity>()
            .without::<Unknown>()
            .into_iter()
            .count(),
        5
    );
    assert!(
        world
            .query::<Option<&Unknown>>()
            .into_iter()
            .all(|unknown| unknown.is_none())
    );
}

#[test]
fn queries_follow_structural_changes() {
    let (mut world, [a, b, ..]) = sample();
    world.insert(a, Vel(10)).unwrap();
    world.remove::<Vel>(b).unwrap();

    let moving = world.query::<(Entity, &Pos)>().with::<Vel>().into_iter();
    let moving = sorted(moving.map(|(entity, _)| entity).collect());
    let c = world
        .query::<Entity>()
        .with::<Name>()
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(moving, sorted(vec![a, c]));
}

#[test]
fn unusual_component_types() {
    #[derive(Debug, PartialEq)]
    #[repr(align(128))]
    struct Aligned(u8);
    impl Component for Aligned {}

    #[derive(Debug, PartialEq)]
    struct Zst;
    impl Component for Zst {}

    let mut world = World::new();
    world.spawn_batch((0..20).map(|i| (Aligned(i), Zst)));
    for (mut aligned, mut zst) in world.query_mut::<(&mut Aligned, &mut Zst)>() {
        assert_eq!(ptr::from_ref(&*aligned).addr() % 128, 0);
        aligned.0 += 1;
        *zst = Zst;
    }
    let sum: u32 = world
        .query::<&Aligned>()
        .into_iter()
        .map(|aligned| u32::from(aligned.0))
        .sum();
    assert_eq!(sum, (1..=20).sum());
}

#[test]
fn query_states_pick_up_new_archetypes() {
    let mut world = World::new();
    let mut state = QueryState::<(&mut Pos, &Vel), Without<Frozen>>::new(&mut world);
    let mut step = |world: &mut World| {
        for (mut pos, vel) in state.iter_mut(world) {
            pos.0 += vel.0;
        }
    };

    let a = world.spawn((Pos(0), Vel(1)));
    step(&mut world);
    let b = world.spawn((Pos(0), Vel(2), Name("b")));
    let frozen = world.spawn((Pos(0), Vel(3), Frozen));
    step(&mut world);
    world.despawn(a).unwrap(); // leaves an empty archetype behind
    step(&mut world);

    assert_eq!(world.get::<Pos>(b), Some(&Pos(4)));
    assert_eq!(world.get::<Pos>(frozen), Some(&Pos(0)));
}

#[test]
fn query_states_fetch_single_entities() {
    let (mut world, [a, b, c, ..]) = sample();
    let mut state = QueryState::<&mut Pos, With<Vel>>::new(&mut world);

    state.get_mut(&mut world, b).unwrap().0 = 7;
    assert_eq!(world.get::<Pos>(b), Some(&Pos(7)));
    assert_eq!(
        state.get_mut(&mut world, a).err(),
        Some(QueryError::DoesNotMatch(a))
    );

    let reader = QueryState::<(Entity, &Pos)>::new(&mut world);
    assert_eq!(reader.get(&world, c), Ok((c, &Pos(3))));
    world.despawn(c).unwrap();
    assert_eq!(reader.get(&world, c), Err(QueryError::NoSuchEntity(c)));
    assert_eq!(state.iter_mut(&mut world).count(), 1);
}

#[test]
#[should_panic(expected = "accesses")]
fn query_states_reject_conflicting_access() {
    let mut world = World::new();
    let _ = QueryState::<(&mut Pos, Option<&Pos>)>::new(&mut world);
}

#[test]
fn queries_can_move_to_other_threads() {
    fn assert_send_sync<T: Send + Sync>(_: &T) {}

    let (mut world, [_, b, ..]) = sample();
    let query = world.query_mut::<(&mut Pos, Entity, Option<&Vel>)>();
    assert_send_sync(&query);
    let iter = query.into_iter();
    assert_send_sync(&iter);

    std::thread::scope(|scope| {
        scope.spawn(move || {
            for (mut pos, _, vel) in iter {
                pos.0 += vel.map_or(0, |vel| vel.0);
            }
        });
    });
    assert_eq!(world.get::<Pos>(b), Some(&Pos(22)));
}

#[test]
fn borrowed_queries_can_be_used_again() {
    let (mut world, [a, b, ..]) = sample();

    let mut query = world.query_mut::<&mut Pos>();
    for mut pos in &mut query {
        pos.0 += 1;
    }
    query.iter_mut().for_each(|mut pos| pos.0 *= 2);
    query.get_mut(a).unwrap().0 += 100;
    assert_eq!(query.iter_mut().len(), 3);

    let query = world.query::<&Pos>();
    assert_eq!(query.iter().len(), 3);
    assert_eq!((&query).into_iter().len(), 3);
    assert_eq!(query.get(a), Ok(&Pos(104)));
    assert_eq!(query.get(b), Ok(&Pos(6)));
}

#[test]
fn query_state_items_only_borrow_the_world() {
    let (mut world, [a, ..]) = sample();
    let mut state = QueryState::<&Pos>::new(&mut world);

    // The items of one run stay usable while the state runs again.
    let first: Vec<&Pos> = state.iter(&world).collect();
    let second: Vec<&Pos> = state.iter(&world).collect();
    assert_eq!(first, second);
    assert_eq!(state.get(&world, a), Ok(first[0]));
}
