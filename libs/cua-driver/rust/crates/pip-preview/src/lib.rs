//! pip-preview — shared types + trait for the experimental
//! picture-in-picture agent preview stack.
//!
//! PiP is opt-in and only receives exact application-window targets from
//! Computer Use observations and actions. Platform backends may provide a live
//! preview; the shared frame hook remains available as a compatibility fallback.
//! It mirrors the architecture used by
//! `cursor-overlay` (shared
//! config/types here, platform-specific renderer in each `platform-*`
//! crate) and the registration pattern used by `cua_driver_core::video`
//! (a `OnceLock` factory set once at startup by `main.rs`).
//!
//! macOS is the first working implementation (NSWindow + NSImageView).
//! Windows + Linux ship as compile-clean stubs whose `start()` returns
//! a clear "not yet implemented" error so the rest of the daemon
//! continues without a PiP window.

pub mod temporary_activation;

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// Fit a complete source window inside the preview bounds without cropping or
/// stretching. Platform adapters use the current image dimensions, including
/// after the source window changes shape. An unavailable image fills the bounds.
pub fn fit_preview_size(bounds: (f64, f64), source: (f64, f64)) -> (f64, f64) {
    let positive = |value: f64| {
        if value.is_finite() {
            value.max(0.0)
        } else {
            0.0
        }
    };
    let (width, height) = (positive(bounds.0), positive(bounds.1));
    if !source.0.is_finite() || !source.1.is_finite() || source.0 <= 0.0 || source.1 <= 0.0 {
        return (width, height);
    }
    let scale = (width / source.0).min(height / source.1);
    (source.0 * scale, source.1 * scale)
}

/// Canonical `~/.cua-driver/config.json` path matching what the per-platform
/// `set_config` tools write to. Resolves `$HOME` first (Unix/macOS) and falls
/// back to `%USERPROFILE%` (Windows, where `HOME` is usually unset). Returns
/// `None` when neither is set (sandboxed CI).
pub fn default_config_path() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|h| {
            std::path::PathBuf::from(h)
                .join(".cua-driver")
                .join("config.json")
        })
}

/// Read a single key from `~/.cua-driver/config.json` as a raw JSON value,
/// returning `None` when the file is missing/malformed or the key is absent.
/// Used by the per-platform `load_driver_config` helpers to rehydrate the
/// in-memory `DriverConfig` at process startup so `set_config` writes survive
/// across stateless `cua-driver call` invocations.
pub fn read_config_value(key: &str) -> Option<serde_json::Value> {
    let path = default_config_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    json.get(key).cloned()
}

/// Merge a single `key`/`value` into `~/.cua-driver/config.json`,
/// preserving any other keys that are already there. Used by the
/// per-platform `set_config` tools to persist `experimental_pip` /
/// `experimental_pip_geometry` so the next daemon restart picks them up.
pub fn write_config_key(key: &str, value: serde_json::Value) -> Result<(), String> {
    let path = default_config_path().ok_or_else(|| "$HOME is not set".to_string())?;
    let mut json: serde_json::Value = path
        .exists()
        .then(|| std::fs::read_to_string(&path).ok())
        .flatten()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    json[key] = value;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let body = serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?;
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(())
}

/// Read `experimental_pip` + `experimental_pip_geometry` from the
/// config file, falling back to defaults when missing or malformed.
/// Surfaced by the per-platform `get_config` tools alongside the
/// in-memory `DriverConfig` fields.
pub fn read_pip_keys_from_file() -> (bool, Option<String>) {
    let path = match default_config_path() {
        Some(p) => p,
        None => return (false, None),
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return (false, None),
    };
    let json: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return (false, None),
    };
    let enabled = json
        .get("experimental_pip")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let geometry = json
        .get("experimental_pip_geometry")
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned());
    (enabled, geometry)
}

/// Geometry of the PiP window, in screen points (top-left origin).
///
/// Parsed from `--experimental-pip-geometry WxH+X+Y`. `x` / `y` are
/// optional; when `None` the platform backend picks a sensible
/// "top-right corner with a small inset" default so a user enabling
/// the feature without any geometry flags still sees a window.
#[derive(Debug, Clone, Copy)]
pub struct PipGeometry {
    pub width: u32,
    pub height: u32,
    pub x: Option<i32>,
    pub y: Option<i32>,
}

impl Default for PipGeometry {
    fn default() -> Self {
        Self {
            width: 320,
            height: 200,
            x: None,
            y: None,
        }
    }
}

