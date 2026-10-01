//! `cua-driver skills {install|update|uninstall|status|path}` —
//! agent skill-pack management.
//!
//! The install scripts intentionally do NOT touch the user's
//! `~/.claude/skills/` (etc.) directories — too invasive for an
//! `irm | iex` / `curl | sh` one-liner that the user might be running
//! just to try the binary. This verb is the opt-in path: write the skill
//! pack bundled into this binary to a local copy and symlink it into each
//! detected agent's skills dir.
//!
//! ## Subcommands
//!
//! - `install` — place + symlink (idempotent: re-run is a no-op).
//! - `update` — same as `install --force`: rewrite the local copy from
//!   the bundled pack even if it already exists.
//! - `uninstall [--all]` — remove the agent symlinks. With `--all`, also
//!   delete the local copy under `<HomeDir>/skills/cua-driver/` (and the
//!   pre-rename `cua-driver-rs/` location if present).
//! - `status` — print local install state + per-agent link state.
//! - `path` — print `<HomeDir>/skills/cua-driver` (the local copy).
//!
//! Default install drops only the host platform's deep-dive .md
//! (WINDOWS.md / MACOS.md / LINUX.md — whichever matches). Pass
//! `--all-platforms` to keep all three (useful when assisting users
//! across OSes from one machine).
//!
//! ## Source
//!
//! The skill pack is compiled into the binary from `Skills/cua-driver/`
//! (see [`BUNDLED_SKILL_FILES`]), so its content always matches the daemon
//! an agent will talk to. Nothing is fetched over the network: the former
//! GitHub release-asset download and the `--from main` per-file fetch were
//! removed, and `--from` is now rejected with an explanation.
//!
//! ## Agent detection
//!
//! Same agent dirs as the Swift cua-driver installer detects:
//!
//! - Claude Code: `~/.claude/skills/`
//! - Codex:       `~/.agents/skills/`
//! - Prime Agent: `~/.prime/agent/skills/`
//! - OpenClaw:    `~/.openclaw/skills/`
//! - OpenCode: `~/.config/opencode/skills/` (macOS / Linux),
//!   `%APPDATA%\opencode\skills\` (Windows)
//! - Antigravity: `~/.gemini/skills/` — shared between Antigravity CLI
//!   (`agy`) and Antigravity IDE; same dir Google Gemini CLI used before the
//!   May-2026 transition, so existing installs migrate forward unchanged.
//! - Hermes: `$HERMES_HOME/skills/` when set, otherwise `~/.hermes/skills/` on
//!   macOS/Linux or `%LOCALAPPDATA%\hermes\skills\` on Windows. This is the
//!   user-level skill space Hermes resolves at agent load time (separate from
//!   the repo-bundled `hermes-agent/skills/` tree, which is read-only and
//!   version-controlled). Hermes' own `computer-use` skill teaches its wrapper
//!   vocabulary; the cua-driver pack provides the platform deep dives.
//!
//! Only acts on a given agent when its parent skills dir already
//! exists (i.e. the agent itself is installed). Never clobbers an
//! existing `<agent_skills>/cua-driver` link — preserves dev users'
//! hand-rolled symlinks.

use anyhow::{anyhow, bail, Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

const SKILL_PACK_NAME: &str = "cua-driver";
/// Pre-rename name. The skill pack used to install as `cua-driver-rs`
/// (when the Rust port lived at `libs/cua-driver-rs/`). On install /
/// uninstall we sweep this name out of every agent skills dir and the
/// local stage so a user who had the old skill installed ends up with
/// exactly one pack, named consistently with the rest of the binary.
const LEGACY_SKILL_PACK_NAME: &str = "cua-driver-rs";
/// The skill pack bundled into this binary at compile time. `install` and
/// `update` write exactly these files; nothing is downloaded.
const BUNDLED_SKILL_FILES: &[(&str, &str)] = &[
    (
        "README.md",
        include_str!("../../../Skills/cua-driver/README.md"),
    ),
    (
        "SKILL.md",
        include_str!("../../../Skills/cua-driver/SKILL.md"),
    ),
    (
        "WINDOWS.md",
        include_str!("../../../Skills/cua-driver/WINDOWS.md"),
    ),
    (
        "MACOS.md",
        include_str!("../../../Skills/cua-driver/MACOS.md"),
    ),
    (
        "LINUX.md",
        include_str!("../../../Skills/cua-driver/LINUX.md"),
    ),
    (
        "BROWSER.md",
        include_str!("../../../Skills/cua-driver/BROWSER.md"),
    ),
    (
        "RECORDING.md",
        include_str!("../../../Skills/cua-driver/RECORDING.md"),
    ),
    (
        "EMBEDDING.md",
        include_str!("../../../Skills/cua-driver/EMBEDDING.md"),
    ),
];

/// Per-host filter: returns the platform-specific docs that should NOT
/// land in the local stage. The bundled skill pack carries docs for all
/// three platforms, but a Windows user has no need for
/// LINUX.md / MACOS.md and vice-versa. SKILL.md still references the
/// matching platform doc by name, so the LLM sees one specific deep
/// dive without two extra files of unused noise.
///
/// Override with `cua-driver skills install --all-platforms` to keep
/// the full set (useful when assisting users across OSes from one
/// machine).
fn excluded_platform_docs(all_platforms: bool) -> &'static [&'static str] {
    if all_platforms {
        return &[];
    }
    #[cfg(target_os = "windows")]
    {
        &["LINUX.md", "MACOS.md"]
    }
    #[cfg(target_os = "linux")]
    {
        &["WINDOWS.md", "MACOS.md"]
    }
    #[cfg(target_os = "macos")]
    {
        &["WINDOWS.md", "LINUX.md"]
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        &[]
    }
}

