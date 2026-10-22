//! Filters that narrow down which entities a query matches.

use std::any::{TypeId, type_name};
use std::fmt;
use std::marker::PhantomData;

use super::sealed;
use crate::archetype::Archetype;
use crate::change::{Tick, TickSlice, Ticks};
use crate::component::{Component, ComponentId, ComponentInfo, Components};

/// Narrows down which entities a query matches, without fetching anything.
///
/// | Filter                   | Matches entities                                 |
/// | ------------------------ | ------------------------------------------------ |
/// | [`With<T>`]              | with a `T`                                       |
/// | [`Without<T>`]           | without a `T`                                    |
/// | [`Added<T>`]             | whose `T` was added since the query last ran     |
/// | [`Changed<T>`]           | whose `T` was added or changed since then        |
/// | `(F1, F2, ..)`           | matching every `Fi`                              |
/// | [`Or<(F1, F2, ..)>`](Or) | matching at least one `Fi`                       |
/// | `()`                     | all                                              |
///
/// Tuples of up to 12 filters are supported, and they nest.
///
/// This trait is sealed, like [`QueryData`](super::QueryData): it cannot be
/// implemented outside of `petek`.
pub trait QueryFilter: sealed::Filter {
    /// What the filter resolves once per world: the ids of its components.
    #[doc(hidden)]
    type State: Copy + Send + Sync + 'static;

    /// What the filter resolves once per archetype, to check single rows.
    #[doc(hidden)]
    type Fetch<'w>;

    /// `true` if the filter decides for whole archetypes at once, never
    /// rejecting single rows.
    #[doc(hidden)]
    const IS_ARCHETYPAL: bool;

    /// Resolves the state, registering the components it needs.
    #[doc(hidden)]
    fn init_state(components: &mut Components) -> Self::State;

    /// Resolves the state without registering anything.
    #[doc(hidden)]
    fn get_state(components: &Components) -> Self::State;

    /// Returns `true` if the filter may let entities of the archetype
    /// through. Unless the filter is archetypal, it still checks each row.
    #[doc(hidden)]
    fn matches(state: &Self::State, archetype: &Archetype) -> bool;

    /// Prepares to check the rows of an archetype.
    ///
    /// # Safety
    ///
    /// `archetype` must match, and `state` must come from its world.
    #[doc(hidden)]
    unsafe fn fetch<'w>(
        state: &Self::State,
        archetype: &'w Archetype,
        ticks: Ticks,
    ) -> Self::Fetch<'w>;

    /// Returns `true` if the filter lets the entity at `row` through.
    ///
    /// # Safety
    ///
    /// - `row` must be in bounds of the archetype that `fetch` was prepared
    ///   for, and that archetype must not change for `'w`.
    /// - No one may be writing the ticks of the components the filter
    ///   checks. (A query that writes one of them only does so for rows it
    ///   has already let through, so it can still use the filter.)
    #[doc(hidden)]
    unsafe fn filter_row(fetch: &Self::Fetch<'_>, row: usize) -> bool;
}

/// A [`QueryFilter`] that decides for whole archetypes at once, so the number
/// of entities a query matches is known before visiting them.
///
/// Every filter except [`Added`] and [`Changed`] is one, and so are tuples and
/// [`Or`]s of them. It is sealed, like [`QueryFilter`].
///
/// Only queries with such filters have an exact length up front:
///
/// ```compile_fail,E0599
/// # use petek::{Component, World};
/// # use petek::query::Changed;
/// # struct Position(f32);
/// # impl Component for Position {}
/// let world = World::new();
/// let len = world.query::<&Position>().filter::<Changed<Position>>().iter().len();
/// ```
pub trait ArchetypeFilter: QueryFilter + sealed::Archetypal {}

/// A [`QueryFilter`] that matches entities with a `T` component.
///
/// Unlike querying for `&T`, this does not borrow the component, so it can be
/// combined with `&mut T` in the same query.
pub struct With<T>(PhantomData<fn() -> T>);

