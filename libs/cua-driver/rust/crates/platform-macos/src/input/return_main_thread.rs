//! Main-thread construction broker for the isolated window-tagged Return experiment.
//!
//! This queue never authenticates or posts an event. A timed-out caller only
//! cancels its owned construction job; a late callback can construct/drop, but
//! cannot deliver input. The normal input worker retains all target/activity
//! checks and is the sole owner of any successfully transferred CGEvent.

use super::return_trace::Trace;
use core_graphics::event::CGEvent;
use std::ffi::c_void;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

const WAIT_LIMIT: Duration = Duration::from_millis(500);
const WAIT_SLICE: Duration = Duration::from_millis(10);
static BROKER: OnceLock<Arc<Broker>> = OnceLock::new();

#[derive(Default)]
struct Broker {
    // Odd epochs are ready. Closing/reopening changes the epoch, so an old
    // queued callback cannot revive just because a new host becomes ready.
    host_epoch: AtomicU64,
    outstanding: AtomicBool,
}

impl Broker {
    fn ready(&self, on_main: bool, enabled: bool) -> bool {
        if !on_main || !enabled {
            return false;
        }
        self.host_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                if epoch % 2 == 1 {
                    Some(epoch)
                } else {
                    epoch.checked_add(1).filter(|next| *next < u64::MAX)
                }
            })
            .is_ok()
    }

    fn close(&self) {
        let _ = self
            .host_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |epoch| {
                (epoch % 2 == 1).then(|| epoch.checked_add(1)).flatten()
            });
    }

    fn reserve(self: &Arc<Self>, deadline: Instant) -> Result<Permit, String> {
        let epoch = self.host_epoch.load(Ordering::Acquire);
        if epoch % 2 != 1 {
            return Err("Return construction main-thread host is not ready".into());
        }
        self.outstanding
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "Return construction main-thread queue is busy".to_owned())?;
        let permit = Permit {
            broker: Arc::clone(self),
            epoch,
            control: Arc::new(Control {
                cancelled: AtomicBool::new(false),
                settled: AtomicBool::new(false),
                deadline,
            }),
        };
        permit.check()?;
        Ok(permit)
    }
}

struct Control {
    cancelled: AtomicBool,
    settled: AtomicBool,
    deadline: Instant,
}

struct Permit {
    broker: Arc<Broker>,
    epoch: u64,
    control: Arc<Control>,
}

impl Permit {
    fn check(&self) -> Result<(), String> {
        check_live(&self.broker, self.epoch, &self.control)
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        self.broker.outstanding.store(false, Ordering::Release);
        self.control.settled.store(true, Ordering::Release);
    }
}

fn check_live(broker: &Broker, epoch: u64, control: &Control) -> Result<(), String> {
    if control.cancelled.load(Ordering::Acquire) {
        return Err("Return construction was cancelled".into());
    }
    if Instant::now() >= control.deadline {
        return Err("Return construction main-thread deadline expired".into());
    }
    if epoch % 2 != 1 || broker.host_epoch.load(Ordering::Acquire) != epoch {
        return Err("Return construction main-thread host changed".into());
    }
    Ok(())
}

#[derive(Clone)]
struct Request {
    window_id: u32,
    down: bool,
    trace: Trace,
}

struct Job<T> {
    request: Request,
    reply: SyncSender<Result<T, String>>,
    // Drop this last, after any unsent result has been destroyed. The caller
    // additionally waits for settled before accepting a received result.
    permit: Permit,
}

impl<T> Job<T> {
    fn run(self, on_main: bool, build: impl FnOnce(Request) -> Result<T, String>) {
        let trace = self.request.trace.clone();
        let down = self.request.down;
        let span = trace.begin(if down {
            "broker.down.main_callback"
        } else {
            "broker.up.main_callback"
        });
        let result = catch_unwind(AssertUnwindSafe(|| {
            if !on_main {
                trace.event("broker.callback_wrong_thread");
                return Err("Return construction callback is not on the main thread".into());
            }
            self.permit.check().inspect_err(|_| {
                trace.event(if down {
                    "broker.down.before_build_refused"
                } else {
                    "broker.up.before_build_refused"
                })
            })?;
            let value = trace.run(
                if down {
                    "broker.down.construct"
                } else {
                    "broker.up.construct"
                },
                || build(self.request),
            )?;
            // A cancellation/timeout during OS construction discards the owned
            // result. Nothing in this callback is capable of posting it.
            self.permit.check().inspect_err(|_| {
                trace.event(if down {
                    "broker.down.late_result_discarded"
                } else {
                    "broker.up.late_result_discarded"
                })
            })?;
            Ok(value)
        }))
        .unwrap_or_else(|_| Err("Return construction callback panicked".into()));
        span.finish(result.is_ok());
        // Capacity one, one sender, one result: never block the main queue.
        // A disconnected waiter drops a late owned result right here.
        if let Err(unsent) = self.reply.try_send(result) {
            trace.event(if down {
                "broker.down.reply_discarded"
            } else {
                "broker.up.reply_discarded"
            });
            drop(unsent);
        }
    }
}

