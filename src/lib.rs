use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use async_std::task::{self, sleep, TaskId};
use futures::{future::poll_fn, Future};

use derive_builder::Builder;
use pin_project::pin_project;

#[derive(Debug, PartialEq)]
enum TaskState {
    Spawned,
    Running,
    Pending,
}

struct TaskEntry {
    state: TaskState,
    waker: Option<Waker>,
}

#[derive(Default, Builder, Debug)]
pub struct Options {
    #[builder(setter(into, strip_option))]
    timeout: Option<Duration>,
    debug: bool,
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
        };
        self.tasks.insert(task_id, entry);
    }

    fn running(&mut self, task_id: &TaskId) {
        log::trace!("{:?} -> Running", task_id);
        self.set_state(task_id, TaskState::Running);
    }

    fn ready(&mut self, task_id: &TaskId) {
        log::trace!("{:?} -> Ready", task_id);
        self.tasks.remove(&task_id);
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

#[doc(hidden)]
pub fn get_tick() -> usize {
    CONTEXT.with(|state| state.borrow().as_ref().unwrap().lock().unwrap().tick)
}

#[macro_export]
macro_rules! assert_tick {
    ($expected:expr) => {
        let actual = async_metronome::get_tick();
        assert!(
            actual == $expected,
            "tick mismatch: expected={}, actual={}",
            $expected,
            actual
        )
    };
}

#[doc(hidden)]
pub fn wait_tick(tick: usize) -> impl Future<Output = usize> {
    let task_id = task::current().id();

    log::trace!("{:?} ** tick_wait / wait", task_id);

    poll_fn(move |cx| {
        CONTEXT.with(move |cell| {
            let cell = cell.borrow();
            let mut context = cell.as_ref().unwrap().lock().unwrap();

            match context.tick.cmp(&tick) {
                Ordering::Equal => {
                    log::trace!("{:?} ** tick_wait / ready", task_id);
                    Poll::Ready(tick)
                }
                Ordering::Less => {
                    log::trace!("{:?} ** tick_wait / early", task_id);
                    context.register_waker(task_id, cx.waker());

                    Poll::Pending
                }
                Ordering::Greater => {
                    panic!();
                }
            }
        })
    })
}

#[macro_export]
macro_rules! await_tick {
    ($tick:expr) => {
        async_metronome::wait_tick($tick as usize).await
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

// instrument task and spawn it. Will panic if executed outside of
// already intstrumented task.
pub fn spawn<T, O>(task: T) -> task::JoinHandle<O>
where
    O: Send + 'static,
    T: Future<Output = O> + Send + 'static,
{
    CONTEXT.with(move |cell| {
        let cell = cell.borrow();
        let context = cell.as_ref().expect("nocontext");

        let mut locked = context.lock().unwrap();

        let wrapped = TaskWrapper {
            task,
            context: context.clone(),
        };

        let handle = task::spawn(wrapped);

        locked.spawned(handle.task().id());

        handle
    })
}

// run test case
pub fn run_opt<T, O>(test: T, options: Options) -> task::JoinHandle<O>
where
    O: Send + 'static,
    T: Future<Output = O> + Send + 'static,
{
    CONTEXT.with(move |cell| {
        let context = Arc::new(Mutex::new(TestContext::new(options)));

        cell.replace(Some(context));
        spawn(test)
    })
}

// run test case with default options
pub fn run<T, O>(test: T) -> task::JoinHandle<O>
where
    O: Send + 'static,
    T: Future<Output = O> + Send + 'static,
{
    run_opt(test, Options::default())
}
