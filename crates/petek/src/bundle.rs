//! Bundles: groups of components that are added to or removed from an entity
//! together.

use std::any::{TypeId, type_name};
use std::mem::ManuallyDrop;
use std::ptr::NonNull;

use crate::component::{Component, ComponentId, ComponentInfo, Components};
use crate::hash::FastHashMap;

/// A group of components that are added to or removed from an entity
/// together.
///
/// Every [`Component`] is a bundle, and so is every tuple of up to 12
/// bundles. Tuples nest, so `((A, B), C)` is a bundle too, and `()` is the
/// empty bundle.
///
/// ```
/// use petek::{Component, World};
///
/// struct Position(f32, f32);
/// impl Component for Position {}
///
/// struct Velocity(f32, f32);
/// impl Component for Velocity {}
///
/// let mut world = World::new();
/// let entity = world.spawn((Position(0.0, 0.0), Velocity(1.0, 0.0)));
/// world.remove::<(Position, Velocity)>(entity).unwrap();
/// assert!(!world.has::<Position>(entity));
/// ```
///
/// # Safety
///
/// Implementing this trait by hand is not supported: it is implemented for
/// every component and every tuple of bundles, and its methods are an
/// implementation detail that may change. A derive macro for named bundles is
/// planned.
///
/// Internally, worlds move components into and out of bundles through raw
/// pointers. They rely on `component_infos` describing exactly the components
/// that `take_components` hands over and `from_components` takes, in the same
/// order.
pub unsafe trait Bundle: 'static {
    /// Calls `f` with the description of each component in the bundle.
    #[doc(hidden)]
    fn component_infos(f: &mut impl FnMut(ComponentInfo));

    /// Moves each component out of the bundle by handing `f` a pointer to it.
    ///
    /// `f` takes ownership of the value behind the pointer and must move it
    /// out before returning.
    #[doc(hidden)]
    fn take_components(self, f: &mut impl FnMut(NonNull<u8>));

    /// Builds a bundle from the components that `f` points to.
    ///
    /// # Safety
    ///
    /// Each call to `f` must return a pointer to a valid value of the next
    /// component's type, and ownership of that value passes to the bundle.
    #[doc(hidden)]
    unsafe fn from_components(f: &mut impl FnMut() -> NonNull<u8>) -> Self;
}

// SAFETY: a single component reports itself, and hands over or takes exactly
// one value of its own type.
unsafe impl<C: Component> Bundle for C {
    fn component_infos(f: &mut impl FnMut(ComponentInfo)) {
        f(ComponentInfo::of::<C>());
    }

    fn take_components(self, f: &mut impl FnMut(NonNull<u8>)) {
        let mut value = ManuallyDrop::new(self);
        f(NonNull::from(&mut *value).cast());
    }

    unsafe fn from_components(f: &mut impl FnMut() -> NonNull<u8>) -> Self {
        // SAFETY: the caller guarantees `f` returns a pointer to an owned `C`.
        unsafe { f().cast::<C>().read() }
    }
}

// SAFETY: the empty bundle has no components and never calls `f`.
unsafe impl Bundle for () {
    fn component_infos(_: &mut impl FnMut(ComponentInfo)) {}

    fn take_components(self, _: &mut impl FnMut(NonNull<u8>)) {}

    unsafe fn from_components(_: &mut impl FnMut() -> NonNull<u8>) -> Self {}
}

macro_rules! impl_bundle_for_tuples {
    () => {};
    ($first:ident $(, $rest:ident)*) => {
        // SAFETY: each element upholds the contract on its own, and all three
        // methods visit the elements in the same order.
        unsafe impl<$first: Bundle $(, $rest: Bundle)*> Bundle for ($first, $($rest,)*) {
            fn component_infos(f: &mut impl FnMut(ComponentInfo)) {
                $first::component_infos(f);
                $($rest::component_infos(f);)*
            }

            #[allow(non_snake_case)]
            fn take_components(self, f: &mut impl FnMut(NonNull<u8>)) {
                let ($first, $($rest,)*) = self;
                $first.take_components(f);
                $($rest.take_components(f);)*
            }

            unsafe fn from_components(f: &mut impl FnMut() -> NonNull<u8>) -> Self {
                // SAFETY: the caller's guarantee covers every element in turn,
                // and tuple fields are evaluated from left to right.
                unsafe { ($first::from_components(f), $($rest::from_components(f),)*) }
            }
        }

        impl_bundle_for_tuples!($($rest),*);
    };
}

impl_bundle_for_tuples!(B0, B1, B2, B3, B4, B5, B6, B7, B8, B9, B10, B11);

