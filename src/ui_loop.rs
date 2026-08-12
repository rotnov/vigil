//! The real terminal event loop behind `vigil ui`.
//!
//! Excluded from the coverage gate (see AGENTS.md's testing section and the
//! `--ignore-filename-regex` in the test command): this needs a real
//! terminal in raw mode and real keyboard events, and only returns when the
//! user presses `q`/Esc — none of that is practically driven from a unit
//! test without a pty-emulation dependency this project's size doesn't
//! warrant. Every piece of logic inside it (alert evaluation, incident
//! tracking, trend classification, the frame renderer, the pre-filled `w`
//! question) is defined and unit-tested elsewhere (`ui.rs`, `alerts.rs`,
//! `battery.rs`) — this is just the glue that wires them to real key events
//! on a timer.

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use std::io;
use std::time::{Duration, Instant};
use sysinfo::System;

use crate::alerts::AlertState;
use crate::ui::{draw, mem_percent, why_question, AppState, History, ProcTrends, UiOptions, ALERT_CHECK_INTERVAL};

pub fn run(opts: UiOptions) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let mut sys = System::new_all();
    let cpu_count = sys.cpus().len().max(1);
    let mut history = History::new();
    let mut trends = ProcTrends::new();
    let mut app = AppState::new(opts.top_n);
    let mut alert_state = AlertState::new();
    let mut battery_trend = crate::battery::BatteryTrend::new();
    let mut incident_tracker = crate::alerts::IncidentTracker::new();
    // See main.rs's Watch loop for the rationale (2x cooldown).
    let incident_timeout = opts.cooldown * 2;

    let mut last_tick = Instant::now() - opts.interval; // force immediate first sample
    let mut last_alert_check = Instant::now() - ALERT_CHECK_INTERVAL;

    let result = loop {
        if last_tick.elapsed() >= opts.interval {
            last_tick = Instant::now();

            let due_for_alerts = opts.notify && last_alert_check.elapsed() >= ALERT_CHECK_INTERVAL;
            let (cpu_pct, mem_pct) = if due_for_alerts {
                let now = Instant::now();
                last_alert_check = now;
                let snap = crate::take_snapshot(&mut sys, opts.top_n);

                battery_trend.record(
                    snap.battery.as_ref().and_then(|b| b.charging),
                    snap.battery.as_ref().and_then(|b| b.percentage),
                    now,
                );
                let battery_eta = battery_trend.eta();
                app.battery_pct = snap.battery.as_ref().and_then(|b| b.percentage);
                app.battery_charging = snap.battery.as_ref().and_then(|b| b.charging);
                app.battery_eta_secs = battery_eta.map(|d| d.as_secs());

                let mut fired = crate::alerts::evaluate(&snap, cpu_count, &mut alert_state, opts.cooldown, now);
                fired.extend(crate::alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, opts.cooldown, now));
                for alert in fired {
                    app.push_alert(format!("[{}] {}", alert.key, alert.message));
                    if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                        if crate::agent::is_journal_worthy(&alert.key) {
                            let incidents_dir = std::path::Path::new(&opts.incidents_dir);
                            let stub = crate::incidents::IncidentStub {
                                alert_key: &alert.key,
                                alert_title: &alert.title,
                                alert_message: &alert.message,
                                command: alert.command.as_deref(),
                            };
                            // Same reasoning as watch.rs: vigil-ui owns
                            // notifying journal-worthy alerts now.
                            if let Err(e) = crate::incidents::write_stub(incidents_dir, &stub) {
                                app.push_alert(format!("[vigil] failed to write incident stub: {e}"));
                                crate::alerts::notify(&alert);
                            }
                        } else {
                            crate::alerts::notify(&alert);
                        }
                    }
                }
                (sys.global_cpu_usage(), mem_percent(&sys))
            } else {
                sys.refresh_cpu_usage();
                sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                sys.refresh_memory();
                (sys.global_cpu_usage(), mem_percent(&sys))
            };
            history.push(cpu_pct, mem_pct);
            trends.record(&sys);
        }

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app))?;

        let timeout = opts
            .interval
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::from_millis(50))
            .min(Duration::from_millis(250));

        if event::poll(timeout)? {
            if let Event::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }

                if app.input_mode {
                    match key.code {
                        KeyCode::Enter => {
                            let question = app.input_buffer.trim().to_string();
                            app.input_mode = false;
                            app.input_buffer.clear();
                            if !question.is_empty() {
                                app.thinking = true;
                                app.answer = None;
                                terminal.draw(|f| draw(f, &sys, &history, &trends, &app))?;

                                let snap = crate::take_snapshot(&mut sys, opts.top_n);
                                let snapshot_json = serde_json::to_string(&snap).unwrap_or_default();
                                let result = crate::agent::ask(&question, &snapshot_json, &opts.agent_dir);
                                app.answer = Some(result);
                                app.thinking = false;
                            }
                        }
                        KeyCode::Esc => {
                            app.input_mode = false;
                            app.input_buffer.clear();
                        }
                        KeyCode::Backspace => {
                            app.input_buffer.pop();
                        }
                        KeyCode::Char(c) => {
                            app.input_buffer.push(c);
                        }
                        _ => {}
                    }
                } else if app.answer.is_some() {
                    match key.code {
                        KeyCode::Esc | KeyCode::Enter | KeyCode::Char('a') => app.answer = None,
                        KeyCode::Char('q') => break Ok(()),
                        _ => {}
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break Ok(()),
                        KeyCode::Char('a') => {
                            app.input_mode = true;
                            app.input_buffer.clear();
                        }
                        KeyCode::Char('w') => {
                            let top = sys
                                .processes()
                                .values()
                                .max_by(|a, b| a.cpu_usage().partial_cmp(&b.cpu_usage()).unwrap());
                            if let Some(p) = top {
                                let question =
                                    why_question(&p.name().to_string_lossy(), p.pid().as_u32(), p.cpu_usage(), p.memory());
                                app.thinking = true;
                                app.answer = None;
                                terminal.draw(|f| draw(f, &sys, &history, &trends, &app))?;

                                let snap = crate::take_snapshot(&mut sys, opts.top_n);
                                let snapshot_json = serde_json::to_string(&snap).unwrap_or_default();
                                let result = crate::agent::ask(&question, &snapshot_json, &opts.agent_dir);
                                app.answer = Some(result);
                                app.thinking = false;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    };

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}
