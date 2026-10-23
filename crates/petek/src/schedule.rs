//! Schedules: lists of systems that run in parallel, with the same result as
//! running them one after another.

use std::any::Any;
use std::collections::VecDeque;
use std::fmt;
use std::num::NonZeroUsize;
use std::ops::Range;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use crate::change::Ticks;
use crate::system::{IntoSystem, System};
use crate::world::{World, WorldId};

/// A list of systems to run on a world, in parallel where their parameters
/// allow.
///
/// Running a schedule always has the same result as running its systems one
/// after another, in the order they were added, however many threads it
/// uses:
///
/// - Systems that conflict, because one of them writes something the other
///   accesses, run in the order they were added. Others may run at the same
///   time, as they cannot affect each other.
/// - Each system runs at its own tick, in the order the systems were added,
///   whichever finishes first.
/// - Systems that take [`Commands`](crate::command::Commands) never run at
///   the same time, so that they reserve entities in the order they were
///   added. Their commands are applied at the end of the schedule, and
///   before each exclusive system, in the order the systems were added.
/// - Exclusive systems, which take `&mut World`, run on their own.
///
/// ```
/// use petek::command::Commands;
/// use petek::query::Query;
/// use petek::schedule::Schedule;
/// use petek::{Component, Entity, World};
///
/// struct Health(u32);
/// impl Component for Health {}
///
/// struct Poisoned;
/// impl Component for Poisoned {}
///
/// fn poison(query: Query<&mut Health, petek::query::With<Poisoned>>) {
///     for mut health in query {
///         health.0 = health.0.saturating_sub(1);
///     }
/// }
///
/// fn bury(query: Query<(Entity, &Health)>, mut commands: Commands) {
///     for (entity, health) in query {
///         if health.0 == 0 {
///             commands.despawn(entity);
///         }
///     }
/// }
///
/// let mut world = World::new();
/// let doomed = world.spawn((Health(1), Poisoned));
/// let fine = world.spawn(Health(1));
///
/// let mut schedule = Schedule::new();
/// schedule.add_system(poison).add_system(bury);
/// schedule.run(&mut world);
/// assert!(!world.contains(doomed) && world.contains(fine));
/// ```
///
/// A schedule belongs to the world it first runs with, and running it with
/// another world panics.
pub struct Schedule {
    systems: Vec<Box<dyn System>>,
    /// The world the systems were initialized with, if any.
    world: Option<WorldId>,
    /// How many systems have been initialized; the rest were added since.
    initialized: usize,
    /// How to run the systems, worked out after they are initialized.
    plan: Vec<Phase>,
    threads: NonZeroUsize,
    /// Whether the last run panicked, leaving behind what systems deferred
    /// during a frame that never finished.
    interrupted: bool,
}

/// A part of a schedule's run.
enum Phase {
    /// Systems that run on a shared world, in parallel where they do not
    /// conflict.
    Shared(SharedPhase),
    /// A system that runs with exclusive access to the world.
    Exclusive(usize),
}

/// Systems that run on a shared world, and the order they must keep.
struct SharedPhase {
    systems: Range<usize>,
    /// For each system of the phase, how many earlier systems of the phase
    /// it conflicts with, and so must wait for.
    waits_for: Vec<usize>,
    /// For each system of the phase, the later systems of the phase that
    /// conflict with it, and so wait for it.
    blocks: Vec<Vec<usize>>,
    /// For each system of the phase, whether it reads the whole world, and so
    /// can see the world's tick.
    reads_world: Vec<bool>,
}

impl Schedule {
    /// Creates an empty schedule, which runs on as many threads as the
    /// machine has.
    #[must_use]
    pub fn new() -> Self {
        Self {
            systems: Vec::new(),
            world: None,
            initialized: 0,
            plan: Vec::new(),
            threads: thread::available_parallelism().unwrap_or(NonZeroUsize::MIN),
            interrupted: false,
        }
    }

    /// Adds a system, which runs after the systems added before it that it
    /// conflicts with.
    pub fn add_system<M>(&mut self, system: impl IntoSystem<M>) -> &mut Self {
        self.systems.push(system.into_system());
        self
    }

    /// Sets how many threads the schedule may run systems on, the calling
    /// thread included. With one, systems run one after another on the
    /// calling thread.
    ///
    /// # Panics
    ///
    /// If `threads` is zero.
    pub fn set_threads(&mut self, threads: usize) -> &mut Self {
        self.threads = NonZeroUsize::new(threads).expect("a schedule needs at least one thread");
        self
    }

    /// The number of systems in the schedule.
    #[must_use]
    pub fn len(&self) -> usize {
        self.systems.len()
    }

