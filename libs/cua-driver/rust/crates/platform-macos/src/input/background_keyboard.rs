//! Sequencing for one exact-window background key operation.
//!
//! The platform context owns target-only RAII cleanup. This layer has no
//! foreground-activation or retry primitive, and dispatch is a `FnOnce`.

pub(super) fn dispatch_with_target_context<C, R, E>(
    target_is_frontmost: bool,
    begin: impl FnOnce() -> Result<C, E>,
    make_key: impl FnOnce(&C) -> Result<(), E>,
    dispatch: impl FnOnce() -> Result<R, E>,
    settle: impl FnOnce(),
    end: impl FnOnce(C) -> Result<(), E>,
) -> Result<R, E> {
    if target_is_frontmost {
        return dispatch();
    }
    let context = begin()?;
    if let Err(error) = make_key(&context) {
        // Cleanup is still attempted before returning the preparation error.
        // The owned context's Drop remains the panic/unwind safety net.
        let _ = end(context);
        return Err(error);
    }
    let result = dispatch();
    settle();
    let cleanup = end(context);
    match result {
        Err(error) => Err(error),
        Ok(value) => cleanup.map(|()| value),
    }
}

#[cfg(test)]
mod tests {
    use super::dispatch_with_target_context;
    use std::cell::RefCell;
    use std::rc::Rc;

    type Events = Rc<RefCell<Vec<&'static str>>>;

    struct Context {
        events: Events,
        ended: bool,
    }

    impl Drop for Context {
        fn drop(&mut self) {
            if !self.ended {
                self.events.borrow_mut().push("drop_cleanup");
            }
        }
    }

    fn execute(
        frontmost: bool,
        failure: Option<&str>,
        events: &Events,
    ) -> Result<u8, &'static str> {
        dispatch_with_target_context(
            frontmost,
            || {
                events.borrow_mut().push("begin");
                if failure == Some("begin") {
                    Err("begin failed")
                } else {
                    Ok(Context {
                        events: Rc::clone(events),
                        ended: false,
                    })
                }
            },
            |_| {
                events.borrow_mut().push("make_key");
                if failure == Some("make_key") {
                    Err("make_key failed")
                } else if failure == Some("prepare_panic") {
                    panic!("prepare panic");
                } else {
                    Ok(())
                }
            },
            || {
                events.borrow_mut().push("authenticated_dispatch");
                if failure == Some("dispatch") {
                    Err("dispatch failed")
                } else if failure == Some("dispatch_panic") {
                    panic!("dispatch panic");
                } else {
                    Ok(7)
                }
            },
            || events.borrow_mut().push("settle"),
            |mut context| {
                events.borrow_mut().push("end");
                context.ended = true;
                if failure == Some("cleanup") {
                    Err("cleanup failed")
                } else {
                    Ok(())
                }
            },
        )
    }

    #[test]
    fn background_context_is_key_before_one_authenticated_dispatch_and_ends_after_settle() {
        let events = Events::default();
        assert_eq!(execute(false, None, &events), Ok(7));
        assert_eq!(
            *events.borrow(),
            [
                "begin",
                "make_key",
                "authenticated_dispatch",
                "settle",
                "end"
            ]
        );
    }

    #[test]
    fn real_foreground_target_dispatches_without_synthetic_activation() {
        let events = Events::default();
        assert_eq!(execute(true, None, &events), Ok(7));
        assert_eq!(*events.borrow(), ["authenticated_dispatch"]);
    }

    #[test]
    fn begin_failure_never_dispatches_or_retries() {
        let events = Events::default();
        assert_eq!(execute(false, Some("begin"), &events), Err("begin failed"));
        assert_eq!(*events.borrow(), ["begin"]);
    }

    #[test]
    fn key_window_failure_ends_context_without_dispatch() {
        let events = Events::default();
        assert_eq!(
            execute(false, Some("make_key"), &events),
            Err("make_key failed")
        );
        assert_eq!(*events.borrow(), ["begin", "make_key", "end"]);
    }

    #[test]
    fn dispatch_failure_still_settles_and_ends_without_replay() {
        let events = Events::default();
        assert_eq!(
            execute(false, Some("dispatch"), &events),
            Err("dispatch failed")
        );
        assert_eq!(
            *events.borrow(),
            [
                "begin",
                "make_key",
                "authenticated_dispatch",
                "settle",
                "end"
            ]
        );
    }

    #[test]
    fn cleanup_failure_is_not_retried_or_reported_as_success() {
        let events = Events::default();
        assert_eq!(
            execute(false, Some("cleanup"), &events),
            Err("cleanup failed")
        );
        assert_eq!(
            *events.borrow(),
            [
                "begin",
                "make_key",
                "authenticated_dispatch",
                "settle",
                "end"
            ]
        );
    }

    #[test]
    fn preparation_panic_drops_the_owned_context() {
        let events = Events::default();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(false, Some("prepare_panic"), &events)
        }));
        assert!(caught.is_err());
        assert_eq!(*events.borrow(), ["begin", "make_key", "drop_cleanup"]);
    }

    #[test]
    fn dispatch_panic_drops_the_owned_context() {
        let events = Events::default();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            execute(false, Some("dispatch_panic"), &events)
        }));
        assert!(caught.is_err());
        assert_eq!(
            *events.borrow(),
            [
                "begin",
                "make_key",
                "authenticated_dispatch",
                "drop_cleanup"
            ]
        );
    }
}