/// True when the basename matches one of the excluded platform docs.
fn is_excluded_platform_doc(basename: &str, all_platforms: bool) -> bool {
    let excluded = excluded_platform_docs(all_platforms);
    excluded.iter().any(|f| basename.eq_ignore_ascii_case(f))
}

/// Legacy subdir name. `sweep_legacy_skill_pack` removes any skill-pack
/// artifacts that landed here before the rename.
const LEGACY_HOME_SUBDIRECTORY: &str = ".cua-driver-rs";

/// Local install path for the skill pack: `<HomeDir>/skills/cua-driver`.
fn local_skill_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join("skills").join(SKILL_PACK_NAME))
}

/// `<HomeDir>` resolved from the same env override `serve.rs` uses,
/// falling back to platform conventions.
fn home_dir() -> Result<PathBuf> {
    if let Ok(h) = std::env::var("CUA_DRIVER_RS_HOME") {
        return Ok(PathBuf::from(h));
    }
    #[cfg(windows)]
    {
        let userprofile =
            std::env::var("USERPROFILE").map_err(|_| anyhow!("USERPROFILE not set"))?;
        return Ok(PathBuf::from(userprofile).join(crate::bundle::user_home_subdirectory()));
    }
    #[cfg(not(windows))]
    {
        let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
        Ok(PathBuf::from(home).join(crate::bundle::user_home_subdirectory()))
    }
}

/// The pre-rename home (`~/.cua-driver-rs/`) if it exists on disk.
/// `None` when CUA_DRIVER_RS_HOME is set (the env var overrides both
/// the new and legacy defaults — caller is on their own).
fn legacy_home_dir() -> Option<PathBuf> {
    if std::env::var("CUA_DRIVER_RS_HOME").is_ok() || crate::bundle::is_local_installation() {
        return None;
    }
    #[cfg(windows)]
    let base = std::env::var("USERPROFILE").ok()?;
    #[cfg(not(windows))]
    let base = std::env::var("HOME").ok()?;
    let p = PathBuf::from(base).join(LEGACY_HOME_SUBDIRECTORY);
    if p.exists() {
        Some(p)
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy)]
struct Agent {
    label: &'static str,
    /// User-relative parent skills dir, expanded at runtime per OS.
    parent: AgentParent,
}

#[derive(Debug, Clone, Copy)]
enum AgentParent {
    /// `<HOME or USERPROFILE>/<segment>`.
    Home(&'static str),
    /// Hermes' effective user skill directory. Unlike the other agents,
    /// Hermes supports an explicit `HERMES_HOME` and uses a native-Windows
    /// default outside `USERPROFILE`.
    Hermes,
    /// `<APPDATA>/<segment>` (Windows roaming app config).
    ///
    /// `#[allow(dead_code)]`: constructed only inside `#[cfg(windows)]`
    /// AGENTS entries (OpenCode on Windows reads from `%APPDATA%`) and
    /// matched only inside `#[cfg(windows)]` arms of `parent_path`. The
    /// enum variant itself sits in cross-platform code so rustc's
    /// post-cfg-strip dead-code pass flags it on macOS/Linux even though
    /// the Windows build uses it.
    #[allow(dead_code)]
    AppData(&'static str),
}

const AGENTS: &[Agent] = &[
    Agent {
        label: "Claude Code",
        parent: AgentParent::Home(".claude/skills"),
    },
    Agent {
        label: "Codex",
        parent: AgentParent::Home(".agents/skills"),
    },
    Agent {
        label: "Prime Agent",
        parent: AgentParent::Home(".prime/agent/skills"),
    },
    Agent {
        label: "OpenClaw",
        parent: AgentParent::Home(".openclaw/skills"),
    },
    #[cfg(windows)]
    Agent {
        label: "OpenCode",
        parent: AgentParent::AppData("opencode/skills"),
    },
    #[cfg(not(windows))]
    Agent {
        label: "OpenCode",
        parent: AgentParent::Home(".config/opencode/skills"),
    },
    // Antigravity CLI + Antigravity IDE share the `.gemini/skills/` dir
    // (the same path Gemini CLI used pre-May-2026). Registering the
    // single shared path means both surfaces pick up the same symlink.
    Agent {
        label: "Antigravity",
        parent: AgentParent::Home(".gemini/skills"),
    },
    // Hermes (NousResearch/hermes-agent) resolves user skills from its
    // effective home at agent load time. `HERMES_HOME` can select a custom
    // home or profile; otherwise Hermes uses `~/.hermes` on macOS/Linux and
    // `%LOCALAPPDATA%\hermes` on native Windows. Hermes' bundled
    // `skills/computer-use/SKILL.md` teaches the wrapper vocabulary; this
    // pack adds the platform-specific deep dives.
    Agent {
        label: "Hermes",
        parent: AgentParent::Hermes,
    },
];

fn hermes_skills_dir_from_env() -> Result<PathBuf> {
    resolve_hermes_skills_dir(
        std::env::var("HERMES_HOME").ok().as_deref(),
        default_hermes_home,
    )
}

fn resolve_hermes_skills_dir(
    override_home: Option<&str>,
    default_home: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    let home = match override_home.map(str::trim).filter(|path| !path.is_empty()) {
        Some(path) => PathBuf::from(path),
        None => default_home()?,
    };
    Ok(home.join("skills"))
}

#[cfg(not(windows))]
fn default_hermes_home() -> Result<PathBuf> {
    unix_hermes_home(std::env::var("HOME").ok().as_deref())
}

#[cfg(any(not(windows), test))]
fn unix_hermes_home(home: Option<&str>) -> Result<PathBuf> {
    let home = home
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow!("HOME not set"))?;
    Ok(PathBuf::from(home).join(".hermes"))
}

#[cfg(windows)]
fn default_hermes_home() -> Result<PathBuf> {
    windows_hermes_home(
        std::env::var("LOCALAPPDATA").ok().as_deref(),
        std::env::var("USERPROFILE").ok().as_deref(),
    )
}

#[cfg(any(windows, test))]
fn windows_hermes_home(local_appdata: Option<&str>, userprofile: Option<&str>) -> Result<PathBuf> {
    if let Some(path) = local_appdata.map(str::trim).filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path).join("hermes"));
    }
    let profile = userprofile
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| anyhow!("LOCALAPPDATA and USERPROFILE not set"))?;
    Ok(PathBuf::from(profile)
        .join("AppData")
        .join("Local")
        .join("hermes"))
}

