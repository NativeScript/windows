use parking_lot::{Condvar, Mutex};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use v8;

use crate::{ASYNC_PUMP_HOOK, DELEGATE_ISOLATE_PTR};

struct CallbackInfo {
    cb: v8::Global<v8::Function>,
    args: Vec<v8::Global<v8::Value>>,
    repeats: bool,
}

thread_local! {
    static TASKS: RefCell<HashMap<i32, CallbackInfo>> = RefCell::new(HashMap::new());
}

/// Called from `Runtime::drop` while the isolate is alive — the stored
/// `v8::Global`s must not outlive it.
pub(crate) fn clear_thread_tasks() {
    TASKS.with(|tasks| tasks.borrow_mut().clear());
}

struct TimerRef {
    due: Instant,
    id: i32,
}
thread_local! {
    // Per-thread channel used to receive fired timer ids targeted at this
    // thread's runtime. This prevents one thread's pump from draining ids
    // intended for another thread when tests run in parallel.
    static THREAD_TX_RX: RefCell<Option<(Sender<i32>, Receiver<i32>)>> = RefCell::new(None);
}

#[derive(Clone)]
struct TimerMeta {
    interval_ms: u64,
    repeats: bool,
    dest: Sender<i32>,
}

struct SchedulerInner {
    timers: Vec<TimerRef>, // always sorted by due ascending
    metas: HashMap<i32, TimerMeta>,
    deleted: HashSet<i32>,
}

pub(crate) struct Scheduler {
    next_id: AtomicI32,
    inner: Mutex<SchedulerInner>,
    cond: Condvar,
    // NOTE: the global tx/rx are removed in favor of per-thread receivers.
}

static SCHEDULER: OnceLock<Arc<Scheduler>> = OnceLock::new();

impl Scheduler {
    fn instance() -> Arc<Scheduler> {
        SCHEDULER
            .get_or_init(|| {
                let sched = Arc::new(Scheduler {
                    next_id: AtomicI32::new(1),
                    inner: Mutex::new(SchedulerInner {
                        timers: Vec::new(),
                        metas: HashMap::new(),
                        deleted: HashSet::new(),
                    }),
                    cond: Condvar::new(),
                });

                // Spawn scheduler thread
                let runner = sched.clone();
                thread::Builder::new()
                    .name("ns-timer-scheduler".to_string())
                    .spawn(move || {
                        runner.run();
                    })
                    .ok();

                sched
            })
            .clone()
    }

    fn run(self: Arc<Self>) {
        loop {
            let mut guard = self.inner.lock();
            while guard.timers.is_empty() {
                self.cond.wait(&mut guard);
            }

            // Re-check in a loop to handle multiple timers becoming due
            loop {
                if guard.timers.is_empty() {
                    break;
                }
                let now = Instant::now();
                if guard.timers[0].due > now {
                    let wait_dur = guard.timers[0].due - now;
                    let _timeout_res = self.cond.wait_for(&mut guard, wait_dur);
                    continue;
                }

                // Pop the first due timer
                let tr = guard.timers.remove(0);
                let id = tr.id;
                if guard.deleted.remove(&id) {
                    guard.metas.remove(&id);
                    continue;
                }

                // Grab its meta (clone) then release lock before sending
                let Some(meta) = guard.metas.get(&id).cloned() else {
                    continue;
                };
                drop(guard);

                // Send the fired id to the originating thread via the stored
                // destination sender so only the correct thread's pump will
                // receive and handle it.
                let _ = meta.dest.send(id);

                // Reschedule if repeating
                if meta.repeats {
                    let new_due = tr.due + Duration::from_millis(meta.interval_ms);
                    let mut g = self.inner.lock();
                    // insert keeping sorted order (binary search)
                    let idx = match g.timers.binary_search_by(|r| r.due.cmp(&new_due)) {
                        Ok(i) => i,
                        Err(i) => i,
                    };
                    g.timers.insert(idx, TimerRef { due: new_due, id });
                    guard = g;
                } else {
                    let mut g = self.inner.lock();
                    g.metas.remove(&id);
                    guard = g;
                }
            }
        }
    }

    fn add_timer(&self, id: i32, due: Instant, interval_ms: u64, repeats: bool, dest: Sender<i32>) {
        let mut guard = self.inner.lock();
        guard.metas.insert(
            id,
            TimerMeta {
                interval_ms,
                repeats,
                dest,
            },
        );
        let idx = match guard.timers.binary_search_by(|r| r.due.cmp(&due)) {
            Ok(i) => i,
            Err(i) => i,
        };
        guard.timers.insert(idx, TimerRef { due, id });
        // wake scheduler thread
        self.cond.notify_one();
    }

    fn clear_timer(&self, id: i32) {
        let mut guard = self.inner.lock();
        guard.deleted.insert(id);
        guard.metas.remove(&id);
    }
}

