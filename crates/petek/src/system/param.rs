//! System parameters: what a system asks for, and is handed on every run.

use std::any::type_name;
use std::fmt;
use std::ops::{Deref, DerefMut};

use super::access::{SystemAccess, SystemMeta};
use super::sealed;
use crate::change::{Mut, Tick, Ticks};
use crate::command::{CommandQueue, Commands};
use crate::query::{Access, Query, QueryData, QueryFilter, QueryState};
use crate::resource::Resource;
use crate::world::World;

/// What a system is handed along with its parameters on every run.
///
/// This is only `pub` because the hidden methods of [`SystemParam`] mention
/// it.
#[derive(Clone, Copy, Debug)]
pub struct SystemRun {
    /// The system's name, for diagnostics.
    pub(crate) name: &'static str,
    /// When the system last ran, and the tick of this run.
    pub(crate) ticks: Ticks,
}

/// Something a system can take as a parameter.
///
/// | Parameter                  | Gives the system                                   |
/// | -------------------------- | -------------------------------------------------- |
/// | [`Query<D, F>`](Query)     | the components of every entity that matches        |
/// | [`Res<R>`]                 | shared access to the resource `R`                  |
/// | [`ResMut<R>`]              | exclusive access to the resource `R`               |
/// | `Option<Res<R>>`, `Option<ResMut<R>>` | the same, or `None` if there is no `R`  |
/// | [`Commands`]               | a way to record structural changes                 |
/// | `&World`                   | shared access to the whole world                   |
/// | `(P1, P2, ..)`             | each `Pi`                                          |
///
/// Tuples of up to 12 parameters are supported, and they nest.
///
/// A system whose parameters conflict, because one of them writes something
/// that another accesses, panics when it is first run: using both at once
/// could alias a mutable reference. Queries count as accessing every
/// component they name, whichever entities they match.
///
/// This trait is sealed, because the soundness of the scheduler relies on its
/// implementations: it cannot be implemented outside of `petek`.
pub trait SystemParam: sealed::Param {
    /// What the parameter keeps between runs, such as a query's state.
    #[doc(hidden)]
    type State: Send + 'static;

    /// The parameter itself, borrowing the world for `'w`.
    #[doc(hidden)]
    type Item<'w>;

    /// Resolves the state, and records in `meta` what the parameter accesses.
    #[doc(hidden)]
    fn init_state(world: &mut World, meta: &mut SystemMeta) -> Self::State;

    /// Hands out the parameter for one run of the system.
    ///
    /// # Safety
    ///
    /// - `state` must come from [`init_state`](Self::init_state) with
    ///   `world`.
    /// - For `'w`, the caller must hold the access that `init_state`
    ///   recorded: no one else may write what the parameter reads, or access
    ///   what it writes.
    #[doc(hidden)]
    unsafe fn get_param<'w>(
        state: &'w mut Self::State,
        world: &'w World,
        run: SystemRun,
    ) -> Self::Item<'w>;

    /// Applies what the parameter deferred, such as commands, to the world.
    #[doc(hidden)]
    fn apply(state: &mut Self::State, world: &mut World) {
        let _ = (state, world);
    }

    /// Drops what the parameter deferred, without applying it.
    #[doc(hidden)]
    fn discard(state: &mut Self::State) {
        let _ = state;
    }
}

/// The item a parameter `P` hands to a system that borrows the world for
/// `'w`.
pub(crate) type SystemParamItem<'w, P> = <P as SystemParam>::Item<'w>;

impl<D: QueryData + 'static, F: QueryFilter + 'static> sealed::Param for Query<'_, D, F> {}

impl<D: QueryData + 'static, F: QueryFilter + 'static> SystemParam for Query<'_, D, F> {
    type State = QueryState<D, F>;
    type Item<'w> = Query<'w, D, F>;

    fn init_state(world: &mut World, meta: &mut SystemMeta) -> Self::State {
        let state = QueryState::new(world);
        let mut access = SystemAccess::default();
        state.visit_access(&mut |id, how| access.add_component(id, how));
        meta.add_param(&access, world);
        state
    }

    unsafe fn get_param<'w>(
        state: &'w mut Self::State,
        world: &'w World,
        run: SystemRun,
    ) -> Self::Item<'w> {
        // SAFETY: the caller guarantees that the state comes from `world`,
        // and that it holds the access the state reports.
        unsafe { state.query_unchecked(world, run.ticks) }
    }
}

/// Shared access to a resource, as a system parameter.
///
/// It dereferences to the resource, and can tell whether the resource was
/// added or changed since the system last ran.
///
/// A system that asks for a `Res<R>` panics if the world holds no `R`. Ask for
/// an `Option<Res<R>>` instead if the resource may be missing.
pub struct Res<'w, R: Resource> {
    value: &'w R,
    added: Tick,
    changed: Tick,
    last_run: Tick,
}

impl<'w, R: Resource> Res<'w, R> {
    /// Returns `true` if the resource was added since the system last ran.
    #[must_use]
    pub fn is_added(&self) -> bool {
        self.added > self.last_run
    }

