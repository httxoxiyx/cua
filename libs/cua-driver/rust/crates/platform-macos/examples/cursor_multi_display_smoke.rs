//! Opt-in, draw-only native smoke test. No Driver socket or input events.
//! Run on a logged-in macOS desktop with at least two attached displays:
//! cargo run -p platform-macos --example cursor_multi_display_smoke -- --show-for-test
//! Creates only this process's click-through windows and exits within 20 s.

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("This smoke test requires macOS.");
}

#[cfg(target_os = "macos")]
fn main() {
    if std::env::args().skip(1).collect::<Vec<_>>() != ["--show-for-test"] {
        eprintln!(
            "Pass --show-for-test to briefly draw an overlay on each display; no input is posted."
        );
        return;
    }
    native::run();
}

#[cfg(target_os = "macos")]
mod native {
    use core_graphics::display::CGDisplay;
    use cursor_overlay::{CursorConfig, OverlayCommand};
    use platform_macos::{apps, cursor::overlay, windows};
    use serde_json::json;
    use std::{
        io::Write,
        thread,
        time::{Duration, Instant},
    };

    pub fn run() {
        let displays: Vec<_> = CGDisplay::active_displays()
            .expect("enumerate displays")
            .into_iter()
            .map(|id| {
                let bounds = CGDisplay::new(id).bounds();
                (
                    id,
                    [
                        bounds.origin.x,
                        bounds.origin.y,
                        bounds.size.width,
                        bounds.size.height,
                    ],
                )
            })
            .collect();
        assert!(
            displays.len() >= 2,
            "attach at least two displays before running this test"
        );
        assert!(
            displays.len() <= 8,
            "bounded smoke test supports up to eight displays"
        );
        let prior_front = apps::frontmost_pid();
        assert!(prior_front.is_some(), "a logged-in desktop is required");
        let mut cfg = CursorConfig::default();
        cfg.motion.idle_hide_ms = 0.0;
        overlay::init(cfg);
        thread::spawn(|| {
            thread::sleep(Duration::from_secs(20));
            eprintln!("native multi-display smoke timed out");
            std::process::exit(2);
        });
        thread::spawn(move || {
            let result = std::panic::catch_unwind(|| check(displays, prior_front));
            if let Err(error) = result {
                eprintln!("native multi-display smoke failed: {error:?}");
                std::process::exit(1);
            }
            std::process::exit(0);
        });
        overlay::run_on_main_thread();
    }

    fn check(displays: Vec<(u32, [f64; 4])>, prior_front: Option<i32>) {
        let pid = std::process::id() as i32;
        let deadline = Instant::now() + Duration::from_secs(5);
        let own_windows = loop {
            let owned: Vec<_> = windows::visible_windows()
                .into_iter()
                .filter(|w| w.pid == pid)
                .collect();
            if owned.len() == displays.len()
                && displays.iter().all(|(_, bounds)| {
                    owned
                        .iter()
                        .filter(|window| matches_bounds(window, bounds))
                        .count()
                        == 1
                })
            {
                break owned;
            }
            assert!(
                Instant::now() < deadline,
                "native window bounds did not settle: expected {displays:?}, actual {:?}",
                owned
                    .iter()
                    .map(|window| (window.window_id, &window.bounds))
                    .collect::<Vec<_>>()
            );
            thread::sleep(Duration::from_millis(50));
        };
        for (_, bounds) in &displays {
            assert_eq!(
                own_windows
                    .iter()
                    .filter(|window| matches_bounds(window, bounds))
                    .count(),
                1,
                "missing or duplicate window at {bounds:?}"
            );
        }
        let mut samples = Vec::new();
        for (index, (display_id, bounds)) in displays.iter().enumerate() {
            let [x, y, width, height] = *bounds;
            let target = (x + width * 0.5, y + height * 0.5);
            let key = format!("multi-display-smoke-{index}");
            assert!(overlay::send_command(
                key.clone(),
                OverlayCommand::SetSessionLabel("Display test".into())
            ));
            assert!(overlay::send_command(
                key.clone(),
                OverlayCommand::SnapTo {
                    x: target.0,
                    y: target.1,
                    heading_radians: Some(0.0),
                }
            ));
            let deadline = Instant::now() + Duration::from_secs(2);
            let sample = loop {
                assert_eq!(
                    apps::frontmost_pid(),
                    prior_front,
                    "frontmost application changed during the smoke test"
                );
                let snapshots = surface_snapshot();
                let matching = snapshots.iter().find(|sample| {
                    own_windows.iter().any(|window| {
                        window.window_id as i64 == sample.window_id
                            && (window.bounds.x - x).abs() < 1.0
                            && (window.bounds.y - y).abs() < 1.0
                    })
                });
                if let Some(matching) = matching.filter(|sample| sample.visible_pixels > 0) {
                    assert!(
                        snapshots
                            .iter()
                            .all(|sample| sample.window_id == matching.window_id
                                || sample.visible_pixels == 0),
                        "cursor or badge leaked onto another display"
                    );
                    assert!(overlay::is_visible_for_session(&key));
                    break json!({"display_id":display_id, "bounds":bounds, "window_id":matching.window_id,
                        "image_size":[matching.width, matching.height], "visible_pixels":matching.visible_pixels});
                }
                assert!(
                    Instant::now() < deadline,
                    "no native layer pixels appeared on display {display_id}"
                );
                thread::sleep(Duration::from_millis(50));
            };
            samples.push(sample);
            // Exercise animation too; these are overlay commands, never mouse events.
            assert!(overlay::send_command(
                key.clone(),
                OverlayCommand::MoveTo {
                    x: target.0 + 100.0,
                    y: target.1 + 40.0,
                    end_heading_radians: 0.0,
                }
            ));
            thread::sleep(Duration::from_millis(400));
            overlay::remove_cursor(key.clone());
            let deadline = Instant::now() + Duration::from_secs(2);
            while overlay::is_visible_for_session(&key)
                || surface_snapshot().iter().any(|s| s.visible_pixels > 0)
            {
                assert!(Instant::now() < deadline, "removed cursor was not cleared");
                thread::sleep(Duration::from_millis(50));
            }
        }
        assert_eq!(apps::frontmost_pid(), prior_front);
        println!(
            "{}",
            json!({"result":"passed", "pid":pid, "frontmost_pid_unchanged":true,
            "displays":samples, "input_events_posted":false, "driver_restarted":false,
            "pixel_evidence":"own native CALayer contents, not a desktop screenshot"})
        );
        std::io::stdout().flush().unwrap();
    }

