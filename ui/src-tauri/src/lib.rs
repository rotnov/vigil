mod process_tree;
mod vigil_cli;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tauri::{AppHandle, Manager};
use tauri_plugin_deep_link::DeepLinkExt;
use tauri_plugin_notification::NotificationExt;

// Learn more about Tauri commands at https://tauri.app/develop/calling-rust/
#[tauri::command]
fn greet(name: &str) -> String {
    format!("Hello, {}! You've been greeted from Rust!", name)
}

/// How long `vigil investigate` may run before `vigil-ui` gives up and
/// kills it. It shells out to a real Claude Agent SDK session, so a
/// seconds-scale bound would fire on perfectly healthy runs; the main crate
/// has no wall-clock convention to match (its only bound on an agent
/// session is `agent/src/vigil_agent/diagnose.py`'s `MAX_INVESTIGATION_TURNS
/// = 15`), so this is picked to sit comfortably above any investigation
/// this project has actually observed while still being bounded — the
/// design spec requires an explicit error state instead of an indefinite
/// spinner when a spawn "hangs past a reasonable timeout".
const INVESTIGATE_TIMEOUT: Duration = Duration::from_secs(600);

/// Same reasoning as `INVESTIGATE_TIMEOUT`, with a longer budget: `vigil
/// fix` runs the same kind of agent session (`MAX_EXECUTION_TURNS = 10`)
/// *plus* whatever the approved steps actually do on the machine.
const FIX_TIMEOUT: Duration = Duration::from_secs(900);

/// How often the wait loop in `run_with_timeout` re-checks a still-running
/// child. Short enough that the timeout is honored promptly, long enough
/// that waiting on a 10-minute agent session costs essentially nothing
/// (vigil's own overhead counts against this project's governing goal).
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Spawns `args`, optionally writes `stdin_text` to the child, and waits up
/// to `timeout` for it to exit — killing it and returning an error string
/// if it doesn't. Shared by the two commands that spawn a real agent
/// session (`investigate`, `run_fix`); the error string flows back through
/// the same `Result<_, String>` path the frontend already `catch`es, so a
/// timeout surfaces exactly like a non-zero exit does.
///
/// Both pipes are drained on their own threads: a child that fills
/// stdout's or stderr's buffer blocks until someone reads it, and the
/// `try_wait` poll loop below never would — the process would sit there
/// until the timeout killed it, turning a healthy-but-chatty run into a
/// spurious timeout.
fn run_with_timeout(args: &[String], stdin_text: Option<&str>, timeout: Duration, what: &str) -> Result<String, String> {
    use std::io::{Read, Write};
    use std::process::Stdio;

    let mut child = std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch {what}: {e}"))?;

    // Write the approvals (when there are any) and *close* the pipe either
    // way — `take()` moves the handle into this block, so it drops here.
    // `vigil fix`'s prompt loop reads one line per step and would block
    // forever on a stdin handle left open; `vigil investigate` reads
    // nothing but should still see EOF rather than an open pipe.
    if let Some(mut stdin) = child.stdin.take() {
        if let Some(text) = stdin_text {
            let _ = stdin.write_all(text.as_bytes());
        }
    }

    fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Option<std::thread::JoinHandle<Vec<u8>>> {
        pipe.map(|mut p| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let _ = p.read_to_end(&mut buf);
                buf
            })
        })
    }
    let out_thread = drain(child.stdout.take());
    let err_thread = drain(child.stderr.take());

    let deadline = std::time::Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("{what} timed out after {}s and was terminated", timeout.as_secs()));
                }
                std::thread::sleep(CHILD_POLL_INTERVAL);
            }
            Err(e) => return Err(format!("{what} did not exit cleanly: {e}")),
        }
    };

    let stdout = out_thread.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_thread.and_then(|h| h.join().ok()).unwrap_or_default();
    if status.success() {
        Ok(String::from_utf8_lossy(&stdout).trim().to_string())
    } else {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        // A non-zero exit with nothing on stderr would otherwise render as
        // an empty error in the window — say *something* instead.
        Err(if message.is_empty() { format!("{what} exited with {status}") } else { message })
    }
}