impl PipGeometry {
    /// Parse `WxH` or `WxH+X+Y` (matching the common X11 geometry form).
    /// Returns `None` on any parse failure so the caller can fall back
    /// to defaults without panicking.
    pub fn parse(s: &str) -> Option<Self> {
        // Split off the optional `+X+Y` tail first so the leading
        // `WxH` parses cleanly even when no position is provided.
        let (size, pos): (&str, Option<(i32, i32)>) = match s.find('+') {
            Some(i) => {
                let tail = &s[i + 1..];
                let mut parts = tail.split('+');
                let x = parts.next()?.parse().ok()?;
                let y = parts.next()?.parse().ok()?;
                (&s[..i], Some((x, y)))
            }
            None => (s, None),
        };
        let mut wh = size.split('x');
        let w: u32 = wh.next()?.parse().ok()?;
        let h: u32 = wh.next()?.parse().ok()?;
        Some(Self {
            width: w,
            height: h,
            x: pos.map(|p| p.0),
            y: pos.map(|p| p.1),
        })
    }
}

/// Configuration for the PiP window. Built by `main.rs` from CLI
/// flags and handed to `PipBackendFactory::start`.
#[derive(Debug, Clone)]
pub struct PipConfig {
    /// `--experimental-pip` is on argv. The factory is only consulted
    /// when this is true; the field is kept here so backends that
    /// share a `start()` path can early-return.
    pub enabled: bool,
    pub geometry: PipGeometry,
    /// Window title — kept here so the "experimental" label stays in
    /// one place. Defaults to "cua-driver — PiP preview (experimental)".
    pub title: String,
}

impl Default for PipConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            geometry: PipGeometry::default(),
            title: "cua-driver — PiP preview (experimental)".to_owned(),
        }
    }
}

impl PipConfig {
    /// Parse the PiP-related CLI flags out of `std::env::args()`.
    /// Recognised flags (all opt-in):
    /// ```text
    /// --experimental-pip
    /// --pip                        (short alias)
    /// --experimental-pip-geometry  WxH | WxH+X+Y
    /// ```
    /// Unknown flags are ignored so this never conflicts with the
    /// other arg-parser passes (CursorConfig, the subcommand router).
    pub fn from_args() -> Self {
        let args: Vec<String> = std::env::args().collect();
        Self::parse(&args[1..])
    }