fn to_millis_from_arg(arg: &v8::Local<v8::Value>, scope: &mut v8::PinScope<'_, '_>) -> u64 {
    if let Some(n) = arg.integer_value(scope) {
        if n < 0 {
            0
        } else {
            n as u64
        }
    } else if let Some(n) = arg.number_value(scope) {
        if n.is_finite() && n >= 0.0 {
            n as u64
        } else {
            0
        }
    } else {
        0
    }
}

fn invoke_callback_by_id(id: i32) {
    // Create a V8 scope and call the stored callback for `id` from TASKS.
    let isolate_ptr = DELEGATE_ISOLATE_PTR.with(|c| c.get());
    if isolate_ptr.is_null() {
        return;
    }
    let isolate: &mut v8::Isolate = unsafe { &mut *isolate_ptr };
    v8::scope!(scope, isolate);
    let ctx_global = match scope.get_slot::<v8::Global<v8::Context>>() {
        Some(g) => g.clone(),
        None => return,
    };
    let context = v8::Local::new(scope, &ctx_global);
    let scope = &mut v8::ContextScope::new(scope, context);
    v8::tc_scope!(tc, scope);

    // Clone the persistent handles out of the thread-local storage while
    // holding an immutable borrow, then drop the borrow before invoking
    // the JS callback. This avoids nested RefCell mutable-borrow panics if
    // the callback itself calls clearTimeout/clearInterval.
    let item_opt = TASKS.with(|tasks| {
        let map = tasks.borrow();
        map.get(&id)
            .map(|info| (info.cb.clone(), info.args.clone(), info.repeats))
    });
    let Some((cb_global, args_globals, repeats)) = item_opt else {
        return;
    };

    let func = v8::Local::new(tc, &cb_global);
    let recv: v8::Local<v8::Value> = v8::undefined(tc).into();
    let argc = args_globals.len();
    let mut argv: Vec<v8::Local<v8::Value>> = Vec::with_capacity(argc);
    for a in &args_globals {
        argv.push(v8::Local::new(tc, a));
    }

    let _ = func.call(tc, recv, &argv);
    if tc.has_caught() {
        if let Some(ex) = tc.exception() {
            let s = ex.to_string(tc).and_then(|v| v.to_string(tc));
            if let Some(ss) = s {
                eprintln!(
                    "[NativeScript] timer callback error: {}",
                    ss.to_rust_string_lossy(tc)
                );
            }
        }
        tc.reset();
    }

    // The isolate runs with explicit microtasks policy, so each timer task must
    // end with a checkpoint (browser semantics). Deferred when a blocking-wait
    // pump fires this inside a XAML callout; inline otherwise.
    if !crate::defer_microtask_drain() {
        tc.perform_microtask_checkpoint();
    }

    if !repeats {
        TASKS.with(|tasks| {
            tasks.borrow_mut().remove(&id);
        });
    }
}

pub fn pump() {
    crate::websocket::pump();
    // If the scheduler hasn't been initialized yet, nothing to do.
    if SCHEDULER.get().is_none() {
        return;
    }

    // Pump only this thread's receiver so we handle fired timers that
    // belong to this runtime. The receiver is stored in THREAD_TX_RX.
    loop {
        let maybe_id = THREAD_TX_RX.with(|cell| {
            let opt = cell.borrow();
            if let Some((_, rx)) = &*opt {
                match rx.try_recv() {
                    Ok(id) => Some(id),
                    Err(TryRecvError::Empty) => None,
                    Err(TryRecvError::Disconnected) => None,
                }
            } else {
                None
            }
        });

        match maybe_id {
            Some(id) => invoke_callback_by_id(id),
            None => break,
        }
    }
}

pub(crate) fn init() {
    let _ = Scheduler::instance();

    // Register pump into ASYNC_PUMP_HOOK so blocking waits will pump timers.
    ASYNC_PUMP_HOOK.with(|hook| {
        if let Ok(mut guard) = hook.try_borrow_mut() {
            *guard = Some(Box::new(|| {
                crate::timers::pump();
            }));
        }
    });
}

pub(crate) fn handle_ns_set_timeout(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    handle_set_timer_common(scope, args, false, &mut retval);
}

pub(crate) fn handle_ns_set_interval(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    handle_set_timer_common(scope, args, true, &mut retval);
}

