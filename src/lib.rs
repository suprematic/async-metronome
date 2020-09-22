//! ## Unit testing framework for async Rust
//!
//! This crate implements a async unit testing framework, which is based on
//! the [MutithreadedTC](https://www.cs.umd.edu/projects/PL/multithreadedtc/overview.html)
//! developed by William Pugh and Nathaniel Ayewah at the University of Maryland.
//!
//! ## Example
//!
//! ```
//! use futures::{channel::mpsc, SinkExt, StreamExt};
//! use async_metronome::{assert_tick, await_tick};
//!
//! #[async_std::test]
//! async fn test_send_receive() {
//!     let test = async {
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
//!         sender.await;
//!         receiver.await;
//!     };

//!     async_metronome::run(test).await;
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
use std::cell::RefCell;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_std::task::{self, sleep, JoinHandle, TaskId};
use futures::{future::poll_fn, Future};

use derive_builder::Builder;
use pin_project::{pin_project, pinned_drop};

#[derive(Debug, PartialEq, Clone, Copy)]
enum TaskState {
    Spawned,
    Running,
    Pending,
    Ready,
}

#[derive(Debug, Clone)]
struct TaskEntry {
    state: TaskState,
    waker: Option<Waker>,
    detached: bool,
}

/// Options.
#[derive(Default, Builder, Debug)]
pub struct Options {
    #[builder(setter(into, strip_option))]
    timeout: Option<Duration>,
}

struct TestContext {
    tick: usize,
    tasks: HashMap<task::TaskId, TaskEntry>,
    options: Options,
}

type WrappedTestContext = Arc<Mutex<TestContext>>;

impl TestContext {
    pub fn new(options: Options) -> Self {
        Self {
            tick: 0,
            tasks: HashMap::new(),
            options,
        }
    }

    fn set_state(&mut self, task_id: &TaskId, state: TaskState) {
        match self.tasks.get_mut(&task_id) {
            Some(entry) => {
                entry.state = state;
            }
            None => {
                panic!();
            }
        }
    }

    fn spawned(&mut self, task_id: TaskId) {
        log::trace!("{:?} -> Spawned", task_id);

        let entry = TaskEntry {
            state: TaskState::Spawned,
            waker: Option::None,
            detached: false,
        };
        self.tasks.insert(task_id, entry);
    }

    fn running(&mut self, task_id: &TaskId) {
        log::trace!("{:?} -> Running", task_id);
        self.set_state(task_id, TaskState::Running);
    }

    // future is ready
    fn ready(&mut self, task_id: &TaskId) {
        log::trace!("{:?} -> Ready", task_id);

        if let Some(entry) = self.tasks.get_mut(task_id) {
            if entry.detached {
                self.tasks.remove(&task_id);
            } else {
                entry.state = TaskState::Ready;
            }
        }
    }

    // JoinHandle detached
    fn detached(&mut self, task_id: &TaskId) {
        log::trace!("{:?} ** Detach", task_id);

        if let Some(entry) = self.tasks.get_mut(task_id) {
            if entry.state == TaskState::Ready {
                self.tasks.remove(&task_id);
            } else {
                entry.detached = true;
            }
        }
    }

    // returns true if we are in possible deadlock
    fn pending(&mut self, task_id: &TaskId) -> bool {
        log::trace!("{:?} -> Pending", task_id);
        self.set_state(task_id, TaskState::Pending);

        if self.is_all_pending() {
            log::trace!("{:?} ** all pending", task_id);
            if self.has_waiting_for_tick() {
                log::trace!("{:?} ** tick", task_id);
                self.next_tick();
                false
            } else {
                log::trace!("{:?} !! deadlock", task_id);
                true
            }
        } else {
            false
        }
    }

    fn is_all_pending(&self) -> bool {
        self.tasks
            .values()
            .all(|entry| entry.state == TaskState::Pending)
    }

    fn has_waiting_for_tick(&self) -> bool {
        self.tasks.values().any(|entry| entry.waker.is_some())
    }

    fn next_tick(&mut self) {
        self.tick += 1;

        for entry in self.tasks.values_mut() {
            if let Some(waker) = entry.waker.take() {
                waker.wake();
            }
        }
    }

