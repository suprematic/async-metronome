//! ## Unit testing framework for async Rust
//!
//! This crate implements a async unit testing framework, which is based on
//! the [MutithreadedTC](https://www.cs.umd.edu/projects/PL/multithreadedtc/overview.html)
//! developed by William Pugh and Nathaniel Ayewah at the University of Maryland.
//!
//! ## Example
//!
//! ```
//! # use futures::{channel::mpsc, SinkExt, StreamExt};
//! use async_metronome::{assert_tick, await_tick};
//!
//! #[async_metronome::test]
//! async fn test_send_receive() {
//!         let (mut sender, mut receiver) = mpsc::channel::<usize>(1);
//!
//!         let sender = async move {
//!             assert_tick!(0);
//!             sender.send(42).await.unwrap();
//!             sender.send(17).await.unwrap();
//!             assert_tick!(1);
//!         };
//!
//!         let receiver = async move {
//!             assert_tick!(0);
//!             await_tick!(1);
//!             receiver.next().await;
//!             receiver.next().await;
//!         };
//!         let sender = async_metronome::spawn(sender);
//!         let receiver = async_metronome::spawn(receiver);
//!
//!         sender.await;
//!         receiver.await;
//! }
//! ```
//!
//! ## Explanation
//! async-metronome has an internal clock. The clock only advances to the next
//! tick when all tasks are in a pending state.
//!
//! The clock starts at `tick 0`. In this example, the macro `await_tick!(1)` makes
//! the receiver block until the clock reaches `tick 1` before resuming.
//! Thread 1 is allowed to run freely in `tick 0`, until it blocks on
//! the call to `sender.send(17)`. At this point, all threads are blocked, and
//! the clock can advance to the next tick.
//!
//! In `tick 1`, the statement `receiver.next(42)` in the receiver is executed,
//! and this frees up sender. The final statement in sender asserts that the
//! clock is in `tick 1`, in effect asserting that the task blocked on the
//! call to `sender.send(17)`.

use futures::{
    future::poll_fn,
    task::{self, Poll, Waker},
    Future,
};

use async_task::{self, Runnable};

use std::cell::RefCell;
use std::panic;
use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use std::vec::Vec;

use derive_builder::Builder;
use flume;
use pin_project::pin_project;
use threadpool::ThreadPool;

/// Marks async test function.
pub use async_metronome_attributes::test;

const DEADLOCK: &str = "deadlock";
const HASCONTEXT: &str = "hascontext";

/// Options.
#[derive(Clone, Default, Builder, Debug)]
pub struct Options {
    #[builder(setter(into, strip_option), default)]
    _timeout: Option<Duration>,

    #[builder(setter(into), default)]
    debug: bool,
}

struct RunQueueEntry(usize, Runnable);

struct TestContext {
    tick: usize,
    task_id: usize,
    task_active: usize,
    sender: flume::Sender<RunQueueEntry>,
    wakers: Vec<Waker>,
    options: Arc<Options>,
}

/// A future that awaits the result of a task.
///
/// Dropping a [`JoinHandle`] will detach the task, meaning that there is no longer
/// a handle to the task and no way to `join` on it.
pub struct JoinHandle<O> {
    task: Option<async_task::Task<O>>,
}

impl<O> Future for JoinHandle<O> {
    type Output = O;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        Pin::new(&mut self.task.as_mut().unwrap()).poll(cx)
    }
}

impl<T> Drop for JoinHandle<T> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.detach();
        }
    }
}

impl TestContext {
    fn new(sender: flume::Sender<RunQueueEntry>, options: Arc<Options>) -> Self {
        TestContext {
            tick: 0,
            task_id: 0,
            task_active: 0,
            sender,
            wakers: Vec::new(),
            options,
        }
    }

    fn register_wait(&mut self, waker: Waker) {
        self.wakers.push(waker);
    }

    fn next_tick(&mut self) -> usize {
        let wakers = self.wakers.len();

        if wakers > 0 {
            self.tick += 1;

            for waker in &self.wakers {
                waker.wake_by_ref();
            }

            self.wakers.clear();

            wakers
        } else {
            wakers
        }
    }