/// `async` so Tauri runs it off the main thread (a plain `#[tauri::command]`
/// executes in blocking mode and would freeze the whole app for the length
/// of an agent session). A non-`async fn` marked this way runs on Tauri's
/// sync threadpool, which is what the blocking spawn inside wants.
#[tauri::command(async)]
fn investigate(alert_key: String, incidents_dir: String) -> Result<(), String> {
    let args = crate::vigil_cli::build_investigate_args(&vigil_bin(), &alert_key, &incidents_dir, agent_dir().as_deref());
    run_with_timeout(&args, None, INVESTIGATE_TIMEOUT, "vigil investigate").map(|_| ())
}

#[tauri::command]
fn read_incident_json(incidents_dir: String, path: String) -> Result<serde_json::Value, String> {
    // Fast, LLM-free and synchronous, so it stays a blocking command with
    // no timeout — but it *is* a place a frontend-supplied path first
    // reaches the real `vigil` CLI, so it re-checks containment (see
    // `is_allowed_incident_path`).
    reject_path_outside_incidents_dir(&path)?;
    let args = crate::vigil_cli::build_show_json_args(&vigil_bin(), &incidents_dir, &path);
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| format!("failed to launch vigil incidents: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("failed to parse vigil's JSON output: {e}"))
}

#[tauri::command]
fn process_tree(alert_key: String) -> Vec<crate::process_tree::ProcessNode> {
    let scope = crate::process_tree::scope_for_alert_key(&alert_key);
    // `System::new()` rather than `new_all()`: `query_process_tree` refreshes
    // exactly the process data it needs, and this window never looks at
    // disks/networks/components, which `new_all()` would scan on every call.
    let mut sys = sysinfo::System::new();
    crate::process_tree::query_process_tree(&mut sys, &scope)
}

/// `async` for the same reason as `investigate` — this one spawns the
/// execute-agent session, the longest-running thing this app ever waits on.
#[tauri::command(async)]
fn run_fix(path: String, approvals: Vec<bool>) -> Result<String, String> {
    reject_path_outside_incidents_dir(&path)?;
    let args = crate::vigil_cli::build_fix_args(&vigil_bin(), &path, agent_dir().as_deref());
    let stdin_text = crate::vigil_cli::build_fix_stdin(&approvals);
    run_with_timeout(&args, Some(&stdin_text), FIX_TIMEOUT, "vigil fix")
}

/// The `Err` form of `is_allowed_incident_path` against the live
/// `incidents_dir()`, for the two commands that hand a frontend-supplied
/// path to the real `vigil` CLI.
fn reject_path_outside_incidents_dir(path: &str) -> Result<(), String> {
    if is_allowed_incident_path(path, &incidents_dir()) {
        Ok(())
    } else {
        Err(format!("refusing to act on a path outside the incidents directory: {path}"))
    }
}

/// The `vigil` binary to shell out to. Defaults to the bare name (resolved
/// via `$PATH`, today's pre-existing behavior for anyone who hasn't set
/// this) -- `VIGIL_BIN` overrides it with an absolute path, which real
/// deployment (Task 11's LaunchAgent) always sets, since a launchd agent's
/// `$PATH` is minimal and cannot be relied on to contain a dev-built
/// `target/release/vigil`.
fn vigil_bin() -> String {
    std::env::var("VIGIL_BIN").unwrap_or_else(|_| "vigil".to_string())
}

/// The main `vigil` crate's sibling `agent/` Python project, passed to
/// `vigil investigate`/`vigil fix` as `--agent-dir` so they don't fall back
/// to their own relative-to-cwd default (which never resolves correctly
/// from `vigil-ui`'s actual runtime cwd -- see this task's own commit
/// message for the bug this fixes). `None` when unset: `vigil_cli`'s
/// builders simply omit `--agent-dir` in that case, reproducing today's
/// already-broken default rather than guessing a path that could be wrong
/// in a different, silent way (e.g. a compile-time-baked path pointing at
/// a since-removed worktree).
fn agent_dir() -> Option<String> {
    std::env::var("VIGIL_AGENT_DIR").ok()
}

/// Tracks whether the "main" webview has finished its first page load yet.
/// `open_incident_window` is called from four places (the poller, the
/// single-instance callback, and both deep-link paths) any of which can
/// fire before that first load completes -- most realistically the
/// poller, whose background thread starts concurrently with `.setup()`
/// returning, with no ordering guarantee against the webview's own load.
/// Before this existed, a navigation attempted in that window silently
/// no-op'd or got clobbered by the page's own in-flight initial load,
/// permanently losing that incident (the poller had already marked it
/// `seen`). Now: a too-early navigation is queued in `pending` instead,
/// and applied once a one-time `tauri://load` listener (registered in
/// `run()`'s `.setup()`) observes the load actually completing.
struct WindowReady {
    ready: AtomicBool,
    pending: Mutex<Option<(String, bool)>>,
}

impl WindowReady {
    fn new() -> Arc<Self> {
        Arc::new(Self { ready: AtomicBool::new(false), pending: Mutex::new(None) })
    }
}

/// Prepares the "main" window's content for `path` (an incident file path)
/// and brings it forward. Called from both triggers this task wires up:
/// the deep-link handoff (Step 4, `vigil://incident/<path>` from the
/// menu-bar binary) and the incidents-directory poller (Step 5, a new
/// journal-worthy incident detected locally) -- both ultimately want the
/// same "window shows this incident" outcome, just reached from different
/// events.
///
/// `focus` is `false` only for the poller (Step 5): the user hasn't done
/// anything yet at the moment a new incident is detected, so yanking focus
/// away from whatever they're doing would itself be a small tax against
/// this project's governing goal (see AGENTS.md -- vigil's own attention
/// cost counts, the same reasoning `IncidentTracker` exists for). Every
/// other caller passes `true`: a `vigil://` deep link (cold start, running,
/// or routed through single-instance) is something the user just actively
/// triggered by clicking a menu-bar item or notification, so focusing is
/// the expected outcome there.
fn open_incident_window(app: &AppHandle, ready: &WindowReady, path: &str, focus: bool) {
    // Every path that reaches a window — deep link, single-instance, cold
    // start, poller — is checked here, one place, before it can become a
    // `?path=` the frontend then feeds back into `vigil incidents --show`
    // and possibly `vigil fix`. A registered URL scheme is triggerable by
    // anything else on the machine, so an unconstrained path would let
    // arbitrary attacker-chosen markdown render inside vigil's own trusted
    // window. Logged rather than silently dropped: a legitimate path
    // failing this check would otherwise be an undebuggable no-op.
    if !is_allowed_incident_path(path, &incidents_dir()) {
        eprintln!("[vigil-ui] ignoring incident path outside the incidents directory: {path}");
        return;
    }
    // Lock `pending` *before* checking `ready`, rather than checking
    // `ready` first and only locking in the `else` branch -- otherwise
    // there's a race against the `on_page_load` hook's own drain (see
    // `run()`): this call could observe `ready == false`, get preempted,
    // have the hook flip `ready` true and drain an already-empty
    // `pending`, and only then write into `pending` -- which nothing
    // would ever read again, since every later `on_page_load` firing hits
    // the hook's `!swap(...)` guard and skips draining. Serializing both
    // paths on the same mutex (the hook also locks `pending` before its
    // own `swap`) closes that window entirely.
    let mut pending = ready.pending.lock().unwrap();
    if ready.ready.load(Ordering::SeqCst) {
        drop(pending);
        navigate_to_incident(app, path, focus);
    } else {
        // Too early -- the webview hasn't finished its first load yet.
        // Queue it; the `on_page_load` hook registered in `run()` applies
        // whatever's queued once it observes the load actually finishing.
        // A second queued path before the first one is ever applied
        // simply overwrites the first (last-wins) -- in practice this
        // only matters for the poller's very first few ticks before the
        // window has loaded even once, a narrow window where at most one
        // real incident is likely to land.
        *pending = Some((path.to_string(), focus));
    }
}

/// Pure — the frontend URL for one incident. `auto` carries the same
/// user-initiated signal `focus` does: `auto=1` means a human just asked
/// for this window (deep link, menu-bar click routed through
/// single-instance, cold start by URL), `auto=0` means the poller
/// pre-navigated silently with nobody looking yet.
///
/// `incident.js` gates its `investigate` call on exactly that bit, and this
/// is the whole reason the parameter exists: without it, the poller's
/// silent pre-navigation would run a real `vigil investigate` — a real
/// agent session, real tokens — for every journal-worthy alert with no user
/// action at all, which contradicts this project's stated opt-in rule
/// ("Investigation is opt-in, not automatic ... No agent process spawns
/// until the user explicitly runs that command", AGENTS.md).
fn incident_url(path: &str, focus: bool) -> String {
    format!("index.html?path={}&auto={}", urlencoding::encode(path), if focus { "1" } else { "0" })
}

fn navigate_to_incident(app: &AppHandle, path: &str, focus: bool) {
    let url = incident_url(path, focus);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval(&format!("window.location.replace('{url}')"));
        let _ = window.show();
        if focus {
            let _ = window.set_focus();
        }
    }
}