    /// Returns `true` if the schedule has no systems.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.systems.is_empty()
    }

    /// Runs every system once, then applies their commands.
    ///
    /// # Panics
    ///
    /// If the schedule ran with another world before, if the parameters of
    /// a system conflict, or if a system or a command panics.
    ///
    /// In the last case, which other systems ran is unspecified, but the
    /// world stays consistent, and the systems that finished remember
    /// running. The commands recorded during that run are dropped without
    /// being applied the next time the schedule runs: they belong to a frame
    /// that never finished, and a game that recovers by restoring a snapshot
    /// should not see them applied to the next one.
    #[track_caller]
    pub fn run(&mut self, world: &mut World) {
        self.initialize(world);
        if self.interrupted {
            for system in &mut self.systems {
                system.discard_deferred();
            }
        }
        self.interrupted = true;
        // Commands are applied in the order of the systems that recorded
        // them, up to the next exclusive system or the end.
        let mut applied = 0;
        for phase in &self.plan {
            match phase {
                Phase::Shared(phase) => {
                    run_shared(&mut self.systems, phase, world, self.threads.get());
                }
                &Phase::Exclusive(index) => {
                    apply_deferred(&mut self.systems[applied..index], world);
                    applied = index;
                    self.systems[index].run_exclusive(world);
                }
            }
        }
        apply_deferred(&mut self.systems[applied..], world);
        self.interrupted = false;
    }

    /// Initializes the systems added since the last run, and plans how to
    /// run them all.
    #[track_caller]
    fn initialize(&mut self, world: &mut World) {
        let id = *self.world.get_or_insert(world.id());
        assert!(
            id == world.id(),
            "a `Schedule` was run with a world other than the one it first ran with",
        );
        if self.initialized == self.systems.len() {
            return;
        }
        for system in &mut self.systems[self.initialized..] {
            system.initialize(world);
        }
        self.initialized = self.systems.len();
        self.plan = plan(&self.systems);
    }
}

/// Splits `systems` into phases, and works out which systems of each shared
/// phase must wait for which.
fn plan(systems: &[Box<dyn System>]) -> Vec<Phase> {
    let mut plan = Vec::new();
    let mut start = 0;
    for (index, system) in systems.iter().enumerate() {
        if system.access().is_none() {
            if start < index {
                plan.push(Phase::Shared(plan_shared(systems, start..index)));
            }
            plan.push(Phase::Exclusive(index));
            start = index + 1;
        }
    }
    if start < systems.len() {
        plan.push(Phase::Shared(plan_shared(systems, start..systems.len())));
    }
    plan
}

/// Works out which of the given systems, none of them exclusive, must wait
/// for which: each for every earlier one it conflicts with.
fn plan_shared(systems: &[Box<dyn System>], range: Range<usize>) -> SharedPhase {
    let access = |index: usize| {
        systems[range.start + index]
            .access()
            .expect("shared phases hold no exclusive systems")
    };
    let len = range.len();
    let mut waits_for = vec![0; len];
    let mut blocks = vec![Vec::new(); len];
    for (later, waits) in waits_for.iter_mut().enumerate() {
        for (earlier, blocked) in blocks[..later].iter_mut().enumerate() {
            if access(earlier).conflicts_with(access(later)) {
                *waits += 1;
                blocked.push(later);
            }
        }
    }
    let reads_world = (0..len).map(|index| access(index).reads_world()).collect();
    SharedPhase {
        systems: range,
        waits_for,
        blocks,
        reads_world,
    }
}

/// Runs the systems of a shared phase, on up to `threads` threads.
fn run_shared(
    systems: &mut [Box<dyn System>],
    phase: &SharedPhase,
    world: &mut World,
    threads: usize,
) {
    let systems = &mut systems[phase.systems.clone()];
    let len = systems.len();
    // Each system runs at its own tick, in the order the systems were added,
    // as if they ran one after another.
    let start = world.advance_change_tick(len);
    let ticks: Vec<_> = (1..)
        .zip(&*systems)
        .map(|(offset, system)| {
            let slot = system.tick_slot().expect("shared systems have ticks");
            Ticks {
                last_run: world.system_last_run(slot),
                this_run: start.after(offset),
            }
        })
        .collect();

    let world_ref: &World = world;
    let outcome = if threads == 1 || len == 1 {
        run_one_by_one(systems, phase, world_ref, &ticks)
    } else {
        run_parallel(systems, phase, world_ref, &ticks, threads)
    };

    // Whether or not a system panicked, the world's tick ends up past every
    // system of the phase, and the systems that finished remember running.
    world.set_change_tick(start.after(len));
    for ((system, ticks), &finished) in systems.iter().zip(ticks).zip(&outcome.finished) {
        if finished {
            let slot = system.tick_slot().expect("shared systems have ticks");
            world.set_system_last_run(slot, ticks.this_run);
        }
    }
    if let Some(payload) = outcome.panic {
        panic::resume_unwind(payload);
    }
}

/// How the run of a shared phase went.
struct Outcome {
    /// For each system, whether it finished.
    finished: Vec<bool>,
    /// What the first system to panic panicked with.
    panic: Option<Box<dyn Any + Send>>,
}