impl Agent {
    fn parent_path(&self) -> Result<PathBuf> {
        match self.parent {
            AgentParent::Home(seg) => {
                #[cfg(windows)]
                let base =
                    std::env::var("USERPROFILE").map_err(|_| anyhow!("USERPROFILE not set"))?;
                #[cfg(not(windows))]
                let base = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
                Ok(PathBuf::from(base).join(seg.replace('/', std::path::MAIN_SEPARATOR_STR)))
            }
            AgentParent::Hermes => hermes_skills_dir_from_env(),
            AgentParent::AppData(seg) => {
                #[cfg(windows)]
                let base = std::env::var("APPDATA").map_err(|_| anyhow!("APPDATA not set"))?;
                #[cfg(not(windows))]
                let base = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
                Ok(PathBuf::from(base).join(seg.replace('/', std::path::MAIN_SEPARATOR_STR)))
            }
        }
    }
    fn link_path(&self) -> Result<PathBuf> {
        Ok(self.parent_path()?.join(SKILL_PACK_NAME))
    }
}

// ── Public dispatcher ─────────────────────────────────────────────────────

pub fn run(subcommand: &str, flags: &[String]) {
    let result = match subcommand {
        "install" => install(flags, false),
        "update" => install(flags, true),
        "uninstall" => uninstall(flags),
        "status" => status(),
        "path" => print_path(),
        other => {
            eprintln!("Unknown skills subcommand: {other:?}");
            eprintln!("Usage: cua-driver skills {{install|update|uninstall|status|path}}");
            std::process::exit(64);
        }
    };
    match result {
        Ok(()) => {}
        Err(e) => {
            // `anyhow::Error`'s default Display only prints the outermost
            // context. Use alternate Display so failures include the
            // destination path and the underlying filesystem error.
            eprintln!("cua-driver skills {subcommand}: {e:#}");
            std::process::exit(1);
        }
    }
}

// ── install / update ──────────────────────────────────────────────────────

fn install(flags: &[String], force: bool) -> Result<()> {
    if flags
        .iter()
        .any(|flag| flag == "--from" || flag.starts_with("--from="))
    {
        bail!(
            "`--from` is not supported: remote skill sources were removed from this build. \
             `cua-driver skills install` installs the skill pack bundled into this binary; \
             nothing is downloaded."
        );
    }
    let force = force || flags.iter().any(|f| f == "--force");
    // `--all-platforms` opts INTO keeping LINUX.md / MACOS.md / WINDOWS.md
    // for every host. Default is host-only — only the matching platform's
    // doc is kept, the other two are skipped when the pack is written.
    let all_platforms = flags.iter().any(|f| f == "--all-platforms");

    // Sweep the legacy `cua-driver-rs`-named pack out FIRST so the
    // post-install state has exactly one skill pack at the new name.
    // Done before writing so a fresh install on a previously-installed
    // machine doesn't leave orphan links pointing at a stale local dir.
    sweep_legacy_skill_pack();

    let local = local_skill_dir()?;
    let already_present = local.join("SKILL.md").exists();

    if !already_present || force {
        write_bundled_into(&local, all_platforms)
            .with_context(|| format!("failed to install skill pack to {}", local.display()))?;
        println!("✅ Skill pack at {}", local.display());
    } else {
        println!(
            "✅ Skill pack already at {} (use `cua-driver skills update` to refresh)",
            local.display()
        );
    }

    let mut linked_any = false;
    for agent in AGENTS {
        match link_agent(*agent, &local) {
            Ok(true) => linked_any = true,
            Ok(false) => {}
            Err(e) => eprintln!("  warning: failed to link {}: {e}", agent.label),
        }
    }
    if !linked_any {
        println!("(No agent skills dirs present yet — install Claude Code / Codex / Prime Agent / OpenClaw / OpenCode / Antigravity / Hermes then re-run.)");
    }
    Ok(())
}

