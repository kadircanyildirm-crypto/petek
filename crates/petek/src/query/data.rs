//! What a query fetches for each entity.

use std::any::TypeId;
use std::ptr::NonNull;

use super::sealed;
use crate::archetype::Archetype;
use crate::change::{Mut, Tick, TickSlice, Ticks};
use crate::component::{Component, ComponentId, ComponentInfo, Components};
use crate::entity::Entity;

/// Describes what a query fetches for each entity it matches.
///
/// | Query data     | Item                                | Matches entities    |
/// | -------------- | ----------------------------------- | ------------------- |
/// | `&T`           | `&T`                                | with a `T`          |
/// | `&mut T`       | [`Mut<T>`](crate::Mut)              | with a `T`          |
/// | `Entity`       | the entity                          | all                 |
/// | `Option<D>`    | `Some` of `D`'s item if `D` matches | all                 |
/// | `(D1, D2, ..)` | a tuple of the items                | matching every `Di` |
///
/// Tuples of up to 12 elements are supported, and they nest. `()` matches
/// every entity and fetches nothing.
///
/// A query must not access a component mutably and also elsewhere, since that
/// would alias a mutable reference: running `(&mut Position, &Position)`
/// panics.
///
/// This trait is sealed, because the soundness of queries relies on its
/// implementations: it cannot be implemented outside of `petek`.
pub trait QueryData: sealed::Data {
    /// The type yielded for each matching entity.
    type Item<'w>;

    /// What the query resolves once per world: the ids of its components.
    #[doc(hidden)]
    type State: Copy + Send + Sync + 'static;

    /// What the query resolves once per archetype: where its columns start.
    #[doc(hidden)]
    type Fetch<'w>;

    /// Resolves the state, registering the components it needs.
    #[doc(hidden)]
    fn init_state(components: &mut Components) -> Self::State;

    /// Resolves the state, or returns `None` if a component it needs has not
    /// been registered. No entity can match such a query yet.
    #[doc(hidden)]
    fn get_state(components: &Components) -> Option<Self::State>;

    /// Calls `f` with every component the query accesses, and how.
    ///
    /// This must cover everything [`item`](Self::item) reads or writes.
    #[doc(hidden)]
    fn visit_access(state: &Self::State, f: &mut impl FnMut(ComponentId, Access));

    /// Returns `true` if the query yields an item for every entity of the
    /// archetype.
    #[doc(hidden)]
    fn matches(state: &Self::State, archetype: &Archetype) -> bool;

    /// Prepares to fetch items from an archetype. What the query writes is
    /// stamped with `ticks.this_run`.
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

    /// Fetches the item of the entity at `row`.
    ///
    /// # Safety
    ///
    /// - `row` must be in bounds of the archetype that `fetch` was prepared
    ///   for, and that archetype must not change for `'w`.
    /// - For `'w`, the caller must hold the access that
    ///   [`visit_access`](Self::visit_access) reports: nothing else may write
    ///   a component (or its ticks) that the query reads, or access one it
    ///   writes. This includes other items of the same query, so a query that
    ///   writes a component must fetch each row at most once.
    #[doc(hidden)]
    unsafe fn item<'w>(fetch: &Self::Fetch<'w>, row: usize) -> Self::Item<'w>;
}

/// [`QueryData`] that only reads components.
///
/// Any number of read-only queries can run at the same time, through
/// [`World::query`](crate::World::query). Queries that write components need
/// exclusive access to the world instead:
///
/// ```compile_fail,E0277
/// # use petek::{Component, World};
/// # struct Position(f32);
/// # impl Component for Position {}
/// let world = World::new();
/// for position in world.query::<&mut Position>() {}
/// ```
///
/// This trait is sealed, like [`QueryData`]: implementing it for query data
/// that writes would be unsound.
///
/// ```compile_fail,E0277
/// # use petek::Component;
/// # use petek::query::ReadOnlyQueryData;
/// struct Position(f32);
/// impl Component for Position {}
///
/// impl ReadOnlyQueryData for &mut Position {}
/// ```
pub trait ReadOnlyQueryData: QueryData + sealed::ReadOnly {}

/// How a query accesses a component.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Access {
    /// The query reads the component.
    Read,
    /// The query writes the component.
    Write,
}

/// A pointer to the first item of a column of `T`s, from which a query fetches
/// `&T` or `&mut T` items.
///
/// Unlike a raw pointer, it is `Send` and `Sync`, so that query iterators are
/// too. That is sound because the pointer alone grants nothing: only the
/// items made from it can be used, and they are references to components,
/// which are always `Send + Sync`.
#[derive(Debug)]
pub struct ColumnStart<T>(NonNull<T>);

