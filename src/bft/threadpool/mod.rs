//! A thread pool abstraction over a range of other crates. 

#[cfg(feature = "threadpool_crossbeam")]
mod crossbeam;

#[cfg(feature = "threadpool_cthpool")]
mod cthpool;

#[cfg(feature = "threadpool_rayon")]
mod rayon;

use std::convert::TryInto;
use std::sync::{Arc, Barrier};
use thread_priority::{ThreadPriority, ThreadPriorityValue};
use crate::bft::globals::Global;
use crate::bft::error::*;

/// A thread pool type, used to run intensive CPU tasks.
///
/// The thread pool implements `Clone` with a cheap reference
/// count increase operation. This means that if we drop its
/// handle, the thread pool can continue to be used, as long
/// as at least another instance of the original pool remains.
//#[derive(Clone)]
pub struct ThreadPool {
    #[cfg(feature = "threadpool_crossbeam")]
    inner: crossbeam::ThreadPool,

    #[cfg(feature = "threadpool_cthpool")]
    inner: cthpool::ThreadPool,

    #[cfg(feature = "threadpool_rayon")]
    inner: rayon::ThreadPool,
}

/// Helper type used to construct a new thread pool.
pub struct Builder {
    #[cfg(feature = "threadpool_crossbeam")]
    inner: crossbeam::Builder,

    #[cfg(feature = "threadpool_cthpool")]
    inner: cthpool::Builder,

    #[cfg(feature = "threadpool_rayon")]
    inner: rayon::Builder,

    priority: Option<ThreadPriority>
}

impl Builder {
    /// Returns a new thread pool builder.
    pub fn new() -> Builder {
        let inner = {
            #[cfg(feature = "threadpool_crossbeam")]
            { crossbeam::Builder::new() }

            #[cfg(feature = "threadpool_cthpool")]
            { cthpool::Builder::new() }

            #[cfg(feature = "threadpool_rayon")]
            { rayon::Builder::new() }
        };
        Builder { inner, priority: None }
    }

    pub fn priority(mut self, priority: ThreadPriority) -> Self {
        self.priority = Some(priority);

        self
    }

    /// Returns the handle to a new thread pool.
    pub fn build(self) -> ThreadPool {
        let inner = self.inner.build();

        let thread_pool = ThreadPool { inner };

        if let Some(priority) = self.priority {

            let active = thread_pool.inner.active_count();
            let barrier = Arc::new(Barrier::new(active));

            for _ in 0..active {
                let barrier = barrier.clone();
                let priority = priority.clone();

                //Set all the threads in the pool to the given priority
                thread_pool.execute(move || {

                    thread_priority::set_current_thread_priority(priority).expect("Failed to alter the priority of the thread");

                    //Use the barrier to make sure all threads get put like this, and not just 1 thread doing the same thing
                    //N times
                    barrier.wait();

                });
            }

        }

        thread_pool

    }

    /// Configures the number of threads used by the thread pool.
    pub fn num_threads(self, num_threads: usize) -> Self {
        let inner = self.inner.num_threads(num_threads);
        Builder { inner, priority: None }
    }

    // ...eventually add more options?
}

impl ThreadPool {
    /// Spawns a new job into the thread pool.
    pub fn execute<F>(&self, job: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.inner.execute(job)
    }

    /// Synchronously waits for all the jobs queued in the pool
    /// to complete.
    pub fn join(&self) {
        self.inner.join()
    }
}

///We use two separate thread pools because these are mostly used to respond/send messages.
///Therefore, if we used the same threadpool for sending messages to replicas and to clients,
///We could get a situation where client responding would flood the threadpool and cause much larger latency
/// On the requests that are meant for the other replicas, leading to possibly much worse performance
/// By splitting these up we are assuring that does not happen as frequently at least

static mut REPLICA_POOL: Global<ThreadPool> = Global::new();

static mut CLIENT_POOL: Global<ThreadPool> = Global::new();

macro_rules! replica_pool {
    () => {
        match unsafe { REPLICA_POOL.get() } {
	        Some(ref pool) => pool,
            None => panic!("Replica thread pool wasn't initialized"),
        }
    }
}

macro_rules! client_pool {
    () => {
        match unsafe { CLIENT_POOL.get() } {
	        Some(ref pool) => pool,
            None => panic!("Client thread pool wasn't initialized"),
        }
    }
}

/// This function initializes the thread pools.
///
/// It should be called once before the core protocol starts executing.
pub unsafe fn init(replica_num_thread: usize, client_num_thread: usize) -> Result<()> {
    let replica_pool = Builder::new()
        .num_threads(replica_num_thread)
        .priority(ThreadPriority::Max)
        .build();

    let client_pool = Builder::new()
        .num_threads(client_num_thread)
        .priority(ThreadPriority::Min)
        .build();

    REPLICA_POOL.set(replica_pool);
    CLIENT_POOL.set(client_pool);
    Ok(())
}

/// This function drops the global thread pool.
///
/// It shouldn't be needed to be called manually called, as the
/// `InitGuard` should take care of calling this.
pub unsafe fn drop() -> Result<()> {
    REPLICA_POOL.drop();
    CLIENT_POOL.drop();
    Ok(())
}

/// Spawns a new job into the global thread pool.
pub fn execute_replicas<F>(job: F)
where
    F: FnOnce() + Send + 'static,
{
    replica_pool!().execute(job)
}

/// Spawns a new job into the global thread pool.
pub fn execute_clients<F>(job: F)
    where
        F: FnOnce() + Send + 'static,
{
    client_pool!().execute(job)
}

/// Synchronously waits for all the jobs queued in the
/// global thread pool to complete.
pub fn join() {
    replica_pool!().join();
    client_pool!().join()
}