    pub fn parse(args: &[String]) -> Self {
        let mut cfg = PipConfig::default();
        let mut i = 0usize;
        while i < args.len() {
            match args[i].as_str() {
                "--experimental-pip" | "--pip" => cfg.enabled = true,
                "--experimental-pip-geometry" => {
                    if let Some(geom) = args.get(i + 1).and_then(|s| PipGeometry::parse(s)) {
                        cfg.geometry = geom;
                        i += 1;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        cfg
    }

    /// Resolve the config from (in order of precedence, low → high):
    ///
    ///   defaults  →  `~/.cua-driver/config.json` keys
    ///                  (`experimental_pip` bool, `experimental_pip_geometry` string)
    ///              →  CLI flags
    ///
    /// Lets users persist `--experimental-pip` across daemon restarts by
    /// editing `~/.cua-driver/config.json` once, instead of re-running
    /// `claude mcp add` with the flag baked into the args list.
    /// Malformed or missing file falls back to the next layer silently.
    pub fn from_args_and_file(config_path: &std::path::Path) -> Self {
        let mut cfg = PipConfig::default();
        if let Ok(text) = std::fs::read_to_string(config_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) {
                if let Some(b) = json.get("experimental_pip").and_then(|v| v.as_bool()) {
                    cfg.enabled = b;
                }
                if let Some(s) = json
                    .get("experimental_pip_geometry")
                    .and_then(|v| v.as_str())
                {
                    if let Some(g) = PipGeometry::parse(s) {
                        cfg.geometry = g;
                    }
                }
            }
        }
        // CLI args override anything in the file.
        let args: Vec<String> = std::env::args().collect();
        let cli = PipConfig::parse(&args[1..]);
        if cli.enabled {
            cfg.enabled = true;
        }
        // CLI geometry only overrides when explicitly passed — detect by
        // diffing against PipGeometry::default() (the parse() entry point
        // returns the default when no flag is present).
        if cli.geometry.width != PipGeometry::default().width
            || cli.geometry.height != PipGeometry::default().height
            || cli.geometry.x.is_some()
            || cli.geometry.y.is_some()
        {
            cfg.geometry = cli.geometry;
        }
        cfg
    }
}

/// Maximum number of simultaneously retained application previews.
///
/// This matches the visible stack limit in the OpenAI Codex desktop client and
/// keeps capture streams, native views, and memory use bounded.
pub const MAX_VISIBLE_PIP_CARDS: usize = 5;

/// The exact native target represented by one application card.
///
/// Cards are keyed by [`PipTarget::app_key_pid`], matching the user-facing
/// "one card per app" model. `pid`/`window_id` remain the actual visual capture
/// target and may point at a trusted delegated helper window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipDelegation {
    pub kind: String,
    pub host_pid: i64,
    pub panel_kind: String,
    pub expected_bundle_id: Option<String>,
    pub expected_app_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipTarget {
    /// Logical application owner used for one-card-per-app identity,
    /// foreground suppression, and user-facing naming. A delegated native
    /// helper window keeps its helper pid in `pid` for capture but sets this to
    /// the host application's pid.
    pub logical_pid: Option<i64>,
    /// Revalidation material for a trusted delegated visual target. Platform
    /// backends must re-prove this association before continuing capture or
    /// activating the helper window.
    pub delegation: Option<PipDelegation>,
    pub pid: i64,
    pub window_id: u64,
    /// Internal runtime session that observed this target. Several sessions may
    /// share an application card while retaining their own exact targets.
    /// Kept out of the rendered UI.
    pub session_id: Option<String>,
    pub app_name: String,
    pub window_title: Option<String>,
}

impl PipTarget {
    pub fn app_key_pid(&self) -> i64 {
        self.logical_pid.filter(|pid| *pid > 0).unwrap_or(self.pid)
    }
}

/// A single exact-target seed/fallback frame authorized by an observation.
///
/// `png_bytes` reuse the platform observation response when it contains an
/// image, or are privately captured after a successful tree-only observation.
/// Platforms with native live capture use this frame to create the card and
/// then replace it with stream frames as they arrive.
#[derive(Debug, Clone)]
pub struct PipFrame {
    pub target: PipTarget,
    pub png_bytes: Vec<u8>,
    /// Wall-clock timestamp (ms since Unix epoch) — used by backends
    /// that want to show "last update Xs ago" in the title bar.
    pub timestamp_ms: u64,
}

/// Visible card changes after updating a session's target or retained frames.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PipModelChange {
    /// Apps with no remaining frame. Their native views and streams can stop.
    pub removed_pids: Vec<i64>,
    /// Apps whose representative frame appeared or changed, including a switch
    /// to another session's frame. Backends must revalidate that exact target.
    pub changed_pids: Vec<i64>,
}

impl PipModelChange {
    pub fn is_empty(&self) -> bool {
        self.removed_pids.is_empty() && self.changed_pids.is_empty()
    }
}

/// Result of inserting a frame into [`PipViewModel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipUpsert {
    /// False when a delayed frame no longer matches its session's selection.
    pub accepted: bool,
    /// The least-recently-updated app evicted to preserve the configured cap.
    pub evicted_pid: Option<i64>,
    /// Whether this app's exact root window changed and its live stream should
    /// be replaced.
    pub window_changed: bool,
    pub change: PipModelChange,
}

struct PublishedFrame {
    frame: PipFrame,
    sequence: u64,
}

/// Platform-neutral card stack for the current target of each runtime session.
///
/// Selecting another target retires only that session's prior frame. Sessions
/// observing the same app retain independent frames; the most recently
/// published surviving frame represents the app's single visible card.
pub struct PipViewModel {
    max_cards: usize,
    selected_apps: HashMap<String, i64>,
    frames_by_session: HashMap<String, Arc<PublishedFrame>>,
    anonymous_frames: HashMap<i64, Arc<PublishedFrame>>,
    frames_by_pid: HashMap<i64, Arc<PublishedFrame>>,
    publication_order: Vec<i64>,
    next_sequence: u64,
}

impl PipViewModel {
    pub fn new(max_cards: usize) -> Self {
        Self {
            max_cards: max_cards.max(1),
            selected_apps: HashMap::new(),
            frames_by_session: HashMap::new(),
            anonymous_frames: HashMap::new(),
            frames_by_pid: HashMap::new(),
            publication_order: Vec::new(),
            next_sequence: 0,
        }
    }

    /// Commit a successful observation's logical app before asynchronous preview
    /// capture. Re-selecting the same app leaves its frame and card intact.
    /// This does not itself authorize or capture any pixels.
    pub fn select_target(&mut self, target: &PipTarget) -> PipModelChange {
        let Some(session_id) = target.session_id.as_ref() else {
            // Sessionless compatibility callers keep the original per-app
            // behavior; they have no shared lifecycle key to retarget.
            return PipModelChange::default();
        };
        if self
            .selected_apps
            .get(session_id)
            .is_some_and(|selected| *selected == target.app_key_pid())
        {
            return PipModelChange::default();
        }
        self.selected_apps
            .insert(session_id.clone(), target.app_key_pid());
        let previous = self.frames_by_session.remove(session_id);
        previous.map_or_else(PipModelChange::default, |previous| {
            self.refresh_apps(vec![previous.frame.target.app_key_pid()])
        })
    }

    /// Whether a frame still belongs to its session's current logical app.
    /// Backends must additionally validate publication generations and session
    /// liveness, including changes between windows or menu sources in that app
    /// and a session ending or returning to an earlier app.
    pub fn accepts_target(&self, target: &PipTarget) -> bool {
        target
            .session_id
            .as_ref()
            .and_then(|session_id| self.selected_apps.get(session_id))
            .is_none_or(|selected| *selected == target.app_key_pid())
    }