    fn spawn<F, O>(&mut self, future: F) -> JoinHandle<O>
    where
        F: Future<Output = O> + Send + 'static,
        O: Send + 'static,
    {
        let sender = self.sender.clone();

        let task_id = self.task_id;
        self.task_id += 1;
        self.task_active += 1;

        let schedule = move |runnable| {
            sender.send(RunQueueEntry(task_id, runnable)).unwrap();
        };

        let options = self.options.clone();
        if options.debug {
            println!("{:?} ** spawn", task_id);
        }
        let (runnable, task) = async_task::spawn(
            TaskWrapper {
                future,
                task_id,
                options,
            },
            schedule,
        );
        runnable.schedule();

        JoinHandle { task: Some(task) }
    }
}

type WrappedTestContext = Arc<Mutex<TestContext>>;

thread_local! {
    static CONTEXT: RefCell<Option<WrappedTestContext>> = RefCell::new(None);
}

fn get_context() -> WrappedTestContext {
    CONTEXT.with(|cell| cell.borrow().as_ref().expect(HASCONTEXT).clone())
}

#[doc(hidden)]
pub fn __private_wait_tick(tick: usize) -> impl Future<Output = usize> {
    poll_fn(move |cx| {
        let test_context = get_context();
        let mut test_context = test_context.lock().unwrap();

        if test_context.tick >= tick {
            Poll::Ready(tick)
        } else {
            test_context.register_wait(cx.waker().clone());
            Poll::Pending
        }
    })
}

/// Awaits for the tick counter reach the specified value.
///
/// Tick counter increments when all tasks in the test case
/// are in 'Pending' state, and at least one of them awaits for a
/// tick.
#[macro_export]
macro_rules! await_tick {
    ($tick:expr) => {
        $crate::__private_wait_tick($tick as usize).await
    };
}

#[doc(hidden)]
pub fn __private_get_tick() -> usize {
    get_context().lock().unwrap().tick
}

/// Asserts current tick counter value.
#[macro_export]
macro_rules! assert_tick {
    ($expected:expr) => {
        let actual = $crate::__private_get_tick();
        assert!(
            actual == $expected,
            "tick mismatch: expected={}, actual={}",
            $expected,
            actual
        )
    };
}

#[pin_project]
struct TaskWrapper<T> {
    task_id: usize,
    #[pin]
    future: T,
    options: Arc<Options>,
}

impl<T: Future> Future for TaskWrapper<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<Self::Output> {
        let debug = self.options.debug;

        let this = self.project();
        let task_id = *this.task_id;

        if debug {
            println!("{:?} ** poll", task_id);
        }

        let context = get_context();
        match panic::catch_unwind(panic::AssertUnwindSafe(|| this.future.poll(cx))) {
            Ok(poll) => {
                if poll.is_ready() {
                    if debug {
                        println!("{:?} ** ready", task_id);
                    }

                    context.lock().unwrap().task_active -= 1;
                } else {
                    if debug {
                        println!("{:?} ** pending", task_id);
                    }
                }

                poll
            }
            Err(error) => {
                context.lock().unwrap().task_active -= 1;
                panic::resume_unwind(error);
            }
        }
    }
}

/// Checks if context is set.
///
pub fn is_context() -> bool {
    CONTEXT.with(|cell| cell.borrow().as_ref().is_some())
}

/// Spawns a task
///
/// Panics if called outside of the test case - either a root task started by `run` / `run_opt` or
/// one of child tasks.
pub fn spawn<F, O>(future: F) -> JoinHandle<O>
where
    F: Future<Output = O> + Send + 'static,
    O: Send + 'static,
{
    get_context().lock().expect(HASCONTEXT).spawn(future)
}

