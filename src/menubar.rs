//! Menu bar health indicator.
//!
//! Reads the small status file `vigil watch` writes every tick and shows a
//! colored (or, when healthy, mostly-transparent) macOS menu bar icon —
//! deliberately *not* a second sampling/evaluation loop of its own. See
//! docs/decisions/0002-menu-bar-health-indicator.md for why.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

/// `~/.vigil/status.json` — same home-relative convention as
/// `incidents::default_dir`, for the same reason (vigil runs from anywhere).
pub fn default_status_file() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".vigil").join("status.json")
}

#[derive(Serialize, Deserialize)]
struct StatusFile {
    updated_unix: u64,
    open_count: usize,
}

/// Pure: builds the JSON body `vigil watch` writes. Kept separate from the
/// actual file write below so it's testable without touching the filesystem.
fn build_status_json(open_count: usize, now_unix: u64) -> String {
    serde_json::to_string(&StatusFile { updated_unix: now_unix, open_count }).unwrap()
}

/// Called from `vigil watch`'s tick loop. A write failure is logged, not
/// fatal — a missing/stale status file just reads as `HealthLevel::Unknown`
/// in the menu bar, which is the honest outcome for "watch isn't updating
/// this" anyway.
pub fn write_status(path: &str, open_count: usize) {
    let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Err(e) = std::fs::write(path, build_status_json(open_count, now_unix)) {
        eprintln!("[vigil] failed to write status file {path}: {e}");
    }
}

fn read_status(path: &Path) -> Option<StatusFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthLevel {
    Ok,
    Warning,
    Critical,
    /// No status file, or one old enough that `vigil watch` looks dead —
    /// shown distinctly rather than defaulting to `Ok`, since "no data" and
    /// "confirmed healthy" are different facts.
    Unknown,
}

/// Pure classification — no I/O. `status` is already-parsed file content
/// (`None` if the file is missing/unparseable).
fn classify_health(status: Option<&StatusFile>, now_unix: u64, stale_after_secs: u64) -> HealthLevel {
    let Some(status) = status else { return HealthLevel::Unknown };
    if now_unix.saturating_sub(status.updated_unix) > stale_after_secs {
        return HealthLevel::Unknown;
    }
    match status.open_count {
        0 => HealthLevel::Ok,
        1 => HealthLevel::Warning,
        _ => HealthLevel::Critical,
    }
}

const ICON_SIZE: u32 = 22;

/// A small filled (or mostly-transparent) circle, drawn procedurally rather
/// than loaded from a bundled asset — see the ADR. `Ok` is a faint outline
/// rather than fully invisible pixels, so the tray item stays locatable
/// while still reading as "nothing to see here".
fn icon_rgba(level: HealthLevel) -> (Vec<u8>, u32, u32) {
    let (r, g, b, a): (u8, u8, u8, u8) = match level {
        HealthLevel::Ok => (255, 255, 255, 40),
        HealthLevel::Warning => (255, 190, 20, 255),
        HealthLevel::Critical => (230, 50, 50, 255),
        HealthLevel::Unknown => (140, 140, 140, 200),
    };
    let size = ICON_SIZE;
    let mut buf = vec![0u8; (size * size * 4) as usize];
    let center = size as f32 / 2.0;
    let radius = size as f32 / 2.0 - 2.0;
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            if dx * dx + dy * dy <= radius * radius {
                let idx = ((y * size + x) * 4) as usize;
                buf[idx] = r;
                buf[idx + 1] = g;
                buf[idx + 2] = b;
                buf[idx + 3] = a;
            }
        }
    }
    (buf, size, size)
}

pub struct MenubarOptions {
    pub status_file: String,
    pub incidents_dir: String,
    pub poll_interval: Duration,
}

enum UserEvent {
    /// Only used to wake the event loop early on a tray click — macOS
    /// already opens the attached menu on its own, so the event's own
    /// payload is never inspected.
    Tray,
    Menu(MenuEvent),
}

/// Blocks for the process's lifetime, same as `ui::run` — launched as its
/// own long-running process alongside `vigil watch`.
pub fn run(opts: MenubarOptions) {
    let mut builder = EventLoopBuilder::<UserEvent>::with_user_event();
    let mut event_loop = builder.build();
    // Menu-bar-only utility: no Dock icon, no app-switcher entry.
    event_loop.set_activation_policy(ActivationPolicy::Accessory);

    let proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |_event| {
        let _ = proxy.send_event(UserEvent::Tray);
    }));
    let proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = proxy.send_event(UserEvent::Menu(event));
    }));

    let stale_after_secs = opts.poll_interval.as_secs().max(1) * 3;
    let mut tray: Option<TrayIcon> = None;
    let mut last_level: Option<HealthLevel> = None;
    let mut last_poll = Instant::now() - opts.poll_interval;
    let mut incident_paths: Vec<PathBuf> = Vec::new();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(400));

        match event {
            Event::NewEvents(StartCause::Init) => {
                let (rgba, w, h) = icon_rgba(HealthLevel::Unknown);
                let icon = Icon::from_rgba(rgba, w, h).expect("failed to build tray icon");
                tray = Some(
                    TrayIconBuilder::new()
                        .with_icon(icon)
                        .with_menu(Box::new(build_menu(&[])))
                        .with_tooltip("vigil: waiting for status...")
                        .build()
                        .expect("failed to create tray icon"),
                );
            }
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if menu_event.id == "quit" {
                    *control_flow = ControlFlow::Exit;
                } else if let Some(path) = incident_paths.iter().find(|p| p.to_string_lossy() == menu_event.id.0) {
                    let _ = std::process::Command::new("open").arg(path).spawn();
                }
            }
            _ => {}
        }

        if last_poll.elapsed() >= opts.poll_interval {
            last_poll = Instant::now();

            let status = read_status(Path::new(&opts.status_file));
            let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
            let level = classify_health(status.as_ref(), now_unix, stale_after_secs);

            if Some(level) != last_level {
                last_level = Some(level);
                if let Some(tray) = &tray {
                    let (rgba, w, h) = icon_rgba(level);
                    if let Ok(icon) = Icon::from_rgba(rgba, w, h) {
                        let _ = tray.set_icon(Some(icon));
                    }
                    let _ = tray.set_tooltip(Some(match level {
                        HealthLevel::Ok => "vigil: all clear",
                        HealthLevel::Warning => "vigil: 1 open incident",
                        HealthLevel::Critical => "vigil: multiple open incidents",
                        HealthLevel::Unknown => "vigil: watch not reporting",
                    }));
                }
            }

            incident_paths = crate::incidents::list(Path::new(&opts.incidents_dir)).unwrap_or_default();
            incident_paths.reverse(); // most recent first
            incident_paths.truncate(8);
            if let Some(tray) = &tray {
                tray.set_menu(Some(Box::new(build_menu(&incident_paths))));
            }
        }
    });
}