/// A [`QueryFilter`] that matches entities without a `T` component.
pub struct Without<T>(PhantomData<fn() -> T>);

/// A [`QueryFilter`] that matches entities whose `T` component was added
/// since the query last ran.
///
/// Only a [`QueryState`](super::QueryState) remembers when it last ran: a
/// one-off [`Query`](super::Query) has never run before, so to it every
/// component counts as added.
pub struct Added<T>(PhantomData<fn() -> T>);

/// A [`QueryFilter`] that matches entities whose `T` component was added or
/// changed since the query last ran.
///
/// A component changes when it is written through a [`Mut`](crate::Mut), or
/// overwritten by [`World::insert`](crate::World::insert). As for [`Added`],
/// to a one-off [`Query`](super::Query) every component counts as changed.
pub struct Changed<T>(PhantomData<fn() -> T>);

/// A [`QueryFilter`] that matches entities that pass at least one of the
/// filters in the tuple `T`.
///
/// ```
/// use petek::{Component, World};
/// use petek::query::{Or, With};
///
/// struct Health(u32);
/// impl Component for Health {}
///
/// struct Player;
/// impl Component for Player {}
///
/// struct Ally;
/// impl Component for Ally {}
///
/// let mut world = World::new();
/// world.spawn((Health(10), Player));
/// world.spawn((Health(20), Ally));
/// world.spawn(Health(30));
///
/// let friendly = world.query::<&Health>().filter::<Or<(With<Player>, With<Ally>)>>();
/// assert_eq!(friendly.iter().map(|health| health.0).sum::<u32>(), 30);
/// ```
pub struct Or<T>(PhantomData<fn() -> T>);

macro_rules! impl_debug_for_markers {
    ($($marker:ident),*) => {$(
        impl<T> fmt::Debug for $marker<T> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, concat!(stringify!($marker), "<{}>"), type_name::<T>())
            }
        }
    )*};
}

impl_debug_for_markers!(With, Without, Added, Changed, Or);

/// Resolves the id of `T`, registering it if needed.
fn init_id<T: Component>(components: &mut Components) -> ComponentId {
    components.register(ComponentInfo::of::<T>())
}

/// Resolves the id of `T`, or `None` if it is not registered.
fn get_id<T: Component>(components: &Components) -> Option<ComponentId> {
    components.id(TypeId::of::<T>())
}

impl<T: Component> sealed::Filter for With<T> {}
impl<T: Component> sealed::Archetypal for With<T> {}
impl<T: Component> ArchetypeFilter for With<T> {}

impl<T: Component> QueryFilter for With<T> {
    /// `None` if `T` is not registered.
    type State = Option<ComponentId>;
    type Fetch<'w> = ();
    const IS_ARCHETYPAL: bool = true;

    fn init_state(components: &mut Components) -> Self::State {
        Some(init_id::<T>(components))
    }

    fn get_state(components: &Components) -> Self::State {
        get_id::<T>(components)
    }

    fn matches(state: &Self::State, archetype: &Archetype) -> bool {
        state.is_some_and(|id| archetype.contains(id))
    }

    unsafe fn fetch(_: &Self::State, _: &Archetype, _: Ticks) {}

    #[inline]
    unsafe fn filter_row((): &Self::Fetch<'_>, _: usize) -> bool {
        true
    }
}

impl<T: Component> sealed::Filter for Without<T> {}
impl<T: Component> sealed::Archetypal for Without<T> {}
impl<T: Component> ArchetypeFilter for Without<T> {}

impl<T: Component> QueryFilter for Without<T> {
    /// `None` if `T` is not registered.
    type State = Option<ComponentId>;
    type Fetch<'w> = ();
    const IS_ARCHETYPAL: bool = true;

    fn init_state(components: &mut Components) -> Self::State {
        Some(init_id::<T>(components))
    }