    fn matches_bounds(window: &windows::WindowInfo, bounds: &[f64; 4]) -> bool {
        let actual = [
            window.bounds.x,
            window.bounds.y,
            window.bounds.width,
            window.bounds.height,
        ];
        actual.iter().zip(bounds).all(|(a, b)| (a - b).abs() < 1.0)
    }

    struct Snapshot {
        window_id: i64,
        width: usize,
        height: usize,
        visible_pixels: usize,
    }

    // Read only this process's AppKit windows and their own already-rendered
    // CGImages. This needs no Screen Recording permission and never captures
    // another app's pixels. All AppKit access stays on its owning main queue.
    fn surface_snapshot() -> Vec<Snapshot> {
        use std::{ffi::c_void, sync::mpsc};
        extern "C" {
            static _dispatch_main_q: u8;
            fn dispatch_async_f(
                queue: *const c_void,
                context: *mut c_void,
                callback: unsafe extern "C" fn(*mut c_void),
            );
        }
        unsafe extern "C" fn snapshot(context: *mut c_void) {
            use core_foundation::array::{CFArrayGetCount, CFArrayGetValueAtIndex, CFArrayRef};
            use core_foundation::{base::TCFType, data::CFData};
            use objc2::{class, msg_send, runtime::AnyObject};
            extern "C" {
                fn CGImageGetWidth(image: *const c_void) -> usize;
                fn CGImageGetHeight(image: *const c_void) -> usize;
                fn CGImageGetDataProvider(image: *const c_void) -> *const c_void;
                fn CGDataProviderCopyData(
                    provider: *const c_void,
                ) -> core_foundation::data::CFDataRef;
            }
            let sender: Box<mpsc::Sender<Vec<Snapshot>>> = Box::from_raw(context.cast());
            let app: *mut AnyObject = msg_send![class!(NSApplication), sharedApplication];
            let windows: *mut AnyObject = msg_send![app, windows];
            let windows = windows as CFArrayRef;
            let count = CFArrayGetCount(windows);
            let mut samples = Vec::new();
            for i in 0..count {
                let window = CFArrayGetValueAtIndex(windows, i) as *mut AnyObject;
                let window_id: i64 = msg_send![window, windowNumber];
                let click_through: bool = msg_send![window, ignoresMouseEvents];
                assert!(click_through, "smoke overlay window must be click-through");
                let view: *mut AnyObject = msg_send![window, contentView];
                let layer: *mut AnyObject = msg_send![view, layer];
                let image: *const AnyObject = msg_send![layer, contents];
                let mut sample = Snapshot {
                    window_id,
                    width: 0,
                    height: 0,
                    visible_pixels: 0,
                };
                if !image.is_null() {
                    let image = image.cast::<c_void>();
                    sample.width = CGImageGetWidth(image);
                    sample.height = CGImageGetHeight(image);
                    let provider = CGImageGetDataProvider(image);
                    if !provider.is_null() {
                        let data = CGDataProviderCopyData(provider);
                        if !data.is_null() {
                            let data = CFData::wrap_under_create_rule(data);
                            sample.visible_pixels = data
                                .bytes()
                                .chunks_exact(4)
                                .filter(|rgba| rgba[3] > 96)
                                .count();
                        }
                    }
                }
                samples.push(sample);
            }
            let _ = sender.send(samples);
        }
        let (sender, receiver) = mpsc::channel();
        unsafe {
            dispatch_async_f(
                (&raw const _dispatch_main_q).cast(),
                Box::into_raw(Box::new(sender)).cast(),
                snapshot,
            );
        }
        receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("AppKit snapshot timeout")
    }
}
