//! Filters that narrow down which entities a query matches.

use std::any::{TypeId, type_name};
use std::fmt;
use std::marker::PhantomData;

use super::sealed;
use crate::archetype::Archetype;
use crate::component::{Component, ComponentId, ComponentInfo, Components};

/// Narrows down which entities a query matches, without fetching anything.
///
/// | Filter            | Matches entities                    |
/// | ----------------- | ----------------------------------- |
/// | [`With<T>`]       | with a `T`                          |
/// | [`Without<T>`]    | without a `T`                       |
/// | `(F1, F2, ..)`    | matching every `Fi`                 |
/// | [`Or<(F1, F2, ..)>`](Or) | matching at least one `Fi`   |
/// | `()`              | all                                 |
///
/// Tuples of up to 12 filters are supported, and they nest.
///
/// This trait is sealed, like [`QueryData`](super::QueryData): it cannot be
/// implemented outside of `petek`.
pub trait QueryFilter: sealed::Filter {
    /// What the filter resolves once per world: the ids of its components.
    #[doc(hidden)]
    type State: Copy + Send + Sync + 'static;

    /// Resolves the state, registering the components it needs.
    #[doc(hidden)]
    fn init_state(components: &mut Components) -> Self::State;

    /// Resolves the state without registering anything.
    #[doc(hidden)]
    fn get_state(components: &Components) -> Self::State;

    /// Returns `true` if the filter lets every entity of the archetype
    /// through.
    #[doc(hidden)]
    fn matches(state: &Self::State, archetype: &Archetype) -> bool;
}

/// A [`QueryFilter`] that matches entities with a `T` component.
///
/// Unlike querying for `&T`, this does not borrow the component, so it can be
/// combined with `&mut T` in the same query.
pub struct With<T>(PhantomData<fn() -> T>);

/// A [`QueryFilter`] that matches entities without a `T` component.
pub struct Without<T>(PhantomData<fn() -> T>);

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
/// assert_eq!(friendly.into_iter().map(|health| health.0).sum::<u32>(), 30);
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

impl_debug_for_markers!(With, Without, Or);

impl<T: Component> sealed::Filter for With<T> {}

impl<T: Component> QueryFilter for With<T> {
    /// `None` if `T` is not registered.
    type State = Option<ComponentId>;

    fn init_state(components: &mut Components) -> Self::State {
        Some(components.register(ComponentInfo::of::<T>()))
    }

    fn get_state(components: &Components) -> Self::State {
        components.id(TypeId::of::<T>())
    }

    fn matches(state: &Self::State, archetype: &Archetype) -> bool {
        state.is_some_and(|id| archetype.contains(id))
    }
}

impl<T: Component> sealed::Filter for Without<T> {}

impl<T: Component> QueryFilter for Without<T> {
    /// `None` if `T` is not registered.
    type State = Option<ComponentId>;

    fn init_state(components: &mut Components) -> Self::State {
        Some(components.register(ComponentInfo::of::<T>()))
    }

    fn get_state(components: &Components) -> Self::State {
        components.id(TypeId::of::<T>())
    }

    fn matches(state: &Self::State, archetype: &Archetype) -> bool {
        state.is_none_or(|id| !archetype.contains(id))
    }
}

impl sealed::Filter for () {}

impl QueryFilter for () {
    type State = ();

    fn init_state(_: &mut Components) {}

    fn get_state(_: &Components) {}

    fn matches((): &(), _: &Archetype) -> bool {
        true
    }
}

macro_rules! impl_query_filter_for_tuples {
    () => {};
    ($first:ident $(, $rest:ident)*) => {
        impl<$first: QueryFilter $(, $rest: QueryFilter)*> sealed::Filter for ($first, $($rest,)*) {}

        #[allow(non_snake_case, reason = "each element is named after its type")]
        impl<$first: QueryFilter $(, $rest: QueryFilter)*> QueryFilter for ($first, $($rest,)*) {
            type State = ($first::State, $($rest::State,)*);

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
        }

        impl<$first: QueryFilter $(, $rest: QueryFilter)*> sealed::Filter
            for Or<($first, $($rest,)*)>
        {
        }

        #[allow(non_snake_case, reason = "each element is named after its type")]
        impl<$first: QueryFilter $(, $rest: QueryFilter)*> QueryFilter
            for Or<($first, $($rest,)*)>
        {
            type State = ($first::State, $($rest::State,)*);

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
        }

        impl_query_filter_for_tuples!($($rest),*);
    };
}

impl_query_filter_for_tuples!(F0, F1, F2, F3, F4, F5, F6, F7, F8, F9, F10, F11);