/// Runs one system of a shared phase.
///
/// # Safety
///
/// The system must have been initialized with `world`, the caller must hold
/// the access it needs while it runs, and `ticks` must be those `run_shared`
/// worked out for it. If it reads the whole world, no other system that does
/// may run at the same time.
unsafe fn run_system(system: &mut dyn System, world: &World, ticks: Ticks, reads_world: bool) {
    if reads_world {
        // It can read the world's tick, which should be its own, as if the
        // systems ran one after another. No other system that can read the
        // tick runs at the same time.
        world.show_system_tick(ticks.this_run);
    }
    // SAFETY: the caller upholds `run_unsafe`'s contract.
    unsafe { system.run_unsafe(world, ticks) };
}

/// Runs the systems of a shared phase one after another, on this thread.
fn run_one_by_one(
    systems: &mut [Box<dyn System>],
    phase: &SharedPhase,
    world: &World,
    ticks: &[Ticks],
) -> Outcome {
    let mut finished = vec![false; systems.len()];
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        for (index, system) in systems.iter_mut().enumerate() {
            // SAFETY: the systems were initialized with this world, and run
            // one at a time, so each has the access it needs.
            unsafe { run_system(&mut **system, world, ticks[index], phase.reads_world[index]) };
            finished[index] = true;
        }
    }));
    Outcome {
        finished,
        panic: result.err(),
    }
}

/// How far a parallel run of a shared phase has got.
struct Progress {
    /// Systems whose turn it is, which are not running yet.
    ready: VecDeque<usize>,
    /// For each system, how many systems it still waits for.
    waits_for: Vec<usize>,
    /// How many systems are left to finish.
    left: usize,
    outcome: Outcome,
}

/// Runs the systems of a shared phase on up to `threads` threads, each
/// system once all the systems it waits for have finished.
fn run_parallel(
    systems: &mut [Box<dyn System>],
    phase: &SharedPhase,
    world: &World,
    ticks: &[Ticks],
    threads: usize,
) -> Outcome {
    let len = systems.len();
    // One lock per system lets the threads share the list, while each system
    // is only ever run by one thread at a time.
    let systems: Vec<Mutex<&mut Box<dyn System>>> = systems.iter_mut().map(Mutex::new).collect();
    let progress = Mutex::new(Progress {
        ready: (0..len)
            .filter(|&index| phase.waits_for[index] == 0)
            .collect(),
        waits_for: phase.waits_for.clone(),
        left: len,
        outcome: Outcome {
            finished: vec![false; len],
            panic: None,
        },
    });
    let changed = Condvar::new();

    let work = || {
        loop {
            let index = {
                let mut progress = lock(&progress);
                loop {
                    if progress.outcome.panic.is_some() || progress.left == 0 {
                        return;
                    }
                    if let Some(index) = progress.ready.pop_front() {
                        break index;
                    }
                    progress = changed
                        .wait(progress)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            let result = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut system = lock(&systems[index]);
                // SAFETY: the systems were initialized with this world. A
                // system only becomes ready once every earlier system it
                // conflicts with has finished, and no later system it
                // conflicts with starts before it finishes, so it has the
                // access it needs. Systems that read the whole world conflict
                // with each other.
                unsafe {
                    run_system(
                        &mut ***system,
                        world,
                        ticks[index],
                        phase.reads_world[index],
                    );
                };
            }));
            let mut progress = lock(&progress);
            match result {
                Ok(()) => {
                    progress.left -= 1;
                    progress.outcome.finished[index] = true;
                    for &later in &phase.blocks[index] {
                        progress.waits_for[later] -= 1;
                        if progress.waits_for[later] == 0 {
                            progress.ready.push_back(later);
                        }
                    }
                }
                Err(payload) => {
                    progress.outcome.panic.get_or_insert(payload);
                }
            }
            drop(progress);
            changed.notify_all();
        }
    };

    // `work` only captures references, so each thread gets a copy of it.
    thread::scope(|scope| {
        for _ in 1..threads.min(len) {
            scope.spawn(work);
        }
        work();
    });

    progress
        .into_inner()
        .unwrap_or_else(PoisonError::into_inner)
        .outcome
}

/// Locks `mutex`, even if a thread panicked while holding it: the data the
/// schedule protects stays consistent, since no lock is held while user code
/// runs, except for the system being run, which is not run again.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Applies what `systems` deferred, in order.
fn apply_deferred(systems: &mut [Box<dyn System>], world: &mut World) {
    for system in systems {
        system.apply_deferred(world);
    }
}

impl Default for Schedule {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for Schedule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Schedule")
            .field(
                "systems",
                &self
                    .systems
                    .iter()
                    .map(|system| system.name())
                    .collect::<Vec<_>>(),
            )
            .field("threads", &self.threads)
            .finish_non_exhaustive()
    }
}