/// Pure -- parses `vigil://incident/<url-encoded-path>` into the decoded
/// path, or `None` for anything else (wrong scheme, wrong prefix, missing
/// path). Deliberately `&str` in rather than whatever type
/// `tauri-plugin-deep-link`'s `.urls()`/`.get_current()` return -- callers
/// convert via `.to_string()` at each call site, so this helper only needs
/// to match the plugin's `Display`/`ToString` output, not its exact
/// wrapped type. Kept as a plain function (no Tauri types) so it's
/// testable without spinning up an app.
fn parse_incident_url(url: &str) -> Option<String> {
    let path = url.strip_prefix("vigil://incident/")?;
    urlencoding::decode(path).ok().map(|s| s.into_owned())
}

/// Pure — is `path` a plausible incident file: a `.md` file lexically
/// inside `incidents_dir`, with no `..` component anywhere?
///
/// Kept separate from `parse_incident_url` (which stays purely about URL
/// shape) and kept *lexical* rather than filesystem-based on purpose:
/// - `parse_incident_url` has unit tests that touch no filesystem; folding
///   a `canonicalize` into it would make them depend on real files
///   existing, and the check is needed at more call sites than just the
///   URL one anyway (both Tauri commands take a `path` straight from the
///   frontend).
/// - The file may legitimately not exist yet at check time — the poller
///   can see a stub mid-write — and `canonicalize` fails outright on a
///   missing path, so it would reject valid incidents.
/// - Canonicalizing only one side is its own trap on macOS, where
///   `/tmp` → `/private/tmp` is a symlink: a smoke test run with
///   `VIGIL_UI_INCIDENTS_DIR=/tmp/...` would compare a resolved path
///   against an unresolved directory and mysteriously reject everything.
///
/// Rejecting any `..` component (rather than resolving them) is what makes
/// the lexical comparison sound: without `..` there is no way for a path
/// that starts with `incidents_dir`'s components to escape it. `starts_with`
/// is component-wise, so a sibling directory sharing a name prefix
/// (`.../incidents-evil/x.md`) is rejected too.
fn is_allowed_incident_path(path: &str, incidents_dir: &Path) -> bool {
    let path = Path::new(path);
    if path.extension().is_none_or(|ext| ext != "md") {
        return false;
    }
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return false;
    }
    path.starts_with(incidents_dir)
}