/// Runs the test case and blocks until it complets or panics.
///
/// Internally, it creates a `test context` that is
/// propagated to subsequestly spawned futures.
///
/// # Panics
/// Will panic if used from already running test case.
/// Will panic if the future it runs panics.
///
/// Will panic if deadlock is detected. That means, all tasks are in 'pending' state
/// and none of them is waiting for next tick (`await_tick`).
pub fn run_opt<O, F>(future: F, options: Options)
where
    F: Future<Output = O> + Send + 'static,
    O: Send + 'static,
{
    CONTEXT.with(|cell| {
        if cell.borrow().is_some() {
            panic!("{}", HASCONTEXT);
        }
    });

    let options = Arc::new(options);
    let pool = ThreadPool::new(8);
    let (sender, receiver) = flume::unbounded::<RunQueueEntry>();
    let mut context = TestContext::new(sender, options.clone());

    context.spawn(future);

    let context = Arc::new(Mutex::new(context));
    let panic_flag = Arc::new(AtomicBool::new(false));
    loop {
        if let Ok(RunQueueEntry(task_id, runnable)) = receiver.try_recv() {
            let panic_flag1 = panic_flag.clone();
            let context = context.clone();

            pool.execute(move || {
                CONTEXT.with(|cell| cell.replace(Some(context)));

                let result = panic::catch_unwind(panic::AssertUnwindSafe(|| runnable.run()));

                if let Err(_) = result {
                    if task_id == 0 {
                        panic_flag1.store(true, Ordering::Relaxed);
                    }
                }

                CONTEXT.with(|cell| cell.replace(None));
            });

            if !panic_flag.load(Ordering::Relaxed) {
                continue;
            }
        }

        // println!("queue empty, waiting joining runnables");
        // wait for all to continue
        pool.join();

        if panic_flag.load(Ordering::Relaxed) {
            panic!("root task panic");
        }

        // if there are new schedules, meanwhile
        if !receiver.is_empty() {
            continue;
        }

        let mut context = context.lock().unwrap();

        if options.debug {
            println!("queue exhaused: tc: {:?}", context.task_active);
        }

        if context.task_active > 0 {
            let wakers = context.next_tick();

            if wakers > 0 {
                if options.debug {
                    println!("tick -> {:?}, waking up {:?} wakers", context.tick, wakers);
                }
                continue;
            } else {
                panic!("{}", DEADLOCK);
            }
        } else {
            break;
        }
    }
}

/// Same as [run_opt](fn.run_opt.html) but with default options.
pub fn run<O, F>(future: F)
where
    F: Future<Output = O> + Send + 'static,
    O: Send + 'static,
{
    run_opt(future, Options::default());
}

#[cfg(test)]
mod tests {
    #[test]
    #[should_panic]
    fn test_panic_no_context() {
        super::spawn(async {});
    }

    #[test]
    #[should_panic]
    fn test_root_task_exception() {
        super::run(async {
            panic!();
        });
    }

    #[test]
    fn test_inner_task_exception() {
        super::run(async {
            super::spawn(async {
                panic!();
            });
        });
    }

    #[test]
    #[should_panic]
    fn test_inner_task_exception_propagates() {
        super::run(async {
            let jh = super::spawn(async {
                panic!();
            });

            jh.await;
        });
    }

    #[test]
    fn test_has_context() {
        super::run(async {
            super::CONTEXT.with(|cell| assert!(cell.borrow().is_some()));

            super::spawn(async {
                super::CONTEXT.with(|cell| {
                    assert!(cell.borrow().is_some());
                });
            })
            .await;
        });
    }

    #[test]
    #[should_panic]
    fn test_panic_nested() {
        super::run(async {
            super::run(async {});
        });
    }

    #[test]
    fn test_initial_ticks_0() {
        super::run(async {
            assert_tick!(0);
        });
    }

    #[test]
    fn test_task_count() {
        use futures::future::FutureExt;

        super::run(async {
            // just top level
            assert_eq!(super::get_context().lock().unwrap().task_active, 1);

            super::spawn(async {
                // top level + nested
                assert_eq!(super::get_context().lock().unwrap().task_active, 2);
            })
            .await;

            // nested is gone
            assert_eq!(super::get_context().lock().unwrap().task_active, 1);

            // nested starts and panics
            let handle = super::spawn(async {
                panic!();
            });

            // still top level only
            let _ = handle.catch_unwind().await;

            assert_eq!(super::get_context().lock().unwrap().task_active, 1);
        });
    }

    #[test]
    fn test_ticks_increment_on_wait() {
        super::run(async {
            super::await_tick!(1);
            super::assert_tick!(1);
        });
    }

    #[test]
    fn test_ticks_increment_on_wait_inner() {
        super::run(async {
            super::spawn(async {
                super::await_tick!(1);
            })
            .await;
            super::assert_tick!(1);
        });
    }

    #[test]
    #[should_panic]
    fn test_deadlock() {
        use async_std::task;
        use std::time::Duration;

        super::run(async {
            task::sleep(Duration::from_secs(1)).await;
        });
    }

    #[test]
    #[should_panic]
    fn test_deadlock_inner() {
        use async_std::task;
        use std::time::Duration;

        super::run(async {
            super::spawn(async {
                task::sleep(Duration::from_secs(1)).await;
            })
            .await;
        });
    }
}