/// Identifies a bundle type within one world.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct BundleId(u32);

/// What a world knows about a bundle type.
pub(crate) struct BundleInfo {
    /// The bundle's components, in the order the bundle visits them. No
    /// component appears twice.
    pub(crate) component_ids: Box<[ComponentId]>,
}

/// Assigns a [`BundleId`] to each bundle type a world encounters.
#[derive(Default)]
pub(crate) struct Bundles {
    infos: Vec<BundleInfo>,
    ids: FastHashMap<TypeId, BundleId>,
}

impl Bundles {
    /// Returns the id of bundle type `B`, registering it and its components on
    /// first use.
    ///
    /// # Panics
    ///
    /// If `B` contains the same component more than once.
    #[track_caller]
    pub(crate) fn register<B: Bundle>(&mut self, components: &mut Components) -> BundleId {
        if let Some(&id) = self.ids.get(&TypeId::of::<B>()) {
            return id;
        }
        let mut component_ids = Vec::new();
        B::component_infos(&mut |info| component_ids.push(components.register(info)));
        let mut sorted = component_ids.clone();
        sorted.sort_unstable();
        if let Some(pair) = sorted.windows(2).find(|pair| pair[0] == pair[1]) {
            panic!(
                "bundle `{}` contains component `{}` more than once",
                type_name::<B>(),
                components.info(pair[0]).name(),
            );
        }

        let id = BundleId(u32::try_from(self.infos.len()).expect("too many bundle types"));
        self.infos.push(BundleInfo {
            component_ids: component_ids.into(),
        });
        self.ids.insert(TypeId::of::<B>(), id);
        id
    }

    #[inline]
    pub(crate) fn info(&self, id: BundleId) -> &BundleInfo {
        &self.infos[id.0 as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::blob_vec::BlobVec;

    #[derive(Debug, PartialEq)]
    struct A(u32);
    impl Component for A {}

    #[derive(Debug, PartialEq)]
    struct B(String);
    impl Component for B {}

    #[derive(Debug, PartialEq)]
    struct C;
    impl Component for C {}

    fn type_ids<T: Bundle>() -> Vec<TypeId> {
        let mut ids = Vec::new();
        T::component_infos(&mut |info| ids.push(info.type_id()));
        ids
    }

    #[test]
    fn nested_tuples_flatten_in_order() {
        assert_eq!(
            type_ids::<((A, B), (), C)>(),
            [TypeId::of::<A>(), TypeId::of::<B>(), TypeId::of::<C>()],
        );
        assert_eq!(type_ids::<()>(), Vec::<TypeId>::new());
    }

    #[test]
    fn components_round_trip_through_storage() {
        type Nested = (A, (B, C));

        // Move each component into a column of its own...
        let mut columns = Vec::new();
        Nested::component_infos(&mut |info| {
            let mut column = BlobVec::new(info.layout(), info.drop_fn());
            column.reserve(1);
            columns.push(column);
        });
        let mut next = 0;
        (A(7), (B("seven".into()), C)).take_components(&mut |value| {
            // SAFETY: `component_infos` and `take_components` visit the
            // components in the same order, so `value` matches the column,
            // which has room for it.
            unsafe {
                columns[next].write_spare(value);
                columns[next].commit_spare();
            }
            next += 1;
        });

        // ...then read them back out.
        let mut next = 0;
        let mut read = || {
            next += 1;
            // SAFETY: each column holds exactly one value, which is moved out
            // once and read by `from_components` right away.
            unsafe { columns[next - 1].swap_remove_and_forget_unchecked(0) }
        };
        // SAFETY: `read` yields the components in bundle order.
        let nested = unsafe { Nested::from_components(&mut read) };
        assert_eq!(nested, (A(7), (B("seven".into()), C)));
    }

    #[test]
    fn registration_is_cached_per_bundle_type() {
        let mut components = Components::default();
        let mut bundles = Bundles::default();
        let ab = bundles.register::<(A, B)>(&mut components);
        let ba = bundles.register::<(B, A)>(&mut components);

        assert_eq!(bundles.register::<(A, B)>(&mut components), ab);
        assert_ne!(ab, ba);
        let (ab, ba) = (
            &bundles.info(ab).component_ids,
            &bundles.info(ba).component_ids,
        );
        assert_eq!([ab[0], ab[1]], [ba[1], ba[0]]);
    }

    #[test]
    #[should_panic(expected = "more than once")]
    fn duplicate_components_are_rejected() {
        Bundles::default().register::<(A, (C, A))>(&mut Components::default());
    }
}
