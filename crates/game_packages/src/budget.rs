//! Cooperative VM interruption. This bounds ordinary runaway bytecode, not
//! process memory or native/compiler work; packages are trusted local code.
use std::{
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use steel::steel_vm::engine::Engine;
#[derive(Default)]
struct State {
    deadline: Option<Instant>,
    stopped: bool,
}

pub(super) struct Budget {
    interrupted: Arc<AtomicBool>,
    resume: Box<dyn Fn() + Send>,
    state: Arc<(Mutex<State>, Condvar)>,
    worker: Option<JoinHandle<()>>,
}

impl Budget {
    pub fn new(engine: &Engine) -> std::io::Result<Self> {
        // Steel exposes this controller through Engine, but its module is private.
        let controller = engine.get_thread_state_controller();
        let interrupted = Arc::new(AtomicBool::new(false));
        let state = Arc::new((Mutex::new(State::default()), Condvar::new()));
        let shared = state.clone();
        let flag = interrupted.clone();
        let watchdog_controller = controller.clone();
        let worker = std::thread::Builder::new()
            .name("scheme-budget".into())
            .spawn(move || {
                let (lock, wake) = &*shared;
                let mut state = lock.lock().unwrap();
                while !state.stopped {
                    if let Some(deadline) = state.deadline {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            flag.store(true, Ordering::SeqCst);
                            watchdog_controller.interrupt();
                            state.deadline = None;
                        } else {
                            state = wake.wait_timeout(state, remaining).unwrap().0;
                        }
                    } else {
                        state = wake.wait(state).unwrap();
                    }
                }
            })?;
        Ok(Self {
            interrupted,
            resume: Box::new(move || controller.resume()),
            state,
            worker: Some(worker),
        })
    }

    pub fn arm(&self, duration: Duration) {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap();
        self.interrupted.store(false, Ordering::SeqCst);
        (self.resume)();
        state.deadline = Some(Instant::now() + duration);
        wake.notify_one();
    }

    pub fn disarm(&self) -> bool {
        let (lock, wake) = &*self.state;
        let mut state = lock.lock().unwrap();
        let expired = state
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
            || self.interrupted.load(Ordering::SeqCst);
        state.deadline = None;
        (self.resume)();
        wake.notify_one();
        expired
    }
}

impl Drop for Budget {
    fn drop(&mut self) {
        let (lock, wake) = &*self.state;
        lock.lock().unwrap().stopped = true;
        wake.notify_one();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
