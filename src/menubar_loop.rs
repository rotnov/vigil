//! The real macOS menu bar event loop behind `vigil menubar`.
//!
//! Excluded from the coverage gate (see AGENTS.md's testing section and the
//! `--ignore-filename-regex` in the test command): `tao`'s `event_loop.run`
//! hands control to the OS run loop and never returns during normal
//! operation, and everything inside reacts to real `NSStatusItem`
//! click/menu events a unit test has no practical way to synthesize. The
//! actual decision logic (health classification, icon rendering, menu
//! construction) is defined and unit-tested in `menubar.rs`; this is just
//! the glue that wires it to the real tray icon on a timer.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::menubar::{classify_health, icon_rgba, read_status, HealthLevel, MenubarOptions};

/// Builds the tray's dropdown menu. Lives here rather than in `menubar.rs`
/// alongside the rest of the pure(r) logic because `muda::Menu::new` panics
/// if it isn't called on the main thread — a hard AppKit constraint that
/// makes this genuinely untestable from `cargo test`'s worker threads, the
/// same category as the event loop it's only ever called from.
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

enum UserEvent {
    /// Only used to wake the event loop early on a tray click — macOS
    /// already opens the attached menu on its own, so the event's own
    /// payload is never inspected.
    Tray,
    Menu(MenuEvent),
}

/// Blocks for the process's lifetime, same as `ui_loop::run` — launched as
/// its own long-running process alongside `vigil watch`.
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