fn handle_set_timer_common(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    repeatable: bool,
    retval: &mut v8::ReturnValue,
) {
    if args.length() < 1 {
        retval.set_int32(0);
        return;
    }

    let func = match v8::Local::<v8::Function>::try_from(args.get(0)) {
        Ok(f) => f,
        Err(_) => {
            retval.set_int32(0);
            return;
        }
    };

    let delay = if args.length() >= 2 {
        to_millis_from_arg(&args.get(1), scope)
    } else {
        0
    };

    // Capture additional args
    let mut extra: Vec<v8::Global<v8::Value>> = Vec::new();
    if args.length() >= 3 {
        for i in 2..args.length() {
            // args.get(i) returns Local<Value>
            let v = args.get(i);
            extra.push(v8::Global::new(scope, v));
        }
    }

    let gfunc = v8::Global::new(scope, func);

    // Allocate an id and store callback in thread-local TASKS
    let sched = Scheduler::instance();
    let id = sched.next_id.fetch_add(1, Ordering::Relaxed);

    TASKS.with(|tasks| {
        let mut map = tasks.borrow_mut();
        map.insert(
            id,
            CallbackInfo {
                cb: gfunc,
                args: extra,
                repeats: repeatable,
            },
        );
    });

    let due = Instant::now() + Duration::from_millis(delay);

    // Ensure this thread has a sender/receiver pair and use the sender as
    // destination for fired timer ids targeted at this runtime.
    let tx = THREAD_TX_RX.with(|cell| {
        if cell.borrow().is_none() {
            let (t, r) = mpsc::channel::<i32>();
            cell.borrow_mut().replace((t, r));
        }
        cell.borrow().as_ref().unwrap().0.clone()
    });

    sched.add_timer(id, due, delay, repeatable, tx);

    retval.set_int32(id);
}

pub(crate) fn handle_ns_clear_timeout(
    _scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    mut retval: v8::ReturnValue,
) {
    let mut id = -1;
    if args.length() > 0 {
        if let Some(n) = args.get(0).integer_value(_scope) {
            id = n as i32;
        }
    }
    if id > 0 {
        let sched = Scheduler::instance();
        sched.clear_timer(id);
        TASKS.with(|tasks| {
            tasks.borrow_mut().remove(&id);
        });
    }
    retval.set_undefined();
}

pub(crate) fn handle_ns_clear_interval(
    scope: &mut v8::PinScope<'_, '_>,
    args: v8::FunctionCallbackArguments,
    retval: v8::ReturnValue,
) {
    handle_ns_clear_timeout(scope, args, retval);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use std::time::Instant;

    // Validate scheduler delivers fired IDs to the specified Sender and that
    // separate receivers on different threads only receive their own IDs.
    #[test]
    fn thread_local_delivery_targets_correct_receiver() {
        let sched = Scheduler::instance();

        let (tx1, rx1) = mpsc::channel::<i32>();
        let (tx2, rx2) = mpsc::channel::<i32>();

        let h1 = thread::spawn(move || {
            rx1.recv_timeout(Duration::from_secs(1))
                .expect("thread 1 did not receive id")
        });
        let h2 = thread::spawn(move || {
            rx2.recv_timeout(Duration::from_secs(1))
                .expect("thread 2 did not receive id")
        });

        // Allocate two ids from the scheduler's counter
        let id1 = sched.next_id.fetch_add(1, Ordering::Relaxed);
        let id2 = sched.next_id.fetch_add(1, Ordering::Relaxed);

        let due = Instant::now();

        // Schedule timers targeted at each thread's sender
        sched.add_timer(id1, due, 0, false, tx1);
        sched.add_timer(id2, due, 0, false, tx2);

        let got1 = h1.join().expect("thread 1 panicked");
        let got2 = h2.join().expect("thread 2 panicked");

        assert_eq!(got1, id1);
        assert_eq!(got2, id2);
    }

    // Stress test: multiple threads each receive many timers targeted at
    // their own sender. Verifies scalability and that no receiver steals
    // another thread's fired ids under load.
    #[test]
    fn stress_many_threads_many_timers() {
        let sched = Scheduler::instance();

        let num_threads = 8usize;
        let timers_per_thread = 50usize;
        let timeout = Duration::from_secs(10);

        let mut txs: Vec<Sender<i32>> = Vec::new();
        let mut handles = Vec::new();

        // spawn threads that will receive their expected number of ids
        for _ in 0..num_threads {
            let (tx, rx) = mpsc::channel::<i32>();
            txs.push(tx.clone());
            let h = thread::spawn(move || {
                let mut got = Vec::with_capacity(timers_per_thread);
                for _ in 0..timers_per_thread {
                    let id = rx.recv_timeout(timeout).expect("didn't receive id");
                    got.push(id);
                }
                got
            });
            handles.push(h);
        }

        // schedule timers targeted at each thread's sender and remember expected ids
        let mut scheduled: Vec<Vec<i32>> = vec![Vec::new(); num_threads];
        for t in 0..num_threads {
            for _ in 0..timers_per_thread {
                let id = sched.next_id.fetch_add(1, Ordering::Relaxed);
                scheduled[t].push(id);
                sched.add_timer(id, Instant::now(), 0, false, txs[t].clone());
            }
        }

        // join and validate each thread only received its scheduled ids
        for (t, h) in handles.into_iter().enumerate() {
            let got = h.join().expect("thread panicked");
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            let mut exp_sorted = scheduled[t].clone();
            exp_sorted.sort_unstable();
            assert_eq!(got_sorted, exp_sorted);
        }
    }
}