    /// Returns `true` if the resource was added or changed since the system
    /// last ran.
    #[must_use]
    pub fn is_changed(&self) -> bool {
        self.changed > self.last_run
    }

    /// Returns the underlying reference.
    #[must_use]
    pub fn into_inner(self) -> &'w R {
        self.value
    }
}

impl<R: Resource> Deref for Res<'_, R> {
    type Target = R;

    #[inline]
    fn deref(&self) -> &R {
        self.value
    }
}

impl<R: Resource> Clone for Res<'_, R> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<R: Resource> Copy for Res<'_, R> {}

impl<R: Resource + fmt::Debug> fmt::Debug for Res<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Res").field(&self.value).finish()
    }
}

/// Exclusive access to a resource, as a system parameter.
///
/// Like a [`Mut`], it dereferences to the resource, and writing through it
/// marks the resource as changed.
///
/// A system that asks for a `ResMut<R>` panics if the world holds no `R`. Ask
/// for an `Option<ResMut<R>>` instead if the resource may be missing.
pub struct ResMut<'w, R: Resource>(Mut<'w, R>);

impl<'w, R: Resource> ResMut<'w, R> {
    /// Returns `true` if the resource was added since the system last ran.
    #[must_use]
    pub fn is_added(&self) -> bool {
        self.0.is_added()
    }

    /// Returns `true` if the resource was added or changed since the system
    /// last ran, including through this `ResMut`.
    #[must_use]
    pub fn is_changed(&self) -> bool {
        self.0.is_changed()
    }

    /// Returns the underlying reference, marking the resource as changed.
    #[must_use]
    pub fn into_inner(self) -> &'w mut R {
        self.0.into_inner()
    }
}

impl<R: Resource> Deref for ResMut<'_, R> {
    type Target = R;

    #[inline]
    fn deref(&self) -> &R {
        &self.0
    }
}

impl<R: Resource> DerefMut for ResMut<'_, R> {
    #[inline]
    fn deref_mut(&mut self) -> &mut R {
        &mut self.0
    }
}

impl<R: Resource + fmt::Debug> fmt::Debug for ResMut<'_, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ResMut").field(&*self.0).finish()
    }
}

/// Records that a system accesses the resource `R` as `how`, and returns its
/// slot.
fn init_resource<R: Resource>(world: &mut World, meta: &mut SystemMeta, how: Access) -> usize {
    let slot = world.resources_mut().slot_or_insert::<R>();
    let mut access = SystemAccess::default();
    access.add_resource(slot, how);
    meta.add_param(&access, world);
    slot
}

/// Hands out the resource in `slot`, if there is one, for reading.
fn get_res<R: Resource>(slot: usize, world: &World, run: SystemRun) -> Option<Res<'_, R>> {
    let stored = world.resources().read(slot)?;
    Some(Res {
        value: stored.downcast_ref(),
        added: stored.added(),
        changed: stored.changed(),
        last_run: run.ticks.last_run,
    })
}

/// Hands out the resource in `slot`, if there is one, for writing.
///
/// # Safety
///
/// While the returned resource lives, no one else may access it.
unsafe fn get_res_mut<R: Resource>(
    slot: usize,
    world: &World,
    run: SystemRun,
) -> Option<ResMut<'_, R>> {
    // SAFETY: the caller guarantees that no one else accesses the resource.
    let stored = unsafe { world.resources().get_unchecked_mut(slot) }?;
    Some(ResMut(stored.downcast_mut(run.ticks)))
}

/// Panics because the system `run` needs a resource the world lacks.
#[cold]
fn missing<R: Resource>(run: SystemRun) -> ! {
    panic!(
        "system `{}` needs a `{}` resource, which the world does not hold",
        run.name,
        type_name::<R>(),
    );
}

impl<R: Resource> sealed::Param for Res<'_, R> {}

impl<R: Resource> SystemParam for Res<'_, R> {
    /// The resource's slot.
    type State = usize;
    type Item<'w> = Res<'w, R>;

    fn init_state(world: &mut World, meta: &mut SystemMeta) -> usize {
        init_resource::<R>(world, meta, Access::Read)
    }

    unsafe fn get_param<'w>(slot: &'w mut usize, world: &'w World, run: SystemRun) -> Res<'w, R> {
        get_res(*slot, world, run).unwrap_or_else(|| missing::<R>(run))
    }
}

impl<R: Resource> sealed::Param for Option<Res<'_, R>> {}

impl<R: Resource> SystemParam for Option<Res<'_, R>> {
    /// The resource's slot.
    type State = usize;
    type Item<'w> = Option<Res<'w, R>>;

    fn init_state(world: &mut World, meta: &mut SystemMeta) -> usize {
        init_resource::<R>(world, meta, Access::Read)
    }

    unsafe fn get_param<'w>(
        slot: &'w mut usize,
        world: &'w World,
        run: SystemRun,
    ) -> Option<Res<'w, R>> {
        get_res(*slot, world, run)
    }
}

impl<R: Resource> sealed::Param for ResMut<'_, R> {}