/// Best-effort removal of any pre-rename skill pack. Three legacy
/// locations are swept:
/// 1. `<HomeDir>/skills/cua-driver-rs/`       — old pack NAME under new home dir
/// 2. `<LegacyHomeDir>/skills/cua-driver/`    — new pack NAME under old home dir
/// 3. `<LegacyHomeDir>/skills/cua-driver-rs/` — old pack NAME under old home dir
///
/// Plus every `<agent_skills>/cua-driver-rs` symlink/junction.
///
/// Then attempts to remove the empty `<LegacyHomeDir>/skills/` and
/// `<LegacyHomeDir>/` themselves so the dot-folder doesn't linger.
/// Only when those dirs are actually empty — never blows away a
/// legacy install that still has packages/ alongside.
///
/// Runs at the start of `install` / `update` so a user who had any
/// flavour of the legacy layout installed gets cleanly migrated
/// without having to run `skills uninstall` first.
///
/// Silent on failure — this is a UX nicety, not a correctness boundary.
/// The new pack still installs even if a stale junction can't be cleaned.
fn sweep_legacy_skill_pack() {
    // (1) Old pack NAME under new home dir.
    if let Ok(home) = home_dir() {
        let legacy_local = home.join("skills").join(LEGACY_SKILL_PACK_NAME);
        if legacy_local.exists() {
            if let Err(e) = fs::remove_dir_all(&legacy_local) {
                eprintln!(
                    "  warning: could not remove legacy local pack at {}: {e}",
                    legacy_local.display()
                );
            } else {
                println!(
                    "  cleaned up legacy local pack at {}",
                    legacy_local.display()
                );
            }
        }
    }
    // (2) + (3) Any pack name under the pre-rename home dir, then try
    // to remove the empty skills/ + home/ dirs themselves.
    if let Some(legacy_home) = legacy_home_dir() {
        let legacy_skills_dir = legacy_home.join("skills");
        for name in [SKILL_PACK_NAME, LEGACY_SKILL_PACK_NAME] {
            let dir = legacy_skills_dir.join(name);
            if dir.exists() {
                if let Err(e) = fs::remove_dir_all(&dir) {
                    eprintln!(
                        "  warning: could not remove legacy pack at {}: {e}",
                        dir.display()
                    );
                } else {
                    println!("  cleaned up legacy local pack at {}", dir.display());
                }
            }
        }
        // remove_dir refuses to delete non-empty dirs — safe to ignore
        // errors here, and intentional: a legacy install that still has
        // packages/ alongside the (now-emptied) skills/ keeps its
        // dot-folder.
        let _ = fs::remove_dir(&legacy_skills_dir);
        let _ = fs::remove_dir(&legacy_home);
    }
    // Agent links named `<parent>/cua-driver-rs`.
    for agent in AGENTS {
        let parent = match agent.parent_path() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let legacy_link = parent.join(LEGACY_SKILL_PACK_NAME);
        if !parent.exists() || legacy_link.symlink_metadata().is_err() {
            continue;
        }
        if !is_symlink_or_junction(&legacy_link) {
            // Real directory — don't clobber user-managed content.
            continue;
        }
        if let Err(e) = remove_link(&legacy_link) {
            eprintln!(
                "  warning: could not remove legacy {} link at {}: {e}",
                agent.label,
                legacy_link.display()
            );
        } else {
            println!(
                "  cleaned up legacy {} link at {}",
                agent.label,
                legacy_link.display()
            );
        }
    }
}

/// Returns `Ok(true)` when a new link was created, `Ok(false)` when
/// skipped (parent dir missing, link already there, etc.).
fn link_agent(agent: Agent, local_skill_dir: &Path) -> Result<bool> {
    let parent = agent.parent_path()?;
    if !parent.exists() {
        return Ok(false);
    }
    let link = agent.link_path()?;
    // Four states for `link`:
    //   1. doesn't exist at all                 → create
    //   2. exists + resolves                    → already linked (skip)
    //   3. exists as link/junction but target dangling → remove + recreate
    //   4. exists as a real directory           → user-managed, leave alone
    //
    // `Path::exists()` follows symlinks, so it returns false for a
    // dangling link even though `symlink_metadata` succeeds — that's
    // the signature of case 3. We then check `is_symlink_or_junction`
    // before deleting, so we never touch a real user directory.
    let has_metadata = link.symlink_metadata().is_ok();
    let resolves = link.exists();
    if has_metadata && resolves {
        println!(
            "  {} skill link already exists at {} (skipping)",
            agent.label,
            link.display()
        );
        return Ok(false);
    }
    if has_metadata && !resolves && is_symlink_or_junction(&link) {
        // Dangling link/junction — target was removed (typical after
        // sweep_legacy_skill_pack cleaned a pre-rename pack out from
        // under it). Remove + recreate pointing at the new target.
        if let Err(e) = remove_link(&link) {
            eprintln!(
                "  warning: could not remove stale {} link at {}: {e}",
                agent.label,
                link.display()
            );
            return Ok(false);
        }
        println!(
            "  cleaned up stale {} link at {}",
            agent.label,
            link.display()
        );
    }
    make_dir_symlink(local_skill_dir, &link).with_context(|| {
        format!(
            "symlink {} -> {}",
            link.display(),
            local_skill_dir.display()
        )
    })?;
    println!("  ✅ linked {} skill at {}", agent.label, link.display());
    Ok(true)
}

