//! Sequencing for one exact-window background key operation.
//!
//! The platform context owns target-only RAII cleanup. This layer has no
//! foreground-activation or retry primitive, and dispatch is a `FnOnce`.

/// Experimental exact-background sequencing. This does not relax the existing
/// singleton gate. The native context must own any preparation state and its
/// panic/unwind cleanup, and cleanup failures must remain visible even when an
/// earlier stage already failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExactBackgroundStage {
    Preparation,
    Readiness,
    Dispatch,
    Settlement,
    Cleanup,
}

#[derive(Debug)]
pub(crate) struct ExactBackgroundFailure<E> {
    pub stage: ExactBackgroundStage,
    /// Conservative: dispatch was entered, not proof that the application
    /// consumed a key. No error with this set authorizes replay.
    pub dispatch_entered: bool,
    pub cleanup_confirmed: bool,
    pub error: E,
    pub cleanup_error: Option<E>,
}

pub(super) fn exact_background_ax_context_matches(
    expected_window: u32,
    observed_window: u32,
    same_field: bool,
) -> bool {
    expected_window == observed_window && same_field
}

pub(super) fn dispatch_exact_background_once<C, R, E>(
    mut context: C,
    prepare: impl FnOnce(&mut C) -> Result<(), E>,
    ready: impl FnOnce(&mut C) -> Result<(), E>,
    dispatch: impl FnOnce(&mut C) -> Result<R, E>,
    settle: impl FnOnce(&mut C) -> Result<(), E>,
    cleanup: impl FnOnce(&mut C) -> Result<(), E>,
) -> Result<R, ExactBackgroundFailure<E>> {
    let mut dispatch_entered = false;
    let result = (|| {
        prepare(&mut context).map_err(|error| (ExactBackgroundStage::Preparation, error))?;
        ready(&mut context).map_err(|error| (ExactBackgroundStage::Readiness, error))?;
        dispatch_entered = true;
        let result = dispatch(&mut context);
        // A failed dispatch can still have posted a key. Always settle before
        // removing its target-local routing context, and never dispatch twice.
        let settlement = settle(&mut context);
        match result {
            Err(error) => Err((ExactBackgroundStage::Dispatch, error)),
            Ok(value) => settlement
                .map(|()| value)
                .map_err(|error| (ExactBackgroundStage::Settlement, error)),
        }
    })();
    let cleanup_result = cleanup(&mut context);
    match (result, cleanup_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(error)) => Err(ExactBackgroundFailure {
            stage: ExactBackgroundStage::Cleanup,
            dispatch_entered,
            cleanup_confirmed: false,
            error,
            cleanup_error: None,
        }),
        (Err((stage, error)), cleanup_result) => Err(ExactBackgroundFailure {
            stage,
            dispatch_entered,
            cleanup_confirmed: cleanup_result.is_ok(),
            error,
            cleanup_error: cleanup_result.err(),
        }),
    }
}

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
    use super::{
        dispatch_exact_background_once, dispatch_with_target_context,
        exact_background_ax_context_matches, ExactBackgroundStage,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    type Events = Rc<RefCell<Vec<&'static str>>>;

    #[test]
    fn exact_background_readiness_rejects_sibling_window_or_different_field() {
        assert!(exact_background_ax_context_matches(12, 12, true));
        assert!(!exact_background_ax_context_matches(12, 13, true));
        assert!(!exact_background_ax_context_matches(12, 12, false));
        assert!(!exact_background_ax_context_matches(12, 13, false));
    }

    fn exact_run(
        failed_stage: Option<ExactBackgroundStage>,
        cleanup_fails: bool,
        events: &Events,
    ) -> Result<(), super::ExactBackgroundFailure<&'static str>> {
        let step = |name, stage| {
            events.borrow_mut().push(name);
            if failed_stage == Some(stage) {
                Err(name)
            } else {
                Ok(())
            }
        };
        dispatch_exact_background_once(
            (),
            |_| step("prepare", ExactBackgroundStage::Preparation),
            |_| step("ready", ExactBackgroundStage::Readiness),
            |_| step("dispatch", ExactBackgroundStage::Dispatch),
            |_| step("settle", ExactBackgroundStage::Settlement),
            |_| {
                events.borrow_mut().push("cleanup");
                if cleanup_fails {
                    Err("cleanup")
                } else {
                    Ok(())
                }
            },
        )
    }

    #[test]
    fn exact_background_dispatches_once_only_after_readiness() {
        let events = Events::default();
        exact_run(None, false, &events).unwrap();
        assert_eq!(
            *events.borrow(),
            ["prepare", "ready", "dispatch", "settle", "cleanup"]
        );
    }

    #[test]
    fn exact_background_prepare_or_readiness_failure_never_dispatches() {
        for stage in [
            ExactBackgroundStage::Preparation,
            ExactBackgroundStage::Readiness,
        ] {
            let events = Events::default();
            let error = exact_run(Some(stage), false, &events).unwrap_err();
            assert_eq!(error.stage, stage);
            assert!(!error.dispatch_entered);
            assert!(error.cleanup_confirmed);
            assert!(!events.borrow().contains(&"dispatch"));
            assert_eq!(events.borrow().last(), Some(&"cleanup"));
        }
    }

    #[test]
    fn exact_background_attempted_failure_settles_without_replay() {
        for stage in [
            ExactBackgroundStage::Dispatch,
            ExactBackgroundStage::Settlement,
        ] {
            let events = Events::default();
            let error = exact_run(Some(stage), false, &events).unwrap_err();
            assert_eq!(error.stage, stage);
            assert!(error.dispatch_entered);
            assert!(error.cleanup_confirmed);
            assert_eq!(
                events
                    .borrow()
                    .iter()
                    .filter(|&&value| value == "dispatch")
                    .count(),
                1
            );
            assert_eq!(&events.borrow()[3..], ["settle", "cleanup"]);
        }
    }

    #[test]
    fn exact_background_cleanup_error_is_not_hidden_by_primary_failure() {
        let events = Events::default();
        let error = exact_run(Some(ExactBackgroundStage::Readiness), true, &events).unwrap_err();
        assert!(!error.dispatch_entered);
        assert!(!error.cleanup_confirmed);
        assert_eq!(error.error, "ready");
        assert_eq!(error.cleanup_error, Some("cleanup"));
        let error = exact_run(None, true, &events).unwrap_err();
        assert!(error.dispatch_entered);
        assert!(!error.cleanup_confirmed);
        assert_eq!(error.stage, ExactBackgroundStage::Cleanup);
    }

    #[test]
    fn exact_background_panic_retains_context_until_its_drop_cleanup() {
        let events = Events::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: Result<(), super::ExactBackgroundFailure<&str>> = dispatch_exact_background_once(
                Context {
                    events: Rc::clone(&events),
                    ended: false,
                },
                |_| Ok(()),
                |_| Ok(()),
                |_| panic!("dispatch panic"),
                |_| Ok(()),
                |context| {
                    context.ended = true;
                    Ok(())
                },
            );
        }));
        assert!(result.is_err());
        assert_eq!(*events.borrow(), ["drop_cleanup"]);
    }

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