    pub fn upsert(&mut self, frame: PipFrame) -> PipUpsert {
        if !self.accepts_target(&frame.target) {
            return PipUpsert {
                accepted: false,
                evicted_pid: None,
                window_changed: false,
                change: PipModelChange::default(),
            };
        }
        let pid = frame.target.app_key_pid();
        let window_changed = self.frames_by_pid.get(&pid).is_some_and(|previous| {
            previous.frame.target.pid != frame.target.pid
                || previous.frame.target.window_id != frame.target.window_id
        });
        self.next_sequence += 1;
        let published = Arc::new(PublishedFrame {
            frame,
            sequence: self.next_sequence,
        });
        if let Some(session_id) = published.frame.target.session_id.as_ref() {
            // Preserve compatibility with callers whose first publication is
            // the selection. Once selected, upsert never changes that app.
            self.selected_apps.entry(session_id.clone()).or_insert(pid);
            self.frames_by_session.insert(session_id.clone(), published);
        } else {
            self.anonymous_frames.insert(pid, published);
        }
        let mut change = self.refresh_apps(vec![pid]);

        let evicted_pid = if self.frames_by_pid.len() > self.max_cards {
            self.frames_by_pid
                .iter()
                .filter(|(candidate_pid, _)| **candidate_pid != pid)
                .min_by_key(|(candidate_pid, candidate)| {
                    (candidate.frame.timestamp_ms, **candidate_pid)
                })
                .map(|(candidate_pid, _)| *candidate_pid)
        } else {
            None
        };
        if let Some(evicted_pid) = evicted_pid {
            self.remove_app(evicted_pid);
            change.removed_pids.push(evicted_pid);
        }

        PipUpsert {
            accepted: true,
            evicted_pid,
            window_changed,
            change,
        }
    }

    /// Clear every retained frame for an app, without revoking the sessions'
    /// current selections. A future successful observation may seed it again.
    pub fn remove_app(&mut self, pid: i64) -> bool {
        self.frames_by_session
            .retain(|_, published| published.frame.target.app_key_pid() != pid);
        self.anonymous_frames.remove(&pid);
        !self.refresh_apps(vec![pid]).removed_pids.is_empty()
    }

    /// Clear a failed preview's own frame, preserving other sessions' previews
    /// for the same app and any newer target selected by this session.
    pub fn remove_target_frame(&mut self, target: &PipTarget) -> PipModelChange {
        let pid = target.app_key_pid();
        if let Some(session_id) = target.session_id.as_ref() {
            if self
                .frames_by_session
                .get(session_id)
                .is_some_and(|published| same_frame_target(&published.frame.target, target))
            {
                self.frames_by_session.remove(session_id);
            }
        } else if self
            .anonymous_frames
            .get(&pid)
            .is_some_and(|published| same_frame_target(&published.frame.target, target))
        {
            self.anonymous_frames.remove(&pid);
        }
        self.refresh_apps(vec![pid])
    }

    /// Validate all retained session frames, including those currently hidden
    /// by another session's newer frame for the same application.
    pub fn retained_targets(&self) -> Vec<&PipTarget> {
        self.frames_by_session
            .values()
            .chain(self.anonymous_frames.values())
            .map(|published| &published.frame.target)
            .collect()
    }

    /// Drop frames whose exact target is no longer live, selecting another
    /// session's surviving frame for the app when possible. The caller must
    /// validate every retained target, not only the currently rendered frames.
    pub fn retain_live_targets(
        &mut self,
        mut target_is_live: impl FnMut(&PipTarget) -> bool,
    ) -> PipModelChange {
        let mut affected = Vec::new();
        self.frames_by_session.retain(|_, published| {
            let live = target_is_live(&published.frame.target);
            if !live {
                affected.push(published.frame.target.app_key_pid());
            }
            live
        });
        self.anonymous_frames.retain(|_, published| {
            let live = target_is_live(&published.frame.target);
            if !live {
                affected.push(published.frame.target.app_key_pid());
            }
            live
        });
        self.refresh_apps(affected)
    }

    /// End only this session's selection and frame. Other sessions sharing its
    /// app remain represented by their own most recent successful observation.
    pub fn remove_session(&mut self, session_id: &str) -> PipModelChange {
        self.selected_apps.remove(session_id);
        let previous = self.frames_by_session.remove(session_id);
        previous.map_or_else(PipModelChange::default, |previous| {
            self.refresh_apps(vec![previous.frame.target.app_key_pid()])
        })
    }

    fn refresh_apps(&mut self, mut affected: Vec<i64>) -> PipModelChange {
        affected.sort_unstable();
        affected.dedup();
        let mut change = PipModelChange::default();
        for pid in affected {
            let representative = self
                .frames_by_session
                .values()
                .chain(self.anonymous_frames.values())
                .filter(|published| published.frame.target.app_key_pid() == pid)
                .max_by_key(|published| published.sequence)
                .cloned();
            match representative {
                Some(representative) => {
                    let previous = self.frames_by_pid.get(&pid);
                    if previous.is_none_or(|previous| !Arc::ptr_eq(previous, &representative)) {
                        change.changed_pids.push(pid);
                        if previous.is_none() {
                            self.publication_order.push(pid);
                        }
                        self.frames_by_pid.insert(pid, representative);
                    }
                }
                None => {
                    if self.frames_by_pid.remove(&pid).is_some() {
                        self.publication_order
                            .retain(|published_pid| *published_pid != pid);
                        change.removed_pids.push(pid);
                    }
                }
            }
        }
        change
    }