#[cfg(windows)]
fn make_dir_symlink(target: &Path, link: &Path) -> Result<()> {
    // NTFS directory junction: works without admin/Developer Mode,
    // unlike `std::os::windows::fs::symlink_dir`. Shell out to cmd's
    // `mklink /J` — the canonical way to create a junction from a
    // standard user token.
    let status = std::process::Command::new("cmd")
        .args(["/c", "mklink", "/J"])
        .arg(link)
        .arg(target)
        .stdout(std::process::Stdio::null())
        .status()?;
    if !status.success() {
        bail!("mklink /J exited with {:?}", status.code());
    }
    Ok(())
}

#[cfg(not(windows))]
fn make_dir_symlink(target: &Path, link: &Path) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

// ── bundled pack ───────────────────────────────────────────────────────────

/// The bundled files, after checking that the pack really is in this build.
fn bundled_skill_files() -> Result<&'static [(&'static str, &'static str)]> {
    let skill = BUNDLED_SKILL_FILES
        .iter()
        .find(|(name, _)| *name == "SKILL.md")
        .map(|(_, body)| *body)
        .unwrap_or_default();
    if skill.trim().is_empty() {
        bail!(
            "this build of cua-driver does not bundle the agent skill pack (SKILL.md is \
             missing or empty), and skills are never downloaded. Install a build that \
             includes Skills/cua-driver/."
        );
    }
    Ok(BUNDLED_SKILL_FILES)
}

/// Replace `dest` with the bundled skill pack, keeping only this host's
/// platform guide unless `all_platforms` is set.
fn write_bundled_into(dest: &Path, all_platforms: bool) -> Result<()> {
    let files = bundled_skill_files()?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    // Wipe stale content so an update is a clean replace, not a merge.
    if dest.exists() {
        fs::remove_dir_all(dest)?;
    }
    fs::create_dir_all(dest)?;
    for (name, body) in files {
        if is_excluded_platform_doc(name, all_platforms) {
            continue;
        }
        fs::write(dest.join(name), body)?;
    }
    Ok(())
}

// ── uninstall ──────────────────────────────────────────────────────────────

fn uninstall(flags: &[String]) -> Result<()> {
    let remove_local = flags.iter().any(|f| f == "--all");
    let mut removed_any = false;
    // Try BOTH the current name and the legacy `cua-driver-rs` name so a
    // user who installed under the old name and then `skills uninstall`s
    // ends up clean. Same symlink/junction safety check applies to each.
    for name in [SKILL_PACK_NAME, LEGACY_SKILL_PACK_NAME] {
        for agent in AGENTS {
            let parent = match agent.parent_path() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let link = parent.join(name);
            if link.symlink_metadata().is_ok() {
                // Only remove if it's a symlink/junction we manage. If a
                // user replaced it with a real dir, leave it alone.
                if is_symlink_or_junction(&link) {
                    remove_link(&link)?;
                    println!("  ✅ removed {} link at {}", agent.label, link.display());
                    removed_any = true;
                } else {
                    println!(
                        "  {} link at {} is not a symlink/junction; leaving alone",
                        agent.label,
                        link.display()
                    );
                }
            }
        }
    }
    if remove_local {
        // Local stage at the current name + any legacy stage from before
        // the rename. Both are owned by the installer; safe to delete.
        if let Ok(home) = home_dir() {
            for name in [SKILL_PACK_NAME, LEGACY_SKILL_PACK_NAME] {
                let local = home.join("skills").join(name);
                if local.exists() {
                    fs::remove_dir_all(&local)?;
                    println!("  ✅ removed local skill pack at {}", local.display());
                }
            }
        }
        // Also clean up any pre-rename home (`~/.cua-driver-rs/`) that
        // might still hold a skill pack from before the
        // `.cua-driver-rs/` → `.cua-driver/` migration. Remove the empty
        // skills/ and home/ dirs only if nothing else lives under them.
        if let Some(legacy_home) = legacy_home_dir() {
            let legacy_skills_dir = legacy_home.join("skills");
            for name in [SKILL_PACK_NAME, LEGACY_SKILL_PACK_NAME] {
                let local = legacy_skills_dir.join(name);
                if local.exists() {
                    fs::remove_dir_all(&local)?;
                    println!(
                        "  ✅ removed legacy local skill pack at {}",
                        local.display()
                    );
                }
            }
            let _ = fs::remove_dir(&legacy_skills_dir);
            let _ = fs::remove_dir(&legacy_home);
        }
    }
    if !removed_any {
        println!("(no agent skill links found)");
    }
    Ok(())
}