// SAFETY: see the type's documentation.
unsafe impl<T: Component> Send for ColumnStart<T> {}
// SAFETY: see the type's documentation.
unsafe impl<T: Component> Sync for ColumnStart<T> {}

impl<T> ColumnStart<T> {
    /// Returns the start of `archetype`'s column for component `id`.
    ///
    /// # Panics
    ///
    /// If the archetype has no such column.
    fn of(id: ComponentId, archetype: &Archetype) -> Self {
        let column = archetype
            .column_index(id)
            .expect("the archetype has the component");
        Self(archetype.column(column).as_ptr().cast())
    }

    /// Returns a pointer to the item at `row`.
    ///
    /// # Safety
    ///
    /// `row` must be in bounds of the column.
    #[inline]
    unsafe fn row(&self, row: usize) -> NonNull<T> {
        // SAFETY: the caller guarantees the offset stays within the column.
        unsafe { self.0.add(row) }
    }
}

impl<T: Component> sealed::Data for &T {}
impl<T: Component> sealed::ReadOnly for &T {}
impl<T: Component> ReadOnlyQueryData for &T {}

impl<T: Component> QueryData for &T {
    type Item<'w> = &'w T;
    type State = ComponentId;
    type Fetch<'w> = ColumnStart<T>;

    fn init_state(components: &mut Components) -> ComponentId {
        components.register(ComponentInfo::of::<T>())
    }

    fn get_state(components: &Components) -> Option<ComponentId> {
        components.id(TypeId::of::<T>())
    }

    fn visit_access(&id: &ComponentId, f: &mut impl FnMut(ComponentId, Access)) {
        f(id, Access::Read);
    }

    fn matches(&id: &ComponentId, archetype: &Archetype) -> bool {
        archetype.contains(id)
    }

    unsafe fn fetch<'w>(&id: &ComponentId, archetype: &'w Archetype, _: Ticks) -> Self::Fetch<'w> {
        // The caller guarantees `id` is `T`'s id in this world, so the column
        // stores `T`s.
        ColumnStart::of(id, archetype)
    }

    #[inline]
    unsafe fn item<'w>(start: &Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
        // SAFETY: the caller guarantees `row` is in bounds of the column, so
        // it holds a valid `T`, and that nothing writes it for `'w`.
        unsafe { start.row(row).as_ref() }
    }
}

impl<T: Component> sealed::Data for &mut T {}

impl<T: Component> QueryData for &mut T {
    type Item<'w> = Mut<'w, T>;
    type State = ComponentId;
    /// The column, its change ticks, and the tick to stamp writes with.
    type Fetch<'w> = (ColumnStart<T>, TickSlice<'w>, Tick);

    fn init_state(components: &mut Components) -> ComponentId {
        components.register(ComponentInfo::of::<T>())
    }

    fn get_state(components: &Components) -> Option<ComponentId> {
        components.id(TypeId::of::<T>())
    }

    fn visit_access(&id: &ComponentId, f: &mut impl FnMut(ComponentId, Access)) {
        f(id, Access::Write);
    }

    fn matches(&id: &ComponentId, archetype: &Archetype) -> bool {
        archetype.contains(id)
    }

    unsafe fn fetch<'w>(
        &id: &ComponentId,
        archetype: &'w Archetype,
        ticks: Ticks,
    ) -> Self::Fetch<'w> {
        // As for `&T`.
        let start = ColumnStart::of(id, archetype);
        let column = archetype
            .column_index(id)
            .expect("the archetype has the component");
        (
            start,
            archetype.column(column).changed_ticks(),
            ticks.this_run,
        )
    }

    #[inline]
    unsafe fn item<'w>((start, changed, this_run): &Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
        // SAFETY: the caller guarantees `row` is in bounds of the column, so
        // it holds a valid `T` and a tick, and that nothing else accesses
        // either for `'w`.
        let (value, changed) = unsafe { (start.row(row).as_mut(), changed.get_mut(row)) };
        Mut::new(value, changed, *this_run)
    }
}

impl sealed::Data for Entity {}
impl sealed::ReadOnly for Entity {}
impl ReadOnlyQueryData for Entity {}

impl QueryData for Entity {
    type Item<'w> = Entity;
    type State = ();
    type Fetch<'w> = &'w [Entity];

    fn init_state(_: &mut Components) {}

    fn get_state(_: &Components) -> Option<()> {
        Some(())
    }

    fn visit_access((): &(), _: &mut impl FnMut(ComponentId, Access)) {}

    fn matches((): &(), _: &Archetype) -> bool {
        true
    }

    unsafe fn fetch<'w>((): &(), archetype: &'w Archetype, _: Ticks) -> Self::Fetch<'w> {
        archetype.entities()
    }

    #[inline]
    unsafe fn item<'w>(entities: &Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
        debug_assert!(row < entities.len());
        // SAFETY: the caller guarantees `row` is in bounds.
        unsafe { *entities.get_unchecked(row) }
    }
}