    fn register_waker(&mut self, task_id: TaskId, waker: &Waker) {
        self.tasks.get_mut(&task_id).expect("no task entry").waker = Some(waker.clone());
    }
}

thread_local! {
    static CONTEXT: RefCell<Option<WrappedTestContext>> = RefCell::new(None);
}

static NOCONTEXT: &str = "nocontext";

#[doc(hidden)]
pub fn __private_get_tick() -> usize {
    CONTEXT.with(|state| {
        state
            .borrow()
            .as_ref()
            .expect(NOCONTEXT)
            .lock()
            .unwrap()
            .tick
    })
}

#[macro_export]
/// Asserts current tick counter value.
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

#[doc(hidden)]
pub fn __private_wait_tick(tick: usize) -> impl Future<Output = usize> {
    let task_id = task::current().id();

    log::trace!("{:?} ** tick_wait / wait", task_id);

    poll_fn(move |cx| {
        CONTEXT.with(move |cell| {
            let cell = cell.borrow();
            let mut context = cell.as_ref().expect(NOCONTEXT).lock().unwrap();

            if context.tick >= tick {
                log::trace!("{:?} ** tick_wait / ready", task_id);
                Poll::Ready(tick)
            } else {
                log::trace!("{:?} ** tick_wait / early", task_id);
                context.register_waker(task_id, cx.waker());
                Poll::Pending
            }
        })
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

#[pin_project]
struct TaskWrapper<T> {
    context: WrappedTestContext,

    #[pin]
    task: T,
}

impl<T: Future> Future for TaskWrapper<T> {
    type Output = T::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let context = self.context.clone();
        let task_id = task::current().id();

        CONTEXT.with(move |cell| {
            context.lock().unwrap().running(&task_id);

            cell.replace(Some(context));
            let poll = self.project().task.poll(cx);
            let context = cell.replace(None).unwrap();

            let result = if let Poll::Ready(result) = poll {
                context.lock().unwrap().ready(&task_id);
                Poll::Ready(result)
            } else {
                let mut locked = context.lock().unwrap();
                let deadlock = locked.pending(&task_id);
                if deadlock {
                    match locked.options.timeout {
                        Some(timeout) => {
                            let tick = locked.tick;
                            let context = context.clone();
                            task::spawn(async move {
                                sleep(timeout).await;
                                let current_tick = context.lock().unwrap().tick;
                                if current_tick == tick {
                                    panic!("{:?} !! deadlock", task_id)
                                }
                            });
                        }
                        None => {
                            drop(locked);
                            panic!("{:?} !! deadlock", task_id);
                        }
                    }
                }

                Poll::Pending
            };

            result
        })
    }
}

#[pin_project(PinnedDrop)]
/// Returned by `run`, `run_opt` and `spawn`.
///
/// For more details see
/// [JoinHandle](https://docs.rs/async-std/~1.6/async_std/task/struct.JoinHandle.html)
/// in `async_std`.
pub struct JoinHandleWrapper<T> {
    task_id: task::TaskId,
    context: Option<WrappedTestContext>,

    #[pin]
    handle: JoinHandle<T>,
}

impl<T> JoinHandleWrapper<T> {
    /// Return handle for the underlying task.
    /// For more details see [JoinHandle::task](https://docs.rs/async-std/~1.6/async_std/task/struct.JoinHandle.html#method.task)
    /// in `async_std`.
    pub fn task(&self) -> &task::Task {
        self.handle.task()
    }
}

impl<T> Future for JoinHandleWrapper<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().handle.poll(cx)
    }
}

#[pinned_drop]
impl<T> PinnedDrop for JoinHandleWrapper<T> {
    fn drop(self: Pin<&mut Self>) {
        if self.context.is_some() {
            self.context
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .detached(&self.task_id);
        }
    }
}

/// Instruments a task and spawns it.
///
/// If called outside of test context, the task is spawned without instrumentation.
pub fn spawn<T, O>(task: T) -> JoinHandleWrapper<O>
where
    O: Send + 'static,
    T: Future<Output = O> + Send + 'static,
{
    CONTEXT.with(move |cell| {
        let cell = cell.borrow();

        if cell.as_ref().is_some() {
            let context = cell.as_ref().unwrap();

            let mut locked = context.lock().unwrap();
            let wrapped = TaskWrapper {
                task,
                context: context.clone(),
            };
            let handle = task::spawn(wrapped);
            let task_id = handle.task().id();
            locked.spawned(task_id);
            JoinHandleWrapper {
                task_id,
                handle,
                context: Some(context.clone()),
            }
        } else {
            let handle = task::spawn(task);
            let task_id = handle.task().id();

            JoinHandleWrapper {
                task_id,
                handle,
                context: None,
            }
        }
    })
}