    /// Move an existing app to the front of the visual card stack.
    ///
    /// The platform renderer paints publication order back-to-front, so the
    /// final entry is the card that receives the full-size foreground slot.
    pub fn promote_app(&mut self, pid: i64) -> bool {
        let Some(index) = self
            .publication_order
            .iter()
            .position(|published_pid| *published_pid == pid)
        else {
            return false;
        };
        if index + 1 == self.publication_order.len() {
            return false;
        }
        self.publication_order.remove(index);
        self.publication_order.push(pid);
        true
    }

    pub fn frame_for_app(&self, pid: i64) -> Option<&PipFrame> {
        self.frames_by_pid
            .get(&pid)
            .map(|published| &published.frame)
    }

    pub fn ordered_frames(&self) -> Vec<&PipFrame> {
        self.publication_order
            .iter()
            .filter_map(|pid| self.frame_for_app(*pid))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.frames_by_pid.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frames_by_pid.is_empty()
    }
}

fn same_frame_target(left: &PipTarget, right: &PipTarget) -> bool {
    left.pid == right.pid
        && left.window_id == right.window_id
        && left.logical_pid == right.logical_pid
        && left.delegation == right.delegation
        && left.session_id == right.session_id
}

/// A live PiP window. Owned by `main.rs` for the lifetime of the
/// process; `shutdown()` consumes it and closes the window.
pub trait PipBackend: Send + Sync {
    /// Push a new frame to the window. Non-blocking; the backend is
    /// responsible for dispatching the actual draw to whatever thread
    /// its UI toolkit requires (the macOS impl dispatches to the main
    /// queue via `dispatch_async`).
    fn push_frame(&self, frame: PipFrame);

    /// Ensure an exact target is represented by a live preview. Implementations
    /// must not synchronously capture on the caller's tool-dispatch path.
    fn ensure_target(&self, _target: PipTarget) {}

    /// A successful tree-only observation may bootstrap a private preview.
    /// It grants no model image or pointer coordinates and must not block the
    /// observation. Action-only Ensure requests do not carry this authority.
    fn observe_target(&self, _target: PipTarget) {}

    /// Remove cards owned by a runtime session that has ended.
    fn end_session(&self, _session_id: &str) {}

    /// Synchronously make the presentation input-transparent while Computer
    /// Use performs a physical desktop action. This prevents an overlapping
    /// card from intercepting a click intended for the controlled app.
    fn set_input_passthrough(&self, _passthrough: bool) -> anyhow::Result<()> {
        Ok(())
    }