impl<D: QueryData> sealed::Data for Option<D> {}
impl<D: ReadOnlyQueryData> sealed::ReadOnly for Option<D> {}
impl<D: ReadOnlyQueryData> ReadOnlyQueryData for Option<D> {}

impl<D: QueryData> QueryData for Option<D> {
    type Item<'w> = Option<D::Item<'w>>;
    /// `None` if `D`'s components are not registered.
    type State = Option<D::State>;
    /// `None` for archetypes that `D` does not match.
    type Fetch<'w> = Option<D::Fetch<'w>>;

    fn init_state(components: &mut Components) -> Self::State {
        Some(D::init_state(components))
    }

    fn get_state(components: &Components) -> Option<Self::State> {
        Some(D::get_state(components))
    }

    fn visit_access(state: &Self::State, f: &mut impl FnMut(ComponentId, Access)) {
        if let Some(state) = state {
            D::visit_access(state, f);
        }
    }

    fn matches(_: &Self::State, _: &Archetype) -> bool {
        true
    }

    unsafe fn fetch<'w>(
        state: &Self::State,
        archetype: &'w Archetype,
        ticks: Ticks,
    ) -> Self::Fetch<'w> {
        let state = state
            .as_ref()
            .filter(|state| D::matches(state, archetype))?;
        // SAFETY: `D` matches the archetype, and the caller guarantees the
        // state comes from its world.
        Some(unsafe { D::fetch(state, archetype, ticks) })
    }

    #[inline]
    unsafe fn item<'w>(fetch: &Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
        // SAFETY: the caller's guarantees carry over to `D`.
        fetch.as_ref().map(|fetch| unsafe { D::item(fetch, row) })
    }
}

impl sealed::Data for () {}
impl sealed::ReadOnly for () {}
impl ReadOnlyQueryData for () {}

impl QueryData for () {
    type Item<'w> = ();
    type State = ();
    type Fetch<'w> = ();

    fn init_state(_: &mut Components) {}

    fn get_state(_: &Components) -> Option<()> {
        Some(())
    }

    fn visit_access((): &(), _: &mut impl FnMut(ComponentId, Access)) {}

    fn matches((): &(), _: &Archetype) -> bool {
        true
    }

    unsafe fn fetch<'w>((): &(), _: &'w Archetype, _: Ticks) -> Self::Fetch<'w> {}

    #[inline]
    unsafe fn item<'w>((): &Self::Fetch<'w>, _: usize) -> Self::Item<'w> {}
}

macro_rules! impl_query_data_for_tuples {
    () => {};
    ($first:ident $(, $rest:ident)*) => {
        impl<$first: QueryData $(, $rest: QueryData)*> sealed::Data for ($first, $($rest,)*) {}
        impl<$first: ReadOnlyQueryData $(, $rest: ReadOnlyQueryData)*> sealed::ReadOnly
            for ($first, $($rest,)*)
        {
        }
        impl<$first: ReadOnlyQueryData $(, $rest: ReadOnlyQueryData)*> ReadOnlyQueryData
            for ($first, $($rest,)*)
        {
        }

        #[allow(non_snake_case, reason = "each element is named after its type")]
        impl<$first: QueryData $(, $rest: QueryData)*> QueryData for ($first, $($rest,)*) {
            type Item<'w> = ($first::Item<'w>, $($rest::Item<'w>,)*);
            type State = ($first::State, $($rest::State,)*);
            type Fetch<'w> = ($first::Fetch<'w>, $($rest::Fetch<'w>,)*);

            fn init_state(components: &mut Components) -> Self::State {
                ($first::init_state(components), $($rest::init_state(components),)*)
            }

            fn get_state(components: &Components) -> Option<Self::State> {
                Some(($first::get_state(components)?, $($rest::get_state(components)?,)*))
            }

            fn visit_access(state: &Self::State, f: &mut impl FnMut(ComponentId, Access)) {
                let ($first, $($rest,)*) = state;
                $first::visit_access($first, f);
                $($rest::visit_access($rest, f);)*
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
            unsafe fn item<'w>(fetch: &Self::Fetch<'w>, row: usize) -> Self::Item<'w> {
                let ($first, $($rest,)*) = fetch;
                // SAFETY: the caller's guarantees cover every element, since
                // the tuple reports the access of all of them.
                unsafe { ($first::item($first, row), $($rest::item($rest, row),)*) }
            }
        }

        impl_query_data_for_tuples!($($rest),*);
    };
}

impl_query_data_for_tuples!(D0, D1, D2, D3, D4, D5, D6, D7, D8, D9, D10, D11);