#[cfg(windows)]
fn is_symlink_or_junction(p: &Path) -> bool {
    if let Ok(md) = p.symlink_metadata() {
        // Both symlinks and junctions have the reparse-point attribute.
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        return md.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
    }
    false
}

#[cfg(not(windows))]
fn is_symlink_or_junction(p: &Path) -> bool {
    p.symlink_metadata()
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(windows)]
fn remove_link(p: &Path) -> Result<()> {
    // For a junction (which appears as a directory) we use rmdir;
    // for a file symlink std::fs::remove_file would work, but
    // junctions are dir-shaped so std::fs::remove_dir is correct.
    fs::remove_dir(p).map_err(Into::into)
}

#[cfg(not(windows))]
fn remove_link(p: &Path) -> Result<()> {
    fs::remove_file(p).map_err(Into::into)
}

// ── status ─────────────────────────────────────────────────────────────────

fn status() -> Result<()> {
    let local = local_skill_dir()?;
    if local.exists() && local.join("SKILL.md").exists() {
        println!("Local skill pack: {} ✅", local.display());
    } else {
        println!("Local skill pack: not installed (`cua-driver skills install` to install the bundled pack)");
    }
    println!();
    println!("Agent links:");
    for agent in AGENTS {
        let parent = match agent.parent_path() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let link = parent.join(SKILL_PACK_NAME);
        let parent_exists = parent.exists();
        if !parent_exists {
            println!(
                "  {} — agent dir not present ({})",
                agent.label,
                parent.display()
            );
            continue;
        }
        if !link.exists() && link.symlink_metadata().is_err() {
            println!("  {} — not linked ({})", agent.label, link.display());
        } else if is_symlink_or_junction(&link) {
            let target = fs::read_link(&link).ok();
            match target {
                Some(t) => println!(
                    "  {} — ✅ linked: {} → {}",
                    agent.label,
                    link.display(),
                    t.display()
                ),
                None => println!("  {} — ✅ linked: {}", agent.label, link.display()),
            }
        } else {
            println!(
                "  {} — non-symlink path at {} (left alone)",
                agent.label,
                link.display()
            );
        }
    }
    Ok(())
}