    fn get_state(components: &Components) -> Self::State {
        get_id::<T>(components)
    }

    fn matches(state: &Self::State, archetype: &Archetype) -> bool {
        state.is_none_or(|id| !archetype.contains(id))
    }

    unsafe fn fetch(_: &Self::State, _: &Archetype, _: Ticks) {}

    #[inline]
    unsafe fn filter_row((): &Self::Fetch<'_>, _: usize) -> bool {
        true
    }
}

/// Implements a filter that compares one of `T`'s tick columns with the
/// query's last run.
macro_rules! impl_tick_filter {
    ($filter:ident, $ticks:ident) => {
        impl<T: Component> sealed::Filter for $filter<T> {}

        impl<T: Component> QueryFilter for $filter<T> {
            /// `None` if `T` is not registered.
            type State = Option<ComponentId>;
            /// The ticks of `T`'s column, and the query's last run.
            type Fetch<'w> = (TickSlice<'w>, Tick);
            const IS_ARCHETYPAL: bool = false;

            fn init_state(components: &mut Components) -> Self::State {
                Some(init_id::<T>(components))
            }

            fn get_state(components: &Components) -> Self::State {
                get_id::<T>(components)
            }

            fn matches(state: &Self::State, archetype: &Archetype) -> bool {
                state.is_some_and(|id| archetype.contains(id))
            }

            unsafe fn fetch<'w>(
                state: &Self::State,
                archetype: &'w Archetype,
                ticks: Ticks,
            ) -> Self::Fetch<'w> {
                let column = state
                    .and_then(|id| archetype.column_index(id))
                    .expect("the archetype has the component");
                (archetype.column(column).$ticks(), ticks.last_run)
            }

            #[inline]
            unsafe fn filter_row((ticks, last_run): &Self::Fetch<'_>, row: usize) -> bool {
                // SAFETY: the caller guarantees `row` is in bounds and that no
                // one is writing the tick.
                unsafe { ticks.read(row) > *last_run }
            }
        }
    };
}

impl_tick_filter!(Added, added_ticks);
impl_tick_filter!(Changed, changed_ticks);

impl sealed::Filter for () {}
impl sealed::Archetypal for () {}
impl ArchetypeFilter for () {}

impl QueryFilter for () {
    type State = ();
    type Fetch<'w> = ();
    const IS_ARCHETYPAL: bool = true;

    fn init_state(_: &mut Components) {}

    fn get_state(_: &Components) {}

    fn matches((): &(), _: &Archetype) -> bool {
        true
    }

    unsafe fn fetch((): &(), _: &Archetype, _: Ticks) {}

    #[inline]
    unsafe fn filter_row((): &Self::Fetch<'_>, _: usize) -> bool {
        true
    }
}

