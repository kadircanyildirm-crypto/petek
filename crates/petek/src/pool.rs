//! Worker threads that a schedule keeps between runs, so that running
//! systems in parallel does not mean spawning threads every time.

use std::any::Any;
use std::mem;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};

/// Threads that call a borrowed function whenever asked to, until the pool
/// is dropped.
pub(crate) struct Pool {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

/// What the pool and its threads share.
struct Shared {
    state: Mutex<State>,
    /// Signalled when a job is posted, or the pool is dropped.
    posted: Condvar,
    /// Signalled when the last thread finishes a job.
    finished: Condvar,
}

struct State {
    /// The function every thread calls for the current job, if there is one.
    job: Option<Job>,
    /// How many jobs have been posted, so that a thread runs each job at most
    /// once.
    jobs: u64,
    /// How many more threads may take the current job.
    slots: usize,
    /// How many threads have yet to finish the current job.
    running: usize,
    /// What a thread panicked with while running the current job, if any.
    panic: Option<Box<dyn Any + Send>>,
    /// Set when the pool is dropped.
    stop: bool,
}

/// A borrowed function, with the lifetime of the borrow erased, so that
/// threads that outlive the borrow can hold it. It is only called while
/// [`Pool::run`] waits for the threads, which keeps the borrow alive.
#[derive(Clone, Copy)]
struct Job(*const (dyn Fn() + Sync));

// SAFETY: the function is `Sync`, so it can be called from any thread, and
// the pointer is only followed while the borrow it came from is alive.
unsafe impl Send for Job {}

impl Pool {
    /// Spawns `threads` threads, which wait for jobs.
    ///
    /// # Panics
    ///
    /// If a thread cannot be spawned.
    pub(crate) fn new(threads: usize) -> Self {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                job: None,
                jobs: 0,
                slots: 0,
                running: 0,
                panic: None,
                stop: false,
            }),
            posted: Condvar::new(),
            finished: Condvar::new(),
        });
        let threads = (0..threads)
            .map(|index| {
                let shared = Arc::clone(&shared);
                thread::Builder::new()
                    .name(format!("petek worker {index}"))
                    .spawn(move || work(&shared))
                    .expect("failed to spawn a worker thread")
            })
            .collect();
        Self { shared, threads }
    }

    /// The number of threads in the pool.
    pub(crate) fn len(&self) -> usize {
        self.threads.len()
    }

    /// Calls `f` on this thread and on up to `helpers` threads of the pool,
    /// and returns once every call has returned. If one of them panics, the
    /// panic is passed on then.
    ///
    /// Only as many threads as can help are woken up, since waking the
    /// others would only cost time.
    #[allow(
        clippy::transmute_ptr_to_ptr,
        reason = "a cast cannot lengthen the lifetime of a trait object"
    )]
    pub(crate) fn run(&self, helpers: usize, f: &(dyn Fn() + Sync)) {
        // SAFETY: this only erases the lifetime of the borrow. The threads
        // follow the pointer until each reports that it has finished the job,
        // and `Wait` waits for that before this function returns or unwinds,
        // so `f` outlives every call.
        let job = Job(unsafe {
            mem::transmute::<*const (dyn Fn() + Sync + '_), *const (dyn Fn() + Sync + 'static)>(f)
        });
        let helpers = helpers.min(self.threads.len());
        {
            let mut state = lock(&self.shared.state);
            debug_assert_eq!(state.running, 0, "the last job is not finished");
            state.job = Some(job);
            state.jobs += 1;
            state.slots = helpers;
            state.running = helpers;
        }
        for _ in 0..helpers {
            self.shared.posted.notify_one();
        }

        let wait = Wait(&self.shared);
        let result = panic::catch_unwind(AssertUnwindSafe(f));
        drop(wait);
        let thread_panic = lock(&self.shared.state).panic.take();
        if let Some(payload) = result.err().or(thread_panic) {
            panic::resume_unwind(payload);
        }
    }
}

/// When dropped, waits until every thread has finished the current job, and
/// forgets the job.
struct Wait<'a>(&'a Shared);

impl Drop for Wait<'_> {
    fn drop(&mut self) {
        let mut state = lock(&self.0.state);
        while state.running > 0 {
            state = self
                .0
                .finished
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.job = None;
    }
}

/// What each thread of a pool does: take jobs as they are posted, while
/// they need more threads, until the pool is dropped.
fn work(shared: &Shared) {
    let mut seen = 0;
    loop {
        let job = {
            let mut state = lock(&shared.state);
            loop {
                if state.stop {
                    return;
                }
                if state.jobs != seen {
                    seen = state.jobs;
                    if state.slots > 0 {
                        state.slots -= 1;
                        break;
                    }
                }
                state = shared
                    .posted
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state
                .job
                .expect("a job is kept until every thread that took it finishes")
        };
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            // SAFETY: `Pool::run` keeps the function alive until this thread
            // reports, below, that it has finished the job.
            unsafe { (*job.0)() };
        }));
        let mut state = lock(&shared.state);
        if let Err(payload) = result {
            state.panic.get_or_insert(payload);
        }
        state.running -= 1;
        if state.running == 0 {
            shared.finished.notify_all();
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.posted.notify_all();
        for thread in self.threads.drain(..) {
            // A thread only ends by returning, since it catches the panics of
            // its jobs.
            let _ = thread.join();
        }
    }
}

/// Locks `mutex`, even if a thread panicked while holding it: no user code
/// runs while the pool's lock is held, so its state stays consistent.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn as_many_threads_as_asked_run_each_job_once() {
        let pool = Pool::new(3);
        let calls = AtomicUsize::new(0);
        let count = || {
            calls.fetch_add(1, Ordering::Relaxed);
        };
        for helpers in [0, 1, 2, 3, 3, 10] {
            calls.store(0, Ordering::Relaxed);
            pool.run(helpers, &count);
            assert_eq!(calls.load(Ordering::Relaxed), 1 + helpers.min(3));
        }
    }

    #[test]
    fn jobs_can_borrow_from_the_caller() {
        let pool = Pool::new(2);
        let mut total = 0;
        for round in 0..5 {
            let values = vec![round; 100];
            let sum = Mutex::new(0);
            pool.run(2, &|| *sum.lock().unwrap() += values.iter().sum::<usize>());
            total += sum.into_inner().unwrap();
        }
        assert_eq!(total, (0..5).map(|round| round * 100 * 3).sum::<usize>());
    }

    #[test]
    fn panics_are_passed_on_and_the_pool_keeps_working() {
        let pool = Pool::new(2);
        let first = AtomicUsize::new(0);
        let result = panic::catch_unwind(AssertUnwindSafe(|| {
            pool.run(2, &|| {
                // One of the three calls panics.
                assert_ne!(first.fetch_add(1, Ordering::Relaxed), 0, "boom");
            });
        }));
        assert!(result.is_err());

        let calls = AtomicUsize::new(0);
        pool.run(2, &|| {
            calls.fetch_add(1, Ordering::Relaxed);
        });
        assert_eq!(calls.load(Ordering::Relaxed), 3);
    }
}