fn wait_for_result<T>(
    reply: Receiver<Result<T, String>>,
    permit_broker: &Broker,
    epoch: u64,
    control: &Control,
    check_request: impl Fn() -> Result<(), String>,
) -> Result<T, String> {
    let mut result = None;
    loop {
        if let Err(error) = check_request().and_then(|_| check_live(permit_broker, epoch, control))
        {
            control.cancelled.store(true, Ordering::Release);
            return Err(error);
        }
        if result.is_some() && control.settled.load(Ordering::Acquire) {
            return result.take().expect("result checked above");
        }
        let remaining = control.deadline.saturating_duration_since(Instant::now());
        if result.is_some() {
            std::thread::sleep(remaining.min(Duration::from_millis(1)));
            continue;
        }
        match reply.recv_timeout(remaining.min(WAIT_SLICE)) {
            Ok(value) => result = Some(value),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                control.cancelled.store(true, Ordering::Release);
                return Err("Return construction result channel closed".into());
            }
        }
    }
}

// CGEvent is mutable and therefore not generally shareable. This private,
// non-Clone, non-Sync wrapper transfers one exclusively owned event once from
// the main queue to its original worker. The NSEvent autorelease pool has
// drained before wrapping; no NSEvent, AX object, borrowed pointer or other
// CGEvent handle is shared across the queue. Destruction is an owned CFRelease.
struct OwnedReturnEvent(CGEvent);
unsafe impl Send for OwnedReturnEvent {}

pub fn experiment_enabled() -> bool {
    std::env::var_os("CUA_EXPERIMENTAL_CHROME_WINDOW_RETURN").as_deref()
        == Some(std::ffi::OsStr::new("1"))
}

fn on_main_thread() -> bool {
    unsafe { libc::pthread_main_np() != 0 }
}

/// Called only immediately before an actual main-thread run loop is serviced.
/// Merely importing this module or enabling the environment flag is not ready.
pub fn host_ready_on_main() -> bool {
    if !on_main_thread() || !experiment_enabled() {
        return false;
    }
    BROKER
        .get_or_init(|| Arc::new(Broker::default()))
        .ready(true, true)
}

pub fn clear_host_ready_on_main() {
    if on_main_thread() {
        if let Some(broker) = BROKER.get() {
            broker.close();
        }
    }
}

#[link(name = "dispatch", kind = "dylib")]
extern "C" {
    static _dispatch_main_q: u8;
    fn dispatch_async_f(
        queue: *const c_void,
        context: *mut c_void,
        work: unsafe extern "C" fn(*mut c_void),
    );
}

unsafe extern "C" fn construct_on_main(context: *mut c_void) {
    // dispatch_async_f owns this exact Box until the single callback runs.
    // Timeout never frees it; a stalled host retains one job and its permit.
    let job = unsafe { Box::from_raw(context.cast::<Job<OwnedReturnEvent>>()) };
    job.run(on_main_thread(), |request| {
        super::skylight::window_tagged_return_event_on_main(request.window_id, request.down)
            .map(OwnedReturnEvent)
            .map_err(|error| error.to_string())
    });
}

pub(super) fn construct_return_event(
    window_id: u32,
    down: bool,
    trace: &Trace,
) -> anyhow::Result<CGEvent> {
    if !experiment_enabled() {
        anyhow::bail!("window-tagged Return experiment is disabled");
    }
    if window_id == 0 || on_main_thread() {
        anyhow::bail!("Return construction requires an addressed worker request");
    }
    trace.run(
        if down {
            "broker.down.prepare"
        } else {
            "broker.up.prepare"
        },
        || {
            crate::foreground_activity::check_request()?;
            let broker = BROKER.get().ok_or_else(|| {
                anyhow::anyhow!("Return construction main-thread host is not ready")
            })?;
            let permit = broker
                .reserve(Instant::now() + WAIT_LIMIT)
                .map_err(anyhow::Error::msg)?;
            let control = Arc::clone(&permit.control);
            let epoch = permit.epoch;
            let (tx, rx) = mpsc::sync_channel(1);
            let job: Box<Job<OwnedReturnEvent>> = Box::new(Job {
                request: Request {
                    window_id,
                    down,
                    trace: trace.clone(),
                },
                reply: tx,
                permit,
            });
            let queued = trace.begin(if down {
                "broker.down.queue_submit"
            } else {
                "broker.up.queue_submit"
            });
            unsafe {
                dispatch_async_f(
                    &raw const _dispatch_main_q as *const c_void,
                    Box::into_raw(job).cast(),
                    construct_on_main,
                );
            }
            queued.finish(true);
            let result = wait_for_result(rx, broker, epoch, &control, || {
                crate::foreground_activity::check_request().map_err(|error| error.to_string())
            })
            .map_err(anyhow::Error::msg)?;
            trace.event(if down {
                "broker.down.accepted"
            } else {
                "broker.up.accepted"
            });
            Ok(result.0)
        },
    )
}