macro_rules! impl_query_filter_for_tuples {
    () => {};
    ($first:ident $(, $rest:ident)*) => {
        impl<$first: QueryFilter $(, $rest: QueryFilter)*> sealed::Filter for ($first, $($rest,)*) {}
        impl<$first: ArchetypeFilter $(, $rest: ArchetypeFilter)*> sealed::Archetypal
            for ($first, $($rest,)*)
        {
        }
        impl<$first: ArchetypeFilter $(, $rest: ArchetypeFilter)*> ArchetypeFilter
            for ($first, $($rest,)*)
        {
        }

        #[allow(non_snake_case, reason = "each element is named after its type")]
        impl<$first: QueryFilter $(, $rest: QueryFilter)*> QueryFilter for ($first, $($rest,)*) {
            type State = ($first::State, $($rest::State,)*);
            type Fetch<'w> = ($first::Fetch<'w>, $($rest::Fetch<'w>,)*);
            const IS_ARCHETYPAL: bool = $first::IS_ARCHETYPAL $(&& $rest::IS_ARCHETYPAL)*;

            fn init_state(components: &mut Components) -> Self::State {
                ($first::init_state(components), $($rest::init_state(components),)*)
            }

            fn get_state(components: &Components) -> Self::State {
                ($first::get_state(components), $($rest::get_state(components),)*)
            }

            fn matches(state: &Self::State, archetype: &Archetype) -> bool {
                let ($first, $($rest,)*) = state;
                $first::matches($first, archetype) $(&& $rest::matches($rest, archetype))*
            }

            unsafe fn fetch<'w>(
                state: &Self::State,
                archetype: &'w Archetype,
                ticks: Ticks,
            ) -> Self::Fetch<'w> {
                let ($first, $($rest,)*) = state;
                // SAFETY: a tuple matches only if every element does, and the
                // caller's guarantee about the state covers every element.
                unsafe {
                    (
                        $first::fetch($first, archetype, ticks),
                        $($rest::fetch($rest, archetype, ticks),)*
                    )
                }
            }

            #[inline]
            unsafe fn filter_row(fetch: &Self::Fetch<'_>, row: usize) -> bool {
                let ($first, $($rest,)*) = fetch;
                // SAFETY: the caller's guarantees cover every element.
                unsafe { $first::filter_row($first, row) $(&& $rest::filter_row($rest, row))* }
            }
        }

        impl<$first: QueryFilter $(, $rest: QueryFilter)*> sealed::Filter
            for Or<($first, $($rest,)*)>
        {
        }
        impl<$first: ArchetypeFilter $(, $rest: ArchetypeFilter)*> sealed::Archetypal
            for Or<($first, $($rest,)*)>
        {
        }
        impl<$first: ArchetypeFilter $(, $rest: ArchetypeFilter)*> ArchetypeFilter
            for Or<($first, $($rest,)*)>
        {
        }

        #[allow(non_snake_case, reason = "each element is named after its type")]
        impl<$first: QueryFilter $(, $rest: QueryFilter)*> QueryFilter
            for Or<($first, $($rest,)*)>
        {
            type State = ($first::State, $($rest::State,)*);
            /// For each element, `None` if it rejects the whole archetype.
            type Fetch<'w> = (Option<$first::Fetch<'w>>, $(Option<$rest::Fetch<'w>>,)*);
            const IS_ARCHETYPAL: bool = $first::IS_ARCHETYPAL $(&& $rest::IS_ARCHETYPAL)*;

            fn init_state(components: &mut Components) -> Self::State {
                ($first::init_state(components), $($rest::init_state(components),)*)
            }

            fn get_state(components: &Components) -> Self::State {
                ($first::get_state(components), $($rest::get_state(components),)*)
            }

            fn matches(state: &Self::State, archetype: &Archetype) -> bool {
                let ($first, $($rest,)*) = state;
                $first::matches($first, archetype) $(|| $rest::matches($rest, archetype))*
            }

            unsafe fn fetch<'w>(
                state: &Self::State,
                archetype: &'w Archetype,
                ticks: Ticks,
            ) -> Self::Fetch<'w> {
                let ($first, $($rest,)*) = state;
                // SAFETY: each element is only fetched if it matches, and the
                // caller's guarantee about the state covers every element.
                unsafe {
                    (
                        $first::matches($first, archetype)
                            .then(|| $first::fetch($first, archetype, ticks)),
                        $($rest::matches($rest, archetype)
                            .then(|| $rest::fetch($rest, archetype, ticks)),)*
                    )
                }
            }

            #[inline]
            unsafe fn filter_row(fetch: &Self::Fetch<'_>, row: usize) -> bool {
                let ($first, $($rest,)*) = fetch;
                // SAFETY: the caller's guarantees cover every element.
                unsafe {
                    $first.as_ref().is_some_and(|fetch| $first::filter_row(fetch, row))
                        $(|| $rest.as_ref().is_some_and(|fetch| $rest::filter_row(fetch, row)))*
                }
            }
        }

        impl_query_filter_for_tuples!($($rest),*);
    };
}

impl_query_filter_for_tuples!(F0, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11);
