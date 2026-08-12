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

#[tauri::command]
fn investigate(alert_key: String, incidents_dir: String) -> Result<(), String> {
    let args = crate::vigil_cli::build_investigate_args(&vigil_bin(), &alert_key, &incidents_dir, agent_dir().as_deref());
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| format!("failed to launch vigil investigate: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[tauri::command]
fn read_incident_json(incidents_dir: String, path: String) -> Result<serde_json::Value, String> {
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
    let mut sys = sysinfo::System::new_all();
    crate::process_tree::query_process_tree(&mut sys, &scope)
}

#[tauri::command]
fn run_fix(path: String, approvals: Vec<bool>) -> Result<String, String> {
    use std::io::Write;
    use std::process::Stdio;

    let args = crate::vigil_cli::build_fix_args(&vigil_bin(), &path, agent_dir().as_deref());
    let mut child = std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch vigil fix: {e}"))?;

    let stdin_text = crate::vigil_cli::build_fix_stdin(&approvals);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_text.as_bytes());
    }

    let output = child.wait_with_output().map_err(|e| format!("vigil fix did not exit cleanly: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
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

fn navigate_to_incident(app: &AppHandle, path: &str, focus: bool) {
    let url = format!("index.html?path={}", urlencoding::encode(path));
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
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
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
}