impl<R: Resource> SystemParam for ResMut<'_, R> {
    /// The resource's slot.
    type State = usize;
    type Item<'w> = ResMut<'w, R>;

    fn init_state(world: &mut World, meta: &mut SystemMeta) -> usize {
        init_resource::<R>(world, meta, Access::Write)
    }

    unsafe fn get_param<'w>(
        slot: &'w mut usize,
        world: &'w World,
        run: SystemRun,
    ) -> ResMut<'w, R> {
        // SAFETY: `init_state` recorded exclusive access to the resource,
        // which the caller holds.
        unsafe { get_res_mut(*slot, world, run) }.unwrap_or_else(|| missing::<R>(run))
    }
}

impl<R: Resource> sealed::Param for Option<ResMut<'_, R>> {}

impl<R: Resource> SystemParam for Option<ResMut<'_, R>> {
    /// The resource's slot.
    type State = usize;
    type Item<'w> = Option<ResMut<'w, R>>;

    fn init_state(world: &mut World, meta: &mut SystemMeta) -> usize {
        init_resource::<R>(world, meta, Access::Write)
    }

    unsafe fn get_param<'w>(
        slot: &'w mut usize,
        world: &'w World,
        run: SystemRun,
    ) -> Option<ResMut<'w, R>> {
        // SAFETY: as for `ResMut`.
        unsafe { get_res_mut(*slot, world, run) }
    }
}

impl sealed::Param for Commands<'_> {}

/// Commands are recorded into a queue of the system's own, which is applied
/// after the system runs; see [`Schedule`](crate::schedule::Schedule).
impl SystemParam for Commands<'_> {
    type State = CommandQueue;
    type Item<'w> = Commands<'w>;

    fn init_state(world: &mut World, meta: &mut SystemMeta) -> CommandQueue {
        let mut access = SystemAccess::default();
        access.reserve_entities();
        meta.add_param(&access, world);
        CommandQueue::new()
    }

    unsafe fn get_param<'w>(
        queue: &'w mut CommandQueue,
        world: &'w World,
        _: SystemRun,
    ) -> Commands<'w> {
        queue.commands(world)
    }

    fn apply(queue: &mut CommandQueue, world: &mut World) {
        queue.apply(world);
    }

    fn discard(queue: &mut CommandQueue) {
        queue.clear();
    }
}

impl sealed::Param for &World {}

/// Reading the whole world conflicts with every system that writes
/// something. It also counts as reserving entities, since the world hands out
/// [`CommandQueue::commands`].
impl SystemParam for &World {
    type State = ();
    type Item<'w> = &'w World;

    fn init_state(world: &mut World, meta: &mut SystemMeta) {
        let mut access = SystemAccess::default();
        access.read_world();
        access.reserve_entities();
        meta.add_param(&access, world);
    }

    unsafe fn get_param<'w>((): &'w mut (), world: &'w World, _: SystemRun) -> &'w World {
        world
    }
}

impl sealed::Param for () {}

impl SystemParam for () {
    type State = ();
    type Item<'w> = ();

    fn init_state(_: &mut World, _: &mut SystemMeta) {}

    unsafe fn get_param<'w>((): &'w mut (), _: &'w World, _: SystemRun) {}
}

macro_rules! impl_system_param_for_tuples {
    () => {};
    ($first:ident $(, $rest:ident)*) => {
        impl<$first: SystemParam $(, $rest: SystemParam)*> sealed::Param for ($first, $($rest,)*) {}

        #[allow(non_snake_case, reason = "each element is named after its type")]
        impl<$first: SystemParam $(, $rest: SystemParam)*> SystemParam for ($first, $($rest,)*) {
            type State = ($first::State, $($rest::State,)*);
            type Item<'w> = ($first::Item<'w>, $($rest::Item<'w>,)*);

            fn init_state(world: &mut World, meta: &mut SystemMeta) -> Self::State {
                ($first::init_state(world, meta), $($rest::init_state(world, meta),)*)
            }

            unsafe fn get_param<'w>(
                state: &'w mut Self::State,
                world: &'w World,
                run: SystemRun,
            ) -> Self::Item<'w> {
                let ($first, $($rest,)*) = state;
                // SAFETY: each element's state comes from `init_state` with
                // `world`, and the tuple recorded the access of all of them,
                // which the caller holds.
                unsafe {
                    (
                        $first::get_param($first, world, run),
                        $($rest::get_param($rest, world, run),)*
                    )
                }
            }

            fn apply(state: &mut Self::State, world: &mut World) {
                let ($first, $($rest,)*) = state;
                $first::apply($first, world);
                $($rest::apply($rest, world);)*
            }

            fn discard(state: &mut Self::State) {
                let ($first, $($rest,)*) = state;
                $first::discard($first);
                $($rest::discard($rest);)*
            }
        }

        impl_system_param_for_tuples!($($rest),*);
    };
}

impl_system_param_for_tuples!(P0, P1, P2, P3, P4, P5, P6, P7, P8, P9, P10, P11);