/// Spawns the topmost future of the test case.
///
/// Internally, it creates a `test context` that is
/// propagated to subsequestly spawned futures.
/// # Panics
/// Will panic if test context is already created.
pub fn run_opt<T, O>(test: T, options: Options) -> JoinHandleWrapper<O>
where
    O: Send + 'static,
    T: Future<Output = O> + Send + 'static,
{
    CONTEXT.with(move |cell| {
        let context = Arc::new(Mutex::new(TestContext::new(options)));

        if cell.borrow().is_none() {
            cell.replace(Some(context));
        } else {
            panic!("test context already created");
        }

        spawn(test)
    })
}

/// Same as [run_opt](fn.run_opt.html) but with default options.
pub fn run<T, O>(test: T) -> JoinHandleWrapper<O>
where
    O: Send + 'static,
    T: Future<Output = O> + Send + 'static,
{
    run_opt(test, Options::default())
}

#[cfg(test)]
mod tests {
    use async_std::task;

    macro_rules! promote_await_panic {
        ($e:expr) => {{
            use futures::future::FutureExt;
            match $e.catch_unwind().await {
                Ok(result) => result,
                Err(_) => panic!("future completed with panic"),
            }
        }};
    }

    // no context if task was not spawned with start / run
    #[async_std::test]
    async fn test_no_context() {
        let task = async {
            let task1 = async {
                super::CONTEXT.with(|cell| {
                    assert!(cell.borrow().is_none());
                });
            };
            promote_await_panic!(task::spawn(task1));

            super::CONTEXT.with(|cell| {
                assert!(cell.borrow().is_none());
            });
        };

        promote_await_panic!(task::spawn(task));
    }

    #[async_std::test]
    async fn test_spawn_no_context() {
        let task = async {
            super::CONTEXT.with(|cell| {
                assert!(cell.borrow().is_none());
            });
        };

        promote_await_panic!(super::spawn(task));
    }

    // context is set if spawned with start / run
    #[async_std::test]
    async fn test_has_context() {
        let task = async {
            let task1 = async {
                super::CONTEXT.with(|cell| {
                    assert!(cell.borrow().is_some());
                });
            };
            promote_await_panic!(super::spawn(task1));

            super::CONTEXT.with(|cell| {
                assert!(cell.borrow().is_some());
            });
        };

        promote_await_panic!(super::run(task));
    }

    #[async_std::test]
    async fn test_initial_ticks_0() {
        let task = async {
            assert_tick!(0);
        };

        promote_await_panic!(super::run(task));
    }

    #[async_std::test]
    async fn test_ticks_increment_on_wait() {
        let task = async {
            await_tick!(1);
            assert_tick!(1);
        };

        promote_await_panic!(super::run(task));
    }

    fn get_task_entry(task_id: &task::TaskId) -> Option<super::TaskEntry> {
        super::CONTEXT.with(move |cell| {
            cell.borrow()
                .as_ref()
                .unwrap()
                .lock()
                .unwrap()
                .tasks
                .get(task_id)
                .map(|e| e.clone())
                .clone()
        })
    }

    #[async_std::test]
    async fn test_task_states() {
        let task = async {
            let task_id = task::current().id();
            let task_entry = get_task_entry(&task_id);
            assert!(task_entry.is_some());

            let task_entry = task_entry.unwrap();

            assert_eq!(task_entry.state, super::TaskState::Running);
            assert!(!task_entry.detached);
            assert!(task_entry.waker.is_none());
        };

        let jh = super::run(task);
        let task_id = jh.task().id();

        let task_entry = get_task_entry(&task_id);
        assert!(task_entry.is_some());
        let task_entry = task_entry.unwrap();
        assert!(!task_entry.detached);

        let _ = promote_await_panic!(jh);
        let task_entry = get_task_entry(&task_id);
        assert!(task_entry.is_none());
    }
}
