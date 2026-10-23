//! Systems: functions that run on a world, given what they ask for.
//!
//! A system is a function whose parameters say what it reads and writes,
//! such as a [`Query`](crate::query::Query) or a [`Res`]. A
//! [`Schedule`](crate::schedule::Schedule) runs systems, in parallel when
//! their parameters allow, and with the same result as running them one
//! after another:
//!
//! ```
//! use petek::schedule::Schedule;
//! use petek::query::Query;
//! use petek::system::Res;
//! use petek::{Component, Resource, World};
//!
//! struct Position(f32);
//! impl Component for Position {}
//!
//! struct Velocity(f32);
//! impl Component for Velocity {}
//!
//! struct TimeStep(f32);
//! impl Resource for TimeStep {}
//!
//! fn movement(step: Res<TimeStep>, query: Query<(&mut Position, &Velocity)>) {
//!     for (mut position, velocity) in query {
//!         position.0 += velocity.0 * step.0;
//!     }
//! }
//!
//! let mut world = World::new();
//! world.insert_resource(TimeStep(0.5));
//! let ball = world.spawn((Position(0.0), Velocity(2.0)));
//!
//! let mut schedule = Schedule::new();
//! schedule.add_system(movement);
//! schedule.run(&mut world);
//! assert_eq!(world.get::<Position>(ball).unwrap().0, 1.0);
//! ```
//!
//! A function that takes `&mut World` is an *exclusive* system: it can do
//! anything, such as spawning entities directly, but never runs alongside
//! other systems.
//!
//! The change detection of a system, such as its
//! [`Changed`](crate::query::Changed) filters, compares with the last time
//! that system ran. That is part of the world, so restoring a
//! [snapshot](crate::World::snapshot) rewinds it too.

use std::any::type_name;
use std::fmt;
use std::marker::PhantomData;

use crate::change::Ticks;
use crate::world::{World, WorldId};

mod access;
mod param;

pub use param::{Res, ResMut, SystemParam};

use access::{SystemAccess, SystemMeta};
use param::{SystemParamItem, SystemRun};

/// Keeps the system traits from being implemented outside of this crate,
/// since the soundness of the scheduler relies on their implementations.
mod sealed {
    /// Implemented by every [`SystemParam`](super::SystemParam).
    pub trait Param {}
    /// Implemented by every [`System`](super::System).
    pub trait System {}
}

/// A system, ready to be run by a [`Schedule`](crate::schedule::Schedule).
///
/// This is only `pub` because [`IntoSystem`] mentions it. It is sealed: a
/// system that misreported what it accesses could race with the systems
/// running alongside it, so it cannot be implemented outside of `petek`.
///
/// ```compile_fail,E0277
/// struct Lying;
///
/// impl petek::system::System for Lying {}
/// ```
pub trait System: sealed::System + Send + 'static {
    /// The system's name, for diagnostics.
    fn name(&self) -> &'static str;

    /// Prepares the system to run on `world`, if it has not been already.
    ///
    /// # Panics
    ///
    /// If the system was prepared for another world, or if its parameters
    /// conflict.
    fn initialize(&mut self, world: &mut World);

    /// What the system accesses, once it is initialized, or `None` if it is
    /// exclusive.
    fn access(&self) -> Option<&SystemAccess>;

    /// Where the world keeps the system's ticks, once it is initialized, or
    /// `None` if it is exclusive.
    fn tick_slot(&self) -> Option<usize>;

    /// Runs the system, which must not be exclusive, on a shared world.
    ///
    /// # Safety
    ///
    /// - The system must have been initialized with `world`.
    /// - While it runs, the caller must hold the access that
    ///   [`access`](Self::access) reports: no one else may write what the
    ///   system reads, or access what it writes. If the system reserves
    ///   entities, no one else may either.
    /// - `ticks.last_run` must be the tick at which the system last ran, and
    ///   `ticks.this_run` a tick newer than any in the world.
    unsafe fn run_unsafe(&mut self, world: &World, ticks: Ticks);

    /// Runs the system, which must be exclusive.
    fn run_exclusive(&mut self, world: &mut World);

    /// Applies what the system deferred, such as its commands.
    fn apply_deferred(&mut self, world: &mut World);

    /// Drops what the system deferred, without applying it.
    fn discard_deferred(&mut self);
}

/// Something that can be turned into a [`System`]: a function whose
/// parameters are all [`SystemParam`]s, or that takes `&mut World`.
///
/// `Marker` only tells the two kinds of functions apart; it is inferred.
pub trait IntoSystem<Marker>: Sized {
    /// Turns this into a system.
    #[doc(hidden)]
    fn into_system(self) -> Box<dyn System>;
}

/// Tells functions that take [`SystemParam`]s apart, as an [`IntoSystem`]
/// marker.
#[doc(hidden)]
#[derive(Debug)]
pub struct FunctionMarker;

/// Tells functions that take `&mut World` apart, as an [`IntoSystem`] marker.
#[doc(hidden)]
#[derive(Debug)]
pub struct ExclusiveMarker;

/// A function that can be called with the items of the parameters `P`.
///
/// It is implemented for functions of up to 12 parameters, for each of which
/// `P` is the tuple of their types.
#[doc(hidden)]
pub trait SystemParamFunction<P: SystemParam>: Send + 'static {
    /// Calls the function with the items of its parameters.
    fn call(&mut self, params: SystemParamItem<'_, P>);
}