    /// Close the window and release native resources. Called from
    /// `main.rs` on shutdown.
    fn shutdown(self: Box<Self>);
}

/// Spawns a fresh PiP window. Registered once at startup via
/// `set_pip_backend_factory`.
pub trait PipBackendFactory: Send + Sync {
    fn start(&self, cfg: &PipConfig) -> anyhow::Result<Box<dyn PipBackend>>;
}

static PIP_FACTORY: OnceLock<Box<dyn PipBackendFactory>> = OnceLock::new();

/// Register the platform's PiP backend factory. Idempotent — subsequent
/// calls are silently ignored, matching the other startup-callback
/// setters in `cua_driver_core`.
pub fn set_pip_backend_factory(factory: Box<dyn PipBackendFactory>) {
    let _ = PIP_FACTORY.set(factory);
}

/// Start a PiP window using the registered backend. Returns an error
/// when no backend has been registered for this platform — `main.rs`
/// treats that as "PiP unavailable on this OS" and continues without
/// the window.
pub fn start_pip(cfg: &PipConfig) -> anyhow::Result<Box<dyn PipBackend>> {
    let factory = PIP_FACTORY
        .get()
        .ok_or_else(|| anyhow::anyhow!("no PiP backend registered for this platform"))?;
    factory.start(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_fits_portrait_landscape_square_and_extreme_ratios() {
        for source in [
            (300.0, 500.0),
            (850.0, 500.0),
            (500.0, 500.0),
            (8000.0, 100.0),
        ] {
            let fitted = fit_preview_size((580.0, 332.0), source);
            assert!(fitted.0 <= 580.0 && fitted.1 <= 332.0);
            assert!((fitted.0 / fitted.1 - source.0 / source.1).abs() < 0.000_001);
            assert!((fitted.0 - 580.0).abs() < 0.000_001 || (fitted.1 - 332.0).abs() < 0.000_001);
        }
    }

    #[test]
    fn preview_with_unavailable_image_or_bounds_has_finite_geometry() {
        for source in [
            (0.0, 100.0),
            (100.0, -1.0),
            (f64::NAN, 100.0),
            (100.0, f64::INFINITY),
        ] {
            assert_eq!(fit_preview_size((580.0, 332.0), source), (580.0, 332.0));
        }
        assert_eq!(fit_preview_size((0.0, 332.0), (800.0, 600.0)), (0.0, 0.0));
        assert_eq!(
            fit_preview_size((f64::NAN, -1.0), (800.0, 600.0)),
            (0.0, 0.0)
        );
    }

    fn frame(pid: i64, window_id: u64, timestamp_ms: u64) -> PipFrame {
        PipFrame {
            target: PipTarget {
                logical_pid: None,
                delegation: None,
                pid,
                window_id,
                session_id: None,
                app_name: format!("app-{pid}"),
                window_title: None,
            },
            png_bytes: vec![pid as u8],
            timestamp_ms,
        }
    }

    fn session_frame(session: &str, pid: i64, window_id: u64, timestamp_ms: u64) -> PipFrame {
        let mut frame = frame(pid, window_id, timestamp_ms);
        frame.target.session_id = Some(session.to_owned());
        frame
    }

    fn delegated_frame(
        logical_pid: i64,
        helper_pid: i64,
        window_id: u64,
        timestamp_ms: u64,
    ) -> PipFrame {
        let mut frame = frame(helper_pid, window_id, timestamp_ms);
        frame.target.logical_pid = Some(logical_pid);
        frame.target.app_name = format!("host-{logical_pid}");
        frame
    }

    #[test]
    fn one_card_per_app_tracks_the_latest_root_window() {
        let mut model = PipViewModel::new(5);
        assert_eq!(model.upsert(frame(42, 7, 10)).window_changed, false);
        assert_eq!(model.upsert(frame(42, 8, 20)).window_changed, true);
        assert_eq!(model.len(), 1);
        assert_eq!(model.frame_for_app(42).unwrap().target.window_id, 8);
    }

    #[test]
    fn shared_helper_windows_are_keyed_by_their_logical_host_apps() {
        let mut model = PipViewModel::new(5);
        model.upsert(delegated_frame(42, 900, 70, 10));
        model.upsert(delegated_frame(43, 900, 71, 20));
        assert_eq!(model.len(), 2);
        assert_eq!(model.frame_for_app(42).unwrap().target.window_id, 70);
        assert_eq!(model.frame_for_app(43).unwrap().target.window_id, 71);
    }

    #[test]
    fn evicts_the_least_recently_updated_app_at_the_visible_limit() {
        let mut model = PipViewModel::new(2);
        model.upsert(frame(1, 11, 10));
        model.upsert(frame(2, 22, 20));
        let outcome = model.upsert(frame(3, 33, 30));
        assert_eq!(outcome.evicted_pid, Some(1));
        assert!(model.frame_for_app(1).is_none());
        assert_eq!(model.len(), 2);
    }

    #[test]
    fn current_app_is_not_immediately_evicted_when_refreshing_at_capacity() {
        let mut model = PipViewModel::new(2);
        model.upsert(frame(1, 11, 10));
        model.upsert(frame(2, 22, 20));
        let outcome = model.upsert(frame(1, 11, 30));
        assert_eq!(outcome.evicted_pid, None);
        let ordered = model.ordered_frames();
        assert_eq!(ordered[0].target.pid, 1);
    }

    #[test]
    fn promoting_an_app_moves_it_to_the_front_without_dropping_frames() {
        let mut model = PipViewModel::new(3);
        model.upsert(frame(1, 11, 10));
        model.upsert(frame(2, 22, 20));
        model.upsert(frame(3, 33, 30));

        assert!(model.promote_app(1));
        let ordered = model.ordered_frames();
        assert_eq!(
            ordered
                .iter()
                .map(|frame| frame.target.pid)
                .collect::<Vec<_>>(),
            vec![2, 3, 1]
        );
        assert_eq!(model.len(), 3);
        assert!(!model.promote_app(1));
        assert!(!model.promote_app(99));
    }

    #[test]
    fn refreshing_an_app_does_not_move_its_card() {
        let mut model = PipViewModel::new(5);
        model.upsert(frame(1, 11, 10));
        model.upsert(frame(2, 22, 20));
        model.upsert(frame(1, 11, 30));
        let order = model
            .ordered_frames()
            .into_iter()
            .map(|frame| frame.target.pid)
            .collect::<Vec<_>>();
        assert_eq!(order, vec![1, 2]);
    }

    #[test]
    fn candidate_refresh_removes_closed_or_replaced_exact_windows() {
        let mut model = PipViewModel::new(5);
        model.upsert(frame(1, 11, 10));
        model.upsert(frame(2, 22, 20));
        model.upsert(frame(3, 33, 30));

        let live = [(1, 11), (2, 99)];
        let removed =
            model.retain_live_targets(|target| live.contains(&(target.pid, target.window_id)));

        assert_eq!(removed.removed_pids, vec![2, 3]);
        assert!(removed.changed_pids.is_empty());
        assert_eq!(
            model
                .ordered_frames()
                .iter()
                .map(|frame| frame.target.pid)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn ending_a_session_removes_only_its_latest_cards() {
        let mut model = PipViewModel::new(5);
        model.upsert(session_frame("session-a", 1, 11, 10));
        model.upsert(session_frame("session-b", 2, 22, 20));

        let change = model.remove_session("session-a");
        assert_eq!(change.removed_pids, vec![1]);
        assert!(change.changed_pids.is_empty());
        assert!(model.frame_for_app(1).is_none());
        assert!(model.frame_for_app(2).is_some());
        assert!(model.remove_session("missing").is_empty());
    }

    #[test]
    fn same_app_window_refresh_remains_one_card_and_tracks_new_session() {
        let mut model = PipViewModel::new(5);
        model.upsert(session_frame("session-a", 7, 70, 10));
        assert!(
            model
                .upsert(session_frame("session-b", 7, 71, 20))
                .window_changed
        );

        assert_eq!(model.len(), 1);
        let target = &model.frame_for_app(7).unwrap().target;
        assert_eq!(target.window_id, 71);
        assert_eq!(target.session_id.as_deref(), Some("session-b"));
        assert!(model.remove_session("session-a").is_empty());
    }

    #[test]
    fn switching_a_session_retires_its_old_app_before_the_new_frame_arrives() {
        let mut model = PipViewModel::new(5);
        let finder = session_frame("task", 1, 11, 10);
        let text_edit = session_frame("task", 2, 22, 20);
        assert!(model.upsert(finder.clone()).accepted);

        let change = model.select_target(&text_edit.target);
        assert_eq!(change.removed_pids, vec![1]);
        assert!(change.changed_pids.is_empty());
        assert!(model.is_empty());
        assert!(!model.accepts_target(&finder.target));
        assert!(model.accepts_target(&text_edit.target));

        let late = model.upsert(finder);
        assert!(!late.accepted);
        assert!(late.change.is_empty());
        assert!(model.is_empty());
        assert!(model.upsert(text_edit).accepted);
        assert_eq!(model.len(), 1);
        assert_eq!(model.ordered_frames()[0].target.pid, 2);
    }

    #[test]
    fn reselection_and_tool_gaps_keep_the_same_app_frame_and_order() {
        let mut model = PipViewModel::new(5);
        let original = session_frame("task", 1, 11, 10);
        model.upsert(original.clone());
        model.upsert(session_frame("other-task", 2, 22, 20));
        let initial_frame = model.frame_for_app(1).unwrap() as *const PipFrame;

        for _ in 0..3 {
            assert!(model.select_target(&original.target).is_empty());
            assert!(model.accepts_target(&original.target));
            assert_eq!(
                model.frame_for_app(1).unwrap() as *const PipFrame,
                initial_frame
            );
        }
        let new_window = session_frame("task", 1, 12, 30);
        assert!(model.select_target(&new_window.target).is_empty());
        assert_eq!(model.frame_for_app(1).unwrap().target.window_id, 11);
        assert!(model.upsert(new_window).window_changed);
        assert_eq!(model.frame_for_app(1).unwrap().target.window_id, 12);
        assert_eq!(
            model
                .ordered_frames()
                .iter()
                .map(|frame| frame.target.pid)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn switching_an_app_preserves_another_session_regardless_of_last_publisher() {
        for last_publisher in ["task-a", "task-b"] {
            let mut model = PipViewModel::new(5);
            model.upsert(session_frame("task-a", 1, 11, 10));
            model.upsert(session_frame("task-b", 1, 11, 20));
            model.upsert(session_frame(last_publisher, 1, 11, 30));

            let text_edit = session_frame("task-a", 2, 22, 40);
            let change = model.select_target(&text_edit.target);
            assert!(change.removed_pids.is_empty());
            assert_eq!(
                change.changed_pids,
                if last_publisher == "task-a" {
                    vec![1]
                } else {
                    vec![]
                }
            );
            assert_eq!(
                model.frame_for_app(1).unwrap().target.session_id.as_deref(),
                Some("task-b")
            );
            assert!(model.upsert(text_edit).accepted);
            assert_eq!(model.len(), 2);
            assert!(model.remove_session("task-a").removed_pids.contains(&2));
            assert!(model.frame_for_app(1).is_some());
        }
    }

    #[test]
    fn ending_or_switching_the_last_publisher_restores_the_other_sessions_exact_window() {
        for end_session in [false, true] {
            let mut model = PipViewModel::new(5);
            let first = session_frame("task-a", 1, 11, 10);
            model.upsert(first.clone());
            model.upsert(session_frame("task-b", 1, 12, 20));
            let change = if end_session {
                model.remove_session("task-b")
            } else {
                model.select_target(&session_frame("task-b", 2, 22, 30).target)
            };
            assert!(change.removed_pids.is_empty());
            assert_eq!(change.changed_pids, vec![1]);
            let restored = model.frame_for_app(1).unwrap();
            assert_eq!(restored.target, first.target);
            assert_eq!(restored.timestamp_ms, first.timestamp_ms);
            assert_eq!(model.remove_session("task-a").removed_pids, vec![1]);
            assert!(model.is_empty());
        }
    }

    #[test]
    fn liveness_checks_cover_unrepresented_session_frames_before_any_fallback() {
        for dead_window in [11, 12] {
            let mut model = PipViewModel::new(5);
            model.upsert(session_frame("task-a", 1, 11, 10));
            model.upsert(session_frame("task-b", 1, 12, 20));
            assert_eq!(model.retained_targets().len(), 2);
            let mut checked = Vec::new();
            let change = model.retain_live_targets(|target| {
                checked.push(target.window_id);
                target.window_id != dead_window
            });
            checked.sort_unstable();
            assert_eq!(checked, vec![11, 12]);
            assert!(change.removed_pids.is_empty());
            assert_eq!(
                change.changed_pids,
                if dead_window == 12 { vec![1] } else { vec![] }
            );
            let survivor = if dead_window == 12 {
                "task-a"
            } else {
                "task-b"
            };
            assert_eq!(model.remove_session(survivor).removed_pids, vec![1]);
            assert!(model.is_empty(), "a dead fallback must not be restored");
        }
    }

    #[test]
    fn failed_preview_cleanup_preserves_other_sessions_and_newer_windows() {
        let mut model = PipViewModel::new(5);
        let first = session_frame("task-a", 1, 11, 10);
        model.upsert(first.clone());
        model.upsert(session_frame("task-b", 1, 12, 20));
        assert!(model.remove_target_frame(&first.target).is_empty());
        assert_eq!(model.frame_for_app(1).unwrap().target.window_id, 12);

        let newer = session_frame("task-a", 1, 13, 30);
        model.upsert(newer.clone());
        assert!(model.remove_target_frame(&first.target).is_empty());
        assert_eq!(model.frame_for_app(1).unwrap().target.window_id, 13);
        let change = model.remove_target_frame(&newer.target);
        assert!(change.removed_pids.is_empty());
        assert_eq!(change.changed_pids, vec![1]);
        assert_eq!(model.frame_for_app(1).unwrap().target.window_id, 12);
    }

    #[test]
    fn current_app_accepts_menu_and_delegated_visual_sources() {
        let mut model = PipViewModel::new(5);
        let document = session_frame("task", 42, 7, 10);
        model.upsert(document.clone());
        let mut menu = session_frame("task", 42, 8, 20);
        menu.target.logical_pid = Some(42);
        assert!(model.select_target(&menu.target).is_empty());
        assert!(model.upsert(menu).accepted);
        let mut panel = session_frame("task", 900, 9, 30);
        panel.target.logical_pid = Some(42);
        assert!(model.select_target(&panel.target).is_empty());
        assert!(model.upsert(panel).accepted);
        assert_eq!(model.len(), 1);
        assert_eq!(model.frame_for_app(42).unwrap().target.pid, 900);

        let other_host = session_frame("task", 43, 10, 40);
        assert_eq!(
            model.select_target(&other_host.target).removed_pids,
            vec![42]
        );
        assert!(!model.accepts_target(&document.target));
    }

    #[test]
    fn capacity_eviction_discards_all_old_frames_without_resurrecting_them() {
        let mut model = PipViewModel::new(1);
        model.upsert(session_frame("task-a", 1, 11, 10));
        model.upsert(session_frame("task-b", 1, 12, 20));
        let outcome = model.upsert(session_frame("task-c", 2, 22, 30));
        assert_eq!(outcome.evicted_pid, Some(1));
        assert_eq!(outcome.change.removed_pids, vec![1]);
        assert_eq!(model.retained_targets().len(), 1);
        assert!(model.remove_session("task-b").is_empty());
        assert_eq!(model.remove_session("task-c").removed_pids, vec![2]);
        assert!(model.is_empty());
        assert!(model.upsert(session_frame("task-a", 1, 11, 40)).accepted);
    }

    #[test]
    fn anonymous_cards_are_not_retargeted_by_a_runtime_session() {
        let mut model = PipViewModel::new(5);
        model.upsert(frame(1, 11, 10));
        model.upsert(session_frame("task", 1, 12, 20));
        let change = model.select_target(&session_frame("task", 2, 22, 30).target);
        assert!(change.removed_pids.is_empty());
        assert_eq!(change.changed_pids, vec![1]);
        assert_eq!(model.frame_for_app(1).unwrap().target.session_id, None);
        assert_eq!(model.frame_for_app(1).unwrap().target.window_id, 11);
    }
}