/// Service the experimental construction queue without NSApplication, a
/// window, activation, or an input source. The no-op timer only keeps this
/// actual main-thread CFRunLoop alive; the host owner controls normal shutdown.
pub fn headless_main_loop() -> anyhow::Result<()> {
    use core_foundation::base::{kCFAllocatorDefault, TCFType};
    use core_foundation::date::CFAbsoluteTimeGetCurrent;
    use core_foundation::runloop::{
        kCFRunLoopDefaultMode, CFRunLoop, CFRunLoopTimer, CFRunLoopTimerCreate,
        CFRunLoopTimerInvalidate, CFRunLoopTimerRef,
    };
    if !on_main_thread() || !experiment_enabled() {
        anyhow::bail!("experimental Return host requires the enabled actual main thread");
    }
    extern "C" fn keep_alive(_: CFRunLoopTimerRef, _: *mut c_void) {}
    let timer = unsafe {
        let raw = CFRunLoopTimerCreate(
            kCFAllocatorDefault,
            CFAbsoluteTimeGetCurrent() + 60.0,
            60.0,
            0,
            0,
            keep_alive,
            std::ptr::null_mut(),
        );
        if raw.is_null() {
            anyhow::bail!("experimental Return keepalive timer is unavailable");
        }
        CFRunLoopTimer::wrap_under_create_rule(raw)
    };
    struct HostLoop {
        run_loop: CFRunLoop,
        timer: CFRunLoopTimer,
    }
    impl Drop for HostLoop {
        fn drop(&mut self) {
            clear_host_ready_on_main();
            unsafe {
                self.run_loop
                    .remove_timer(&self.timer, kCFRunLoopDefaultMode);
                CFRunLoopTimerInvalidate(self.timer.as_concrete_TypeRef());
            }
        }
    }
    let host = HostLoop {
        run_loop: CFRunLoop::get_current(),
        timer,
    };
    unsafe { host.run_loop.add_timer(&host.timer, kCFRunLoopDefaultMode) };
    if !unsafe {
        host.run_loop
            .contains_timer(&host.timer, kCFRunLoopDefaultMode)
    } || !host_ready_on_main()
    {
        anyhow::bail!("experimental Return main-thread host could not become ready");
    }
    CFRunLoop::run_current();
    drop(host);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    fn ready_broker() -> Arc<Broker> {
        let broker = Arc::new(Broker::default());
        assert!(broker.ready(true, true));
        broker
    }

    fn job<T>(broker: &Arc<Broker>) -> (Job<T>, Receiver<Result<T, String>>, Arc<Control>) {
        let permit = broker
            .reserve(Instant::now() + Duration::from_secs(1))
            .unwrap();
        let control = Arc::clone(&permit.control);
        let (reply, rx) = mpsc::sync_channel(1);
        (
            Job {
                request: Request {
                    window_id: 42,
                    down: true,
                    trace: Trace::disabled(),
                },
                reply,
                permit,
            },
            rx,
            control,
        )
    }

    #[test]
    fn window_return_broker_requires_real_enabled_host_and_one_job() {
        let broker = Arc::new(Broker::default());
        assert!(!broker.ready(false, true));
        assert!(!broker.ready(true, false));
        assert!(broker.reserve(Instant::now() + WAIT_LIMIT).is_err());
        assert!(broker.ready(true, true));
        let (job, rx, control) = job::<u32>(&broker);
        assert!(broker.reserve(Instant::now() + WAIT_LIMIT).is_err());
        assert!(!control.settled.load(Ordering::Acquire));
        job.run(true, |request| Ok(request.window_id));
        assert_eq!(rx.try_recv().unwrap().unwrap(), 42);
        assert!(control.settled.load(Ordering::Acquire));
        assert!(broker.reserve(Instant::now() + WAIT_LIMIT).is_ok());
    }

    #[test]
    fn window_return_broker_cancelled_or_worker_callback_never_builds() {
        for cancelled in [true, false] {
            let broker = ready_broker();
            let (job, rx, control) = job::<()>(&broker);
            control.cancelled.store(cancelled, Ordering::Release);
            job.run(cancelled, |_| panic!("refused callback must not construct"));
            let error = rx.try_recv().unwrap().unwrap_err();
            assert!(error.contains(if cancelled {
                "cancelled"
            } else {
                "not on the main"
            }));
            assert!(control.settled.load(Ordering::Acquire));
        }
    }

    #[test]
    fn window_return_broker_cancellation_keeps_queued_permit_until_callback() {
        let broker = ready_broker();
        let (job, rx, control) = job::<()>(&broker);
        let epoch = job.permit.epoch;
        let error = wait_for_result(rx, &broker, epoch, &control, || {
            Err("request cancelled".into())
        })
        .unwrap_err();
        assert_eq!(error, "request cancelled");
        assert!(broker.reserve(Instant::now() + WAIT_LIMIT).is_err());
        job.run(true, |_| panic!("late cancelled job must not construct"));
        assert!(control.settled.load(Ordering::Acquire));
        assert!(broker.reserve(Instant::now() + WAIT_LIMIT).is_ok());
    }

    #[test]
    fn window_return_broker_deadline_refuses_before_work() {
        let broker = ready_broker();
        let mut permit = broker.reserve(Instant::now() + WAIT_LIMIT).unwrap();
        Arc::get_mut(&mut permit.control).unwrap().deadline = Instant::now();
        let (reply, rx) = mpsc::sync_channel(1);
        let job = Job::<()> {
            request: Request {
                window_id: 42,
                down: true,
                trace: Trace::disabled(),
            },
            reply,
            permit,
        };
        job.run(true, |_| panic!("expired job must not construct"));
        assert!(rx.try_recv().unwrap().unwrap_err().contains("deadline"));
    }

    #[test]
    fn window_return_broker_wait_timeout_cannot_release_queued_job() {
        let broker = ready_broker();
        let permit = broker
            .reserve(Instant::now() + Duration::from_millis(5))
            .unwrap();
        let epoch = permit.epoch;
        let control = Arc::clone(&permit.control);
        let (reply, rx) = mpsc::sync_channel(1);
        let job = Job::<()> {
            request: Request {
                window_id: 42,
                down: true,
                trace: Trace::disabled(),
            },
            reply,
            permit,
        };
        assert!(wait_for_result(rx, &broker, epoch, &control, || Ok(()))
            .unwrap_err()
            .contains("deadline"));
        assert!(!control.settled.load(Ordering::Acquire));
        assert!(broker.reserve(Instant::now() + WAIT_LIMIT).is_err());
        job.run(true, |_| panic!("late timeout must not construct"));
        assert!(control.settled.load(Ordering::Acquire));
    }

    struct DropProbe(Arc<AtomicUsize>);
    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn window_return_broker_cancel_during_build_drops_late_result() {
        let broker = ready_broker();
        let drops = Arc::new(AtomicUsize::new(0));
        let (job, rx, control) = job::<DropProbe>(&broker);
        job.run(true, |_| {
            assert!(broker.outstanding.load(Ordering::Acquire));
            control.cancelled.store(true, Ordering::Release);
            Ok(DropProbe(Arc::clone(&drops)))
        });
        assert!(rx.try_recv().unwrap().is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(control.settled.load(Ordering::Acquire));
    }

    #[test]
    fn window_return_broker_receiver_loss_drops_owned_result() {
        let broker = ready_broker();
        let drops = Arc::new(AtomicUsize::new(0));
        let (job, rx, control) = job::<DropProbe>(&broker);
        drop(rx);
        job.run(true, |_| Ok(DropProbe(Arc::clone(&drops))));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(control.settled.load(Ordering::Acquire));
    }

    #[test]
    fn window_return_broker_old_epoch_cannot_revive() {
        let broker = ready_broker();
        let (job, rx, _) = job::<()>(&broker);
        broker.close();
        assert!(broker.ready(true, true));
        job.run(true, |_| panic!("old host job must not construct"));
        assert!(rx.try_recv().unwrap().unwrap_err().contains("host changed"));
    }

    #[test]
    fn window_return_broker_panic_and_channel_loss_fail_closed() {
        let broker = ready_broker();
        let (first_job, rx, control) = job::<()>(&broker);
        let epoch = first_job.permit.epoch;
        first_job.run(true, |_| panic!("test construction unwind"));
        assert!(wait_for_result(rx, &broker, epoch, &control, || Ok(()))
            .unwrap_err()
            .contains("panicked"));
        let (job, rx, control) = job::<()>(&broker);
        let epoch = job.permit.epoch;
        drop(job);
        assert!(wait_for_result(rx, &broker, epoch, &control, || Ok(()))
            .unwrap_err()
            .contains("channel closed"));
    }
}