fn build_menu(incidents: &[PathBuf]) -> Menu {
    let menu = Menu::new();
    if incidents.is_empty() {
        let _ = menu.append(&MenuItem::new("No incidents yet", false, None));
    } else {
        for path in incidents {
            let title = std::fs::read_to_string(path)
                .ok()
                .map(|c| crate::incidents::extract_title(&c).to_string())
                .unwrap_or_else(|| "(unreadable)".to_string());
            let _ = menu.append(&MenuItem::with_id(path.to_string_lossy().to_string(), title, true, None));
        }
    }
    let _ = menu.append(&PredefinedMenuItem::separator());
    let _ = menu.append(&MenuItem::with_id("quit", "Quit vigil menubar", true, None));
    menu
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_status_json_round_trips_through_status_file() {
        let json = build_status_json(3, 1_000_000);
        let parsed: StatusFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.open_count, 3);
        assert_eq!(parsed.updated_unix, 1_000_000);
    }

    #[test]
    fn classify_health_is_unknown_without_a_status_file() {
        assert_eq!(classify_health(None, 1000, 30), HealthLevel::Unknown);
    }

    #[test]
    fn classify_health_is_unknown_when_status_is_stale() {
        let status = StatusFile { updated_unix: 1000, open_count: 0 };
        assert_eq!(classify_health(Some(&status), 1000 + 31, 30), HealthLevel::Unknown);
    }

    #[test]
    fn classify_health_ok_at_zero_open_incidents() {
        let status = StatusFile { updated_unix: 1000, open_count: 0 };
        assert_eq!(classify_health(Some(&status), 1005, 30), HealthLevel::Ok);
    }

    #[test]
    fn classify_health_warning_at_one_open_incident() {
        let status = StatusFile { updated_unix: 1000, open_count: 1 };
        assert_eq!(classify_health(Some(&status), 1005, 30), HealthLevel::Warning);
    }

    #[test]
    fn classify_health_critical_at_multiple_open_incidents() {
        let status = StatusFile { updated_unix: 1000, open_count: 2 };
        assert_eq!(classify_health(Some(&status), 1005, 30), HealthLevel::Critical);

        let many = StatusFile { updated_unix: 1000, open_count: 7 };
        assert_eq!(classify_health(Some(&many), 1005, 30), HealthLevel::Critical);
    }

    #[test]
    fn icon_rgba_has_correct_dimensions_and_is_fully_opaque_at_center_for_warning_and_critical() {
        for level in [HealthLevel::Warning, HealthLevel::Critical] {
            let (buf, w, h) = icon_rgba(level);
            assert_eq!(w, ICON_SIZE);
            assert_eq!(h, ICON_SIZE);
            assert_eq!(buf.len(), (w * h * 4) as usize);
            let center_idx = ((h / 2 * w + w / 2) * 4) as usize;
            assert_eq!(buf[center_idx + 3], 255, "{level:?} should be fully opaque at its center");
        }
    }

    #[test]
    fn icon_rgba_unknown_state_is_visible_but_distinct_from_ok() {
        let (unknown_buf, w, h) = icon_rgba(HealthLevel::Unknown);
        let (ok_buf, _, _) = icon_rgba(HealthLevel::Ok);
        let center_idx = ((h / 2 * w + w / 2) * 4) as usize;
        assert!(unknown_buf[center_idx + 3] > ok_buf[center_idx + 3], "Unknown should be more visible than Ok");
    }

    #[test]
    fn icon_rgba_ok_state_is_mostly_transparent() {
        let (buf, w, h) = icon_rgba(HealthLevel::Ok);
        let center_idx = ((h / 2 * w + w / 2) * 4) as usize;
        assert!(buf[center_idx + 3] < 100, "Ok state should read as faint/unobtrusive");
    }

    #[test]
    fn icon_rgba_corners_are_transparent_outside_the_circle() {
        let (buf, w, _h) = icon_rgba(HealthLevel::Critical);
        // Top-left corner pixel is outside any circle inscribed in the icon.
        assert_eq!(buf[3], 0);
        let _ = w;
    }

    #[test]
    fn default_status_file_is_under_home() {
        assert!(default_status_file().ends_with(".vigil/status.json"));
    }
}