impl<F, P> IntoSystem<(FunctionMarker, P)> for F
where
    F: SystemParamFunction<P>,
    P: SystemParam + 'static,
{
    fn into_system(self) -> Box<dyn System> {
        Box::new(FunctionSystem::<F, P> {
            function: self,
            state: None,
            params: PhantomData,
        })
    }
}

impl<F> IntoSystem<ExclusiveMarker> for F
where
    F: FnMut(&mut World) + Send + 'static,
{
    fn into_system(self) -> Box<dyn System> {
        Box::new(ExclusiveSystem { function: self })
    }
}

/// A system made from a function that takes [`SystemParam`]s.
struct FunctionSystem<F, P: SystemParam> {
    function: F,
    /// `None` until the system is initialized.
    state: Option<FunctionState<P>>,
    params: PhantomData<fn() -> P>,
}

impl<F, P: SystemParam> sealed::System for FunctionSystem<F, P> {}

/// What a [`FunctionSystem`] keeps once it is initialized.
struct FunctionState<P: SystemParam> {
    world: WorldId,
    params: P::State,
    meta: SystemMeta,
    tick_slot: usize,
}

impl<F, P> System for FunctionSystem<F, P>
where
    F: SystemParamFunction<P>,
    P: SystemParam + 'static,
{
    fn name(&self) -> &'static str {
        type_name::<F>()
    }

    #[track_caller]
    fn initialize(&mut self, world: &mut World) {
        if let Some(state) = &self.state {
            assert!(
                state.world == world.id(),
                "system `{}` was run with a world other than the one it was first run with",
                self.name(),
            );
            return;
        }
        let mut meta = SystemMeta::new(self.name());
        let params = P::init_state(world, &mut meta);
        self.state = Some(FunctionState {
            world: world.id(),
            params,
            meta,
            tick_slot: world.add_system_ticks(),
        });
    }

    fn access(&self) -> Option<&SystemAccess> {
        Some(self.state.as_ref()?.meta.access())
    }

    fn tick_slot(&self) -> Option<usize> {
        Some(self.state.as_ref()?.tick_slot)
    }

    unsafe fn run_unsafe(&mut self, world: &World, ticks: Ticks) {
        let state = self
            .state
            .as_mut()
            .expect("the system is initialized before it runs");
        debug_assert_eq!(state.world, world.id());
        let run = SystemRun {
            name: state.meta.name(),
            ticks,
        };
        // SAFETY: the caller guarantees that the system was initialized with
        // `world`, so the state comes from it, and that it holds the access
        // the parameters recorded.
        let params = unsafe { P::get_param(&mut state.params, world, run) };
        self.function.call(params);
    }

    fn run_exclusive(&mut self, _: &mut World) {
        unreachable!("a function system is not exclusive");
    }

    fn apply_deferred(&mut self, world: &mut World) {
        if let Some(state) = &mut self.state {
            P::apply(&mut state.params, world);
        }
    }

    fn discard_deferred(&mut self) {
        if let Some(state) = &mut self.state {
            P::discard(&mut state.params);
        }
    }
}

/// A system made from a function that takes `&mut World`.
struct ExclusiveSystem<F> {
    function: F,
}

impl<F> sealed::System for ExclusiveSystem<F> {}

impl<F: FnMut(&mut World) + Send + 'static> System for ExclusiveSystem<F> {
    fn name(&self) -> &'static str {
        type_name::<F>()
    }

    fn initialize(&mut self, _: &mut World) {}

    fn access(&self) -> Option<&SystemAccess> {
        None
    }

    fn tick_slot(&self) -> Option<usize> {
        None
    }

    unsafe fn run_unsafe(&mut self, _: &World, _: Ticks) {
        unreachable!("an exclusive system needs exclusive access to the world");
    }

    fn run_exclusive(&mut self, world: &mut World) {
        (self.function)(world);
    }

    fn apply_deferred(&mut self, _: &mut World) {}

    fn discard_deferred(&mut self) {}
}

impl fmt::Debug for dyn System {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("System")
            .field("name", &self.name())
            .finish_non_exhaustive()
    }
}

/// Calls `f` with `params`. Going through a function generic over the
/// parameter types lets the compiler pick the right `FnMut` implementation.
macro_rules! impl_system_param_function {
    ($($param:ident),*) => {
        #[allow(non_snake_case, reason = "each parameter is named after its type")]
        impl<F, $($param: SystemParam),*> SystemParamFunction<($($param,)*)> for F
        where
            F: Send + 'static,
            for<'a> &'a mut F: FnMut($($param),*) + FnMut($(SystemParamItem<'_, $param>),*),
        {
            fn call(&mut self, params: SystemParamItem<'_, ($($param,)*)>) {
                #[allow(clippy::too_many_arguments, reason = "one per system parameter")]
                fn call_inner<$($param),*>(mut f: impl FnMut($($param),*), $($param: $param),*) {
                    f($($param),*);
                }
                let ($($param,)*) = params;
                call_inner(self, $($param),*);
            }
        }
    };
}

impl_system_param_function!();
impl_system_param_function!(P0);
impl_system_param_function!(P0, P1);
impl_system_param_function!(P0, P1, P2);
impl_system_param_function!(P0, P1, P2, P3);
impl_system_param_function!(P0, P1, P2, P3, P4);
impl_system_param_function!(P0, P1, P2, P3, P4, P5);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7, P8);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10);
impl_system_param_function!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