/// Where the incidents-directory poller (Step 5) looks for new incident
/// files. `VIGIL_UI_INCIDENTS_DIR` is the one override the manual smoke
/// test (and any future automated test of the poller) needs; real usage
/// falls back to the same fixed, home-relative path
/// `incidents::write_stub` on the main crate writes to
/// (`~/.vigil/incidents`) -- reimplemented here since `ui/` doesn't share
/// a crate with `vigil`.
fn incidents_dir() -> PathBuf {
    std::env::var("VIGIL_UI_INCIDENTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".vigil").join("incidents"))
}

/// Lists `{dir}/*.md`, sorted -- the same convention `incidents::list` on
/// the main crate uses, reimplemented here with plain `std::fs::read_dir`
/// since `ui/` doesn't share a crate with `vigil`. Empty (not an error)
/// when `dir` doesn't exist yet, matching `incidents::list`'s own
/// fresh-install behavior.
fn list_incident_files(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
        .collect();
    entries.sort();
    entries
}

/// The text after `**Rule message:**` on its own line, mirroring
/// `incidents::extract_rule_message` on the main crate (not shared here
/// since `ui/` is a separate Rust project) -- what the poller uses as the
/// notification body.
fn extract_rule_message(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix("**Rule message:**"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// How far before this poller started an incident file's mtime may be and
/// still count as "new" -- generous enough to cover `vigil-ui`'s own
/// build/startup latency (the manual smoke test in Step 6 writes its
/// fixture file, then runs `npm run tauri dev`, which can take real time
/// to build and start; that fixture must still fire), while excluding the
/// hundreds of incident files a machine that's run `vigil watch` for any
/// length of time will already have on disk (verified against this dev
/// machine's real `~/.vigil/incidents`: 305 files at the time this was
/// written) -- without this, every one of those would notify and flip the
/// window's content the moment `vigil-ui` launches.
const STARTUP_GRACE: Duration = Duration::from_secs(300);

/// Every few seconds, checks `incidents_dir()` for `.md` files this poller
/// hasn't handled yet. For each new one that looks like a stub `vigil
/// watch`/`vigil investigate` wrote (a plain `contains("**Alert key:**")`
/// check -- this file has no dependency on the main crate's real
/// markdown parser) and whose mtime is recent (see `STARTUP_GRACE`),
/// prepares the window's content for it *before* posting a notification,
/// per this task's corrected design (see the brief's "IMPORTANT" note):
/// the notification plugin's Actions API is mobile-only, so there's no
/// click-carries-the-path callback on macOS -- the window must already be
/// content-ready by the time the user clicks, since clicking only
/// activates (foregrounds) the app, nothing more specific.
///
/// Runs for the process's lifetime on its own thread -- `ui/`'s equivalent
/// of `vigil watch`'s own poll loop, just scoped to "is there a new
/// incident to surface," not metric collection. A `HashSet` of already
/// seen paths is enough; it doesn't need to survive a `vigil-ui` restart,
/// since a missed notification just means the user finds the incident via
/// the menu-bar dropdown instead (see `menubar_loop.rs`). A path is only
/// added to `seen` once its content has actually been read and classified
/// (stub vs. not) -- inserting any earlier would permanently drop an
/// incident this poller happened to catch mid-write, since `vigil watch`'s
/// stub write is not guaranteed atomic from this reader's point of view.
fn spawn_incident_poller(app: AppHandle, ready: Arc<WindowReady>) {
    let poller_started_at = std::time::SystemTime::now();
    std::thread::spawn(move || {
        let mut seen: HashSet<PathBuf> = HashSet::new();
        loop {
            let dir = incidents_dir();
            for path in list_incident_files(&dir) {
                if seen.contains(&path) {
                    continue;
                }

                let Ok(content) = std::fs::read_to_string(&path) else {
                    continue; // may still be mid-write; retry next poll, not yet seen
                };
                if !content.contains("**Alert key:**") {
                    continue; // not (yet) a stub; retry next poll, not yet seen
                }
                seen.insert(path.clone());

                let is_recent = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|mtime| mtime + STARTUP_GRACE >= poller_started_at)
                    .unwrap_or(false);
                if !is_recent {
                    continue;
                }

                let path_str = path.to_string_lossy().to_string();
                open_incident_window(&app, &ready, &path_str, false);

                let body = extract_rule_message(&content).unwrap_or("New incident detected.");
                app.notification()
                    .builder()
                    .title("vigil")
                    .body(body)
                    .show()
                    .map_err(|e| eprintln!("[vigil-ui] failed to post notification: {e}"))
                    .ok();
            }
            std::thread::sleep(Duration::from_secs(3));
        }
    });
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // Shared between the `.on_page_load(...)` hook below (registered on the
    // `Builder` chain, before any window exists) and `.setup()`'s closure
    // (the deep-link paths and the poller) -- both need the same `ready` so
    // a navigation attempted anywhere before the "main" webview's first
    // load completes gets queued, and the queue drains exactly once when
    // that load actually finishes.
    let ready = WindowReady::new();
    let ready_for_page_load = ready.clone();
    let ready_for_setup = ready.clone();

    tauri::Builder::default()
        // Must be the first `.plugin(...)` call -- it needs to intercept
        // app startup (and decide whether this process should even keep
        // running past that point) before anything else initializes. On a
        // second launch attempt while an instance is already running, its
        // callback receives that second attempt's argv/cwd instead of a
        // second process ever fully starting; route it the same place a
        // `vigil://` deep link goes (Step 4) when one is present, or just
        // surface the existing window otherwise.
        .plugin({
            // This callback fires on a *second launch attempt while
            // already running*, meaning the window from the *first*
            // launch has necessarily already finished loading by then in
            // any realistic scenario. It cannot capture the `ready` state
            // `.setup()` constructs below -- plugin registration happens
            // before `.setup()` runs, and that `ready` doesn't exist yet
            // at this point in the builder chain. Give it its own,
            // independently-constructed `WindowReady`, pre-marked ready
            // (never flipped `false->true` like the shared one) so this
            // call site always navigates immediately, matching its actual
            // real-world timing.
            let ready = WindowReady::new();
            ready.ready.store(true, Ordering::SeqCst);
            tauri_plugin_single_instance::init(move |app, args, _cwd| {
                if let Some(path) = args.iter().find_map(|a| parse_incident_url(a)) {
                    open_incident_window(app, &ready, &path, true);
                } else if let Some(window) = app.get_webview_window("main") {
                    let _ = window.show();
                    let _ = window.set_focus();
                }
            })
        })
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_notification::init())
        // The real "has the main webview finished its first load" signal.
        // An earlier version of this fix listened for a window event named
        // `tauri://load`, which does not exist in Tauri 2.11.5 -- checked
        // directly against this crate's own source
        // (`tauri-2.11.5/src/manager/window.rs`'s `EventName::from_str`
        // constants list every `tauri://`-namespaced window event it
        // actually emits: resize/move/close-requested/destroyed/focus/
        // blur/scale-change/theme-changed/drag-*/suspended/resumed/
        // webview-created/window-created -- no `load`), so that listener
        // would never have fired and `ready` would have stayed `false` for
        // the process's entire lifetime, queuing every incident forever
        // instead of just the racy first one. `Builder::on_page_load` is
        // the hook Tauri actually documents for this
        // (`tauri-2.11.5/src/webview/webview_window.rs`'s own doc example),
        // firing with `PageLoadEvent::Started`/`Finished` for every webview
        // page load, this app's initial one included. Filtered to the
        // "main" webview (the only one this app creates) and to
        // `Finished`; `swap` instead of `load`+`store` so a later
        // navigation firing this same hook again (including this fix's own
        // `navigate_to_incident` calls, which themselves trigger a new page
        // load) is a no-op rather than re-draining an already-empty queue.
        .on_page_load(move |webview, payload| {
            if webview.label() != "main" || payload.event() != tauri::webview::PageLoadEvent::Finished {
                return;
            }
            // Lock `pending` *before* the `swap`, mirroring
            // `open_incident_window`'s own lock-before-check ordering, so
            // this drain and a concurrent `open_incident_window` call can
            // never interleave (see that function's doc comment for the
            // race this closes).
            let mut pending = ready_for_page_load.pending.lock().unwrap();
            if !ready_for_page_load.ready.swap(true, Ordering::SeqCst) {
                if let Some((path, focus)) = pending.take() {
                    drop(pending);
                    navigate_to_incident(webview.app_handle(), &path, focus);
                }
            }
        })
        .setup(move |app| {
            // `LSUIElement` in Info.plist does NOT suppress the Dock icon
            // on this installed Tauri/tao combination -- tao's AppDelegate
            // hardcodes ActivationPolicy::Regular unconditionally on every
            // launch (verified in Task 6 against tao 0.35.3's own source),
            // ignoring Info.plist entirely, in dev and in a real bundled
            // .app alike. This is the same fix `menubar_loop.rs` already
            // relies on (there, called directly through tao's own
            // `EventLoopExtMacOS::set_activation_policy`, since that file
            // drives tao's event loop directly rather than through Tauri).
            #[cfg(target_os = "macos")]
            let _ = app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);

            // Closing the window must not destroy it: the incidents poller
            // lives in this same process, so a destroyed last window would
            // take journal-worthy alert notification down with it (nothing
            // else posts those anymore -- `vigil watch` stopped in
            // 51656bd), and `launchd`'s `KeepAlive` would just relaunch a
            // fresh process in a loop. Hiding instead also preserves
            // `WindowReady`'s `ready` latch and whatever page is loaded, so
            // the next incident is a plain `navigate_to_incident` rather
            // than a re-queue against a window that never loads again.
            // `on_window_event`'s closure takes `&WindowEvent`, and
            // `CloseRequestApi::prevent_close` takes `&self` (verified in
            // tauri-2.11.5/src/app.rs and webview/webview_window.rs).
            if let Some(window) = app.get_webview_window("main") {
                let hide_target = window.clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        api.prevent_close();
                        let _ = hide_target.hide();
                    }
                });
            }

            // `ready_for_setup` is the same `WindowReady` the
            // `.on_page_load(...)` hook above flips once the "main"
            // webview's first load actually completes -- these paths, and
            // that hook, share one queue.
            let ready = ready_for_setup;

            let handle = app.handle().clone();
            // Cold start: the app was launched *by* a `vigil://` URL.
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                if let Some(path) = urls.first().and_then(|u| parse_incident_url(&u.to_string())) {
                    open_incident_window(&handle, &ready, &path, true);
                }
            }
            // Already running: single-instance (above) routes the second
            // invocation's URL here instead of spawning a second process.
            let handle = app.handle().clone();
            let ready_for_deep_link = ready.clone();
            app.deep_link().on_open_url(move |event| {
                if let Some(path) = event.urls().first().and_then(|u| parse_incident_url(&u.to_string())) {
                    open_incident_window(&handle, &ready_for_deep_link, &path, true);
                }
            });

            spawn_incident_poller(app.handle().clone(), ready.clone());

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![greet, investigate, read_incident_json, process_tree, run_fix])
        // `.build(...)` + `.run(closure)` rather than the plain
        // `.run(context)`, which installs no `RunEvent` handling at all:
        // with nothing calling `api.prevent_exit()`, `RunEvent::ExitRequested`
        // (fired when the last window is destroyed) exits the whole process
        // -- taking the incidents poller with it. `.build` returns
        // `crate::Result<App>`, so the `.expect` that used to sit on `.run`
        // moves here; `App::run` itself returns `()`.
        //
        // Only a user-interaction exit is prevented: `ExitRequested`'s
        // `code` is `None` for those and `Some(_)` for a programmatic
        // `AppHandle::exit`/`restart`, which stays honored. Verified live
        // (see this round's smoke run): clicking the window's close button
        // leaves the process running and a later incident still opens the
        // window. Whether macOS Cmd-Q routes through this handler at all
        // was NOT verified; either way the supported way to stop this app
        // is `launchctl unload ~/Library/LaunchAgents/com.vigil.ui.plist`
        // (or killing the process) -- deliberate for a background-resident
        // agent whose whole job is to still be running when an alert fires.
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app_handle, event| {
            if let tauri::RunEvent::ExitRequested { api, code, .. } = event {
                if code.is_none() {
                    api.prevent_exit();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_incident_url_round_trips_a_simple_path() {
        let url = format!("vigil://incident/{}", urlencoding::encode("/tmp/incidents/x.md"));
        assert_eq!(parse_incident_url(&url), Some("/tmp/incidents/x.md".to_string()));
    }

    #[test]
    fn parse_incident_url_decodes_spaces_and_slashes() {
        let raw = "/Users/denis/.vigil/incidents/2026-08-12-00-00-00-cpu hog 1.md";
        let url = format!("vigil://incident/{}", urlencoding::encode(raw));
        assert_eq!(parse_incident_url(&url), Some(raw.to_string()));
    }

    #[test]
    fn parse_incident_url_rejects_the_wrong_scheme() {
        assert_eq!(parse_incident_url("http://incident/foo"), None);
    }

    #[test]
    fn parse_incident_url_rejects_the_wrong_prefix() {
        assert_eq!(parse_incident_url("vigil://other/foo"), None);
    }

    #[test]
    fn parse_incident_url_rejects_a_bare_scheme_with_no_path() {
        assert_eq!(parse_incident_url("vigil://incident/"), Some(String::new()));
        assert_eq!(parse_incident_url("vigil://incident"), None);
    }

    #[test]
    fn is_allowed_incident_path_accepts_a_markdown_file_in_the_incidents_dir() {
        let dir = Path::new("/Users/denis/.vigil/incidents");
        assert!(is_allowed_incident_path("/Users/denis/.vigil/incidents/2026-08-12-00-00-00-cpu-hog-1.md", dir));
    }

    #[test]
    fn is_allowed_incident_path_accepts_a_file_in_this_machine_s_real_incidents_dir() {
        // The `vigil menubar` -> `vigil://incident/<path>` handoff builds
        // its path from the main crate's `incidents::default_dir()`
        // (`$HOME/.vigil/incidents`, absolute), while this crate computes
        // the same location from `dirs::home_dir()`. If those ever
        // diverged, the containment check would silently reject every deep
        // link (an eprintln and a window that never opens), so pin the
        // shape they have to share.
        let dir = incidents_dir();
        assert!(dir.is_absolute(), "incidents_dir() must be absolute, got {dir:?}");
        let path = dir.join("2026-08-12-00-00-00-cpu-hog-1.md");
        assert!(is_allowed_incident_path(&path.to_string_lossy(), &dir));
    }

    #[test]
    fn is_allowed_incident_path_rejects_traversal_out_of_the_incidents_dir() {
        let dir = Path::new("/Users/denis/.vigil/incidents");
        assert!(!is_allowed_incident_path("/Users/denis/.vigil/incidents/../../evil.md", dir));
        assert!(!is_allowed_incident_path("/Users/denis/.vigil/incidents/../.ssh/known_hosts.md", dir));
    }

    #[test]
    fn is_allowed_incident_path_rejects_a_path_outside_the_incidents_dir() {
        let dir = Path::new("/Users/denis/.vigil/incidents");
        assert!(!is_allowed_incident_path("/tmp/evil.md", dir));
        assert!(!is_allowed_incident_path("relative.md", dir));
    }

    #[test]
    fn is_allowed_incident_path_rejects_a_sibling_dir_sharing_a_name_prefix() {
        // The classic string-prefix bug: `starts_with` on `Path` is
        // component-wise, so this is rejected -- locked in by a test since
        // a naive `str::starts_with` rewrite would silently accept it.
        let dir = Path::new("/Users/denis/.vigil/incidents");
        assert!(!is_allowed_incident_path("/Users/denis/.vigil/incidents-evil/x.md", dir));
    }

    #[test]
    fn is_allowed_incident_path_rejects_a_non_markdown_file() {
        let dir = Path::new("/Users/denis/.vigil/incidents");
        assert!(!is_allowed_incident_path("/Users/denis/.vigil/incidents/x.sh", dir));
        assert!(!is_allowed_incident_path("/Users/denis/.vigil/incidents/x", dir));
    }

    #[test]
    fn run_with_timeout_returns_a_successful_child_s_stdout() {
        let args = ["/bin/sh".to_string(), "-c".to_string(), "echo hello".to_string()];
        assert_eq!(run_with_timeout(&args, None, Duration::from_secs(10), "test"), Ok("hello".to_string()));
    }

    #[test]
    fn run_with_timeout_feeds_stdin_and_closes_it() {
        // The `vigil fix` shape: the child reads its approvals from stdin
        // and must see EOF afterwards, or it would hang until the timeout.
        let args = ["/bin/cat".to_string()];
        assert_eq!(run_with_timeout(&args, Some("y\nN\n"), Duration::from_secs(10), "test"), Ok("y\nN".to_string()));
    }

    #[test]
    fn run_with_timeout_reports_a_failing_child_s_stderr() {
        let args = ["/bin/sh".to_string(), "-c".to_string(), "echo boom >&2; exit 3".to_string()];
        assert_eq!(run_with_timeout(&args, None, Duration::from_secs(10), "test"), Err("boom".to_string()));
    }

    #[test]
    fn run_with_timeout_falls_back_to_the_exit_status_when_stderr_is_empty() {
        let args = ["/bin/sh".to_string(), "-c".to_string(), "exit 3".to_string()];
        let err = run_with_timeout(&args, None, Duration::from_secs(10), "test").unwrap_err();
        assert!(err.starts_with("test exited with"), "unexpected error: {err}");
    }

    #[test]
    fn run_with_timeout_kills_a_child_that_outlives_its_budget() {
        let args = ["/bin/sh".to_string(), "-c".to_string(), "sleep 30".to_string()];
        let started = std::time::Instant::now();
        let err = run_with_timeout(&args, None, Duration::from_millis(300), "test").unwrap_err();
        assert!(err.contains("timed out"), "unexpected error: {err}");
        assert!(started.elapsed() < Duration::from_secs(5), "should not have waited for the child to finish");
    }

    #[test]
    fn run_with_timeout_reports_a_binary_that_cannot_be_launched() {
        let args = ["/definitely/not/a/real/binary".to_string()];
        let err = run_with_timeout(&args, None, Duration::from_secs(10), "test").unwrap_err();
        assert!(err.starts_with("failed to launch test"), "unexpected error: {err}");
    }

    #[test]
    fn incident_url_marks_a_user_initiated_arrival_auto_1() {
        assert_eq!(incident_url("/tmp/incidents/x.md", true), "index.html?path=%2Ftmp%2Fincidents%2Fx.md&auto=1");
    }

    #[test]
    fn incident_url_marks_the_pollers_silent_pre_navigation_auto_0() {
        // The whole point of the parameter: `incident.js` must not run a
        // real `vigil investigate` for a window nobody has looked at yet.
        assert_eq!(incident_url("/tmp/incidents/x.md", false), "index.html?path=%2Ftmp%2Fincidents%2Fx.md&auto=0");
    }
}