fn print_path() -> Result<()> {
    let local = local_skill_dir()?;
    println!("{}", local.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        install, resolve_hermes_skills_dir, unix_hermes_home, windows_hermes_home,
        write_bundled_into, AgentParent, AGENTS, BUNDLED_SKILL_FILES,
    };
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn prime_agent_target_matches_its_native_global_skill_directory() {
        let target = AGENTS
            .iter()
            .find(|agent| agent.label == "Prime Agent")
            .expect("Prime Agent must remain a supported skill target");

        assert!(matches!(
            target.parent,
            AgentParent::Home(".prime/agent/skills")
        ));
    }

    #[test]
    fn hermes_target_prefers_hermes_home() {
        let path = resolve_hermes_skills_dir(Some("/profiles/work"), || {
            panic!("the fallback must not be read")
        })
        .unwrap();
        assert_eq!(path, PathBuf::from("/profiles/work/skills"));
    }

    #[test]
    fn hermes_target_ignores_empty_or_whitespace_override() {
        for override_home in [None, Some(""), Some("  \t  ")] {
            let path =
                resolve_hermes_skills_dir(override_home, || Ok(PathBuf::from("/fallback/hermes")))
                    .unwrap();
            assert_eq!(path, PathBuf::from("/fallback/hermes/skills"));
        }
    }

    #[test]
    fn hermes_target_uses_unix_default_home() {
        assert_eq!(
            unix_hermes_home(Some("/home/test")).unwrap(),
            PathBuf::from("/home/test/.hermes")
        );
    }

    #[test]
    fn hermes_target_uses_native_windows_local_appdata() {
        let home = windows_hermes_home(Some("C:/Users/test/AppData/Local"), None).unwrap();
        assert_eq!(home, PathBuf::from("C:/Users/test/AppData/Local/hermes"));
    }

    #[test]
    fn hermes_target_uses_windows_userprofile_fallback() {
        let expected = PathBuf::from("C:/Users/test")
            .join("AppData")
            .join("Local")
            .join("hermes");
        for local_appdata in [None, Some(""), Some("  \t  ")] {
            assert_eq!(
                windows_hermes_home(local_appdata, Some("C:/Users/test")).unwrap(),
                expected
            );
        }
    }

    #[test]
    fn hermes_defaults_report_missing_required_environment() {
        for home in [None, Some(""), Some("  \t  ")] {
            assert_eq!(
                unix_hermes_home(home).unwrap_err().to_string(),
                "HOME not set"
            );
        }
        for (local_appdata, userprofile) in [
            (None, None),
            (Some(""), None),
            (None, Some("  \t  ")),
            (Some("  "), Some("")),
        ] {
            assert_eq!(
                windows_hermes_home(local_appdata, userprofile)
                    .unwrap_err()
                    .to_string(),
                "LOCALAPPDATA and USERPROFILE not set"
            );
        }
    }

    #[test]
    fn bundled_pack_matches_canonical_markdown_files() {
        let skill_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../Skills/cua-driver");
        let mut canonical = std::fs::read_dir(&skill_dir)
            .unwrap_or_else(|error| panic!("failed to read {}: {error}", skill_dir.display()))
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("md"))
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        canonical.sort();

        let mut bundled = BUNDLED_SKILL_FILES
            .iter()
            .map(|(file, _)| (*file).to_owned())
            .collect::<Vec<_>>();
        bundled.sort();

        assert_eq!(
            bundled, canonical,
            "BUNDLED_SKILL_FILES must include every canonical Markdown file"
        );
        for (file, body) in BUNDLED_SKILL_FILES {
            let on_disk = std::fs::read_to_string(skill_dir.join(file)).unwrap();
            assert_eq!(*body, on_disk, "{file} must be bundled verbatim");
        }
    }

    #[test]
    fn bundled_pack_never_sends_agents_to_upstream_installers_or_downloads() {
        // The pack is compiled into the binary and copied into agents' skill
        // directories. The upstream installers fetch upstream builds, which
        // keep telemetry and update checks, so the pack must not suggest them.
        // Needles are assembled at runtime so this source cannot match itself.
        let needles = [
            ["cua.ai/", "driver/install"].concat(),
            ["api.", "github.com"].concat(),
            ["raw.", "githubusercontent.com"].concat(),
            ["releases/", "download"].concat(),
        ];
        for (file, body) in BUNDLED_SKILL_FILES {
            for needle in &needles {
                assert!(
                    !body.contains(needle.as_str()),
                    "{file} contains {needle:?}"
                );
            }
        }
    }

    #[test]
    fn install_rejects_remote_sources_before_touching_anything() {
        for flags in [
            vec!["--from".to_owned(), "main".to_owned()],
            vec!["main".to_owned(), "--from".to_owned()],
            vec!["--from=main".to_owned()],
        ] {
            let error = install(&flags, false).unwrap_err().to_string();
            assert!(
                error.contains("remote skill sources were removed"),
                "{error}"
            );
        }
    }

    #[test]
    fn macos_skill_keeps_ax_only_and_non_prompting_permission_guidance() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let macos = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/MACOS.md"))
            .expect("canonical macOS skill must be readable");

        for required in [
            "screen_recording_capturable` is `null",
            "direct_capture_status` is `\"not_checked\"",
            "include_screenshot:false",
            "element-indexed AX actions",
        ] {
            assert!(
                macos.contains(required),
                "macOS skill lost required permission guidance: {required}"
            );
        }
    }

    #[test]
    fn bundled_skill_keeps_filesystem_outcome_ladder_and_gui_proof_boundaries() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let skill = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/SKILL.md"))
            .expect("canonical skill must be readable");

        for required in [
            "headless filesystem or command capability",
            "perform one batch-safe operation",
            "filesystem rename committed",
            "issue that modified click with",
            "every intended item is selected",
            "source reflects copy-versus-move semantics",
        ] {
            assert!(
                skill.contains(required),
                "skill lost required filesystem outcome guidance: {required}"
            );
        }
    }

    #[test]
    fn bundled_skill_keeps_semantic_clipboard_outcome_ladder() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let skill = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/SKILL.md"))
            .expect("canonical skill must be readable");
        let browser = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/BROWSER.md"))
            .expect("canonical browser skill must be readable");

        for required in [
            "exact value on the system clipboard",
            "`clipboard_write`",
            "`clipboard_read`",
            "passive page-text ref",
        ] {
            assert!(
                skill.contains(required),
                "skill lost required clipboard outcome guidance: {required}"
            );
        }
        for required in [
            "exact page content on the system clipboard",
            "passive headings and text nodes are evidence sources",
            "foreground escalation rules in `SKILL.md`",
        ] {
            assert!(
                browser.contains(required),
                "browser skill lost required clipboard outcome guidance: {required}"
            );
        }
    }

    const HISTORY_CONSULTATION_POLICY: &[&str] = &[
        "continue, resume, or recall prior Cua work",
        "call `history_status` first",
        "one bounded initial",
        "before broad application or window discovery",
        "metadata only as a lead",
        "verify current state",
        "Content, geometry, arguments, results, and user intent",
        "remain unknown",
        "session or sequence boundary",
        "never broaden a query to reconstruct excluded fields",
        "either tool is absent",
        "access is denied",
        "query is empty",
        "history is unhealthy",
        "unrelated tasks merely because the tools are advertised",
        "never mutate history",
        "lifecycle or settings",
    ];

    fn assert_history_consultation_policy(skill: &str, source: &str) {
        let normalized = skill.split_whitespace().collect::<Vec<_>>().join(" ");
        for required in HISTORY_CONSULTATION_POLICY {
            assert!(
                normalized.contains(required),
                "{source} lost required history consultation guidance: {required}"
            );
        }
    }

    #[test]
    fn bundled_skill_keeps_conditional_history_consultation_policy() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let skill = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/SKILL.md"))
            .expect("canonical skill must be readable");

        assert!(
            skill.lines().any(|line| {
                line.starts_with("description:")
                    && line.contains("continue, resume, or recall recent Cua activity")
            }),
            "skill frontmatter must trigger for recent Cua activity continuation"
        );
        assert_history_consultation_policy(&skill, "canonical skill");
    }

    #[test]
    fn installed_skill_pack_keeps_history_consultation_policy() {
        let dest = tempdir().unwrap();
        let local = dest.path().join("skills").join("cua-driver");

        write_bundled_into(&local, false).unwrap();

        let packaged = std::fs::read_to_string(local.join("SKILL.md"))
            .expect("installed skill must be readable");
        assert_history_consultation_policy(&packaged, "installed skill pack");
    }

    #[test]
    fn bundled_skill_keeps_sessions_and_authorization_as_separate_concepts() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let skill = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/SKILL.md"))
            .expect("canonical skill must be readable");
        let browser = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/BROWSER.md"))
            .expect("canonical browser skill must be readable");

        for required in [
            "Choose the target on each action",
            "transport's implicit session",
            "prefer a short public",
            "pass the same label on every call that accepts it",
            "Passing it once is not sticky",
            "revive a name after",
            "There is no `deescalate_session`",
            "--capability-manifest",
            "--approve-capability-manifest",
            "It can remove tools or typed resources",
            "permission authority. A public session",
        ] {
            assert!(
                skill.contains(required),
                "skill lost required lifecycle/authorization guidance: {required}"
            );
        }
        for forbidden in [
            "one-way session phase",
            "if session policy allows",
            "Pass `session` on the first action",
        ] {
            assert!(
                !skill.contains(forbidden),
                "skill restored stale session-state guidance: {forbidden}"
            );
        }
        for required in [
            "start_session(session?)",
            "optional; can name before acting",
            "prefer a short `session` label",
            "Passing it once is not sticky",
            "one-shot CLI calls use disposable transports",
        ] {
            assert!(
                browser.contains(required),
                "browser skill lost required session guidance: {required}"
            );
        }
    }

    #[test]
    fn bundled_skill_keeps_agent_control_non_interfering_by_default() {
        let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let skill = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/SKILL.md"))
            .expect("canonical skill must be readable");
        let linux = std::fs::read_to_string(crate_dir.join("../../Skills/cua-driver/LINUX.md"))
            .expect("canonical Linux skill must be readable");

        for required in [
            "`delivery_mode:\"foreground\"` is a user-visible takeover boundary",
            "ordinary `move_cursor({x,y})` moves only this synthetic cursor",
            "must not be used unless the user asked for",
            "do not retry automatically",
            "stop with the driver's refusal instead of silently escalating",
        ] {
            assert!(
                skill.contains(required),
                "skill lost required agent-control safety guidance: {required}"
            );
        }
        for required in [
            "keyboard actions re-show it automatically",
            "Never select it automatically",
            "explicitly authorized `delivery_mode:\"foreground\"`",
        ] {
            assert!(
                linux.contains(required),
                "Linux skill lost required non-interference guidance: {required}"
            );
        }
    }

    #[test]
    fn install_writes_the_bundled_pack_with_only_the_host_platform_guide() {
        // The bundled pack carries guides for all three platforms, but a given
        // host only needs one. all_platforms=false skips the other two. README,
        // SKILL, and the cross-platform guides are always written.
        let root = tempdir().unwrap();
        let dest = root.path().join("skills").join("cua-driver");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("STALE.md"), "left over from an older pack").unwrap();

        write_bundled_into(&dest, /*all_platforms=*/ false).unwrap();

        assert!(
            !dest.join("STALE.md").exists(),
            "update must replace, not merge"
        );
        for f in [
            "README.md",
            "SKILL.md",
            "RECORDING.md",
            "BROWSER.md",
            "EMBEDDING.md",
        ] {
            assert!(
                dest.join(f).exists(),
                "{f} should be present after a host-only install"
            );
        }
        // Exactly one platform guide should land — whichever matches this
        // test's compile target. The other two must be absent.
        #[cfg(target_os = "windows")]
        let expected_present = "WINDOWS.md";
        #[cfg(target_os = "linux")]
        let expected_present = "LINUX.md";
        #[cfg(target_os = "macos")]
        let expected_present = "MACOS.md";
        #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
        let expected_present = "";
        for f in ["WINDOWS.md", "MACOS.md", "LINUX.md"] {
            let exists = dest.join(f).exists();
            if f == expected_present {
                assert!(exists, "{f} (host guide) should be present");
            } else if !expected_present.is_empty() {
                assert!(!exists, "{f} (non-host guide) should NOT be present");
            }
        }
    }

    #[test]
    fn install_all_platforms_flag_keeps_every_platform_guide() {
        let root = tempdir().unwrap();
        let dest = root.path().join("cua-driver");

        write_bundled_into(&dest, /*all_platforms=*/ true).unwrap();

        for (f, body) in BUNDLED_SKILL_FILES {
            assert_eq!(
                std::fs::read_to_string(dest.join(f)).unwrap(),
                *body,
                "--all-platforms should write {f} verbatim"
            );
        }
    }
}
