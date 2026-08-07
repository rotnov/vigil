use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};
use sysinfo::System;

use crate::alerts::AlertState;

const HISTORY_LEN: usize = 120;
const ALERT_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const ALERT_LOG_LEN: usize = 5;

pub struct UiOptions {
    pub interval: Duration,
    pub top_n: usize,
    pub notify: bool,
    pub cooldown: Duration,
    pub agent_dir: String,
    pub incidents_dir: String,
}

struct History {
    cpu: VecDeque<u64>,
    mem: VecDeque<u64>,
}

impl History {
    fn new() -> Self {
        Self {
            cpu: VecDeque::with_capacity(HISTORY_LEN),
            mem: VecDeque::with_capacity(HISTORY_LEN),
        }
    }

    fn push(&mut self, cpu_pct: f32, mem_pct: f32) {
        push_capped(&mut self.cpu, cpu_pct.round() as u64);
        push_capped(&mut self.mem, mem_pct.round() as u64);
    }
}

fn push_capped(buf: &mut VecDeque<u64>, v: u64) {
    if buf.len() == HISTORY_LEN {
        buf.pop_front();
    }
    buf.push_back(v);
}

/// Snapshot of interactive state needed to render a frame. Kept separate
/// from the event loop so `draw` stays a pure function of (system, history,
/// app) — that's what makes it testable with `TestBackend`.
struct AppState {
    top_n: usize,
    input_mode: bool,
    input_buffer: String,
    thinking: bool,
    answer: Option<Result<String, String>>,
    alert_log: VecDeque<String>,
    /// Only refreshed on the slower alert-check cadence (battery info needs
    /// a `pmset` shell-out) — `None` until the first such refresh happens.
    battery_pct: Option<u8>,
    battery_charging: Option<bool>,
    battery_eta_secs: Option<u64>,
}

impl AppState {
    fn new(top_n: usize) -> Self {
        Self {
            top_n,
            input_mode: false,
            input_buffer: String::new(),
            thinking: false,
            answer: None,
            alert_log: VecDeque::with_capacity(ALERT_LOG_LEN),
            battery_pct: None,
            battery_charging: None,
            battery_eta_secs: None,
        }
    }

    fn push_alert(&mut self, line: String) {
        if self.alert_log.len() == ALERT_LOG_LEN {
            self.alert_log.pop_back();
        }
        self.alert_log.push_front(line);
    }
}

pub fn run(opts: UiOptions) -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let mut sys = System::new_all();
    let cpu_count = sys.cpus().len().max(1);
    let mut history = History::new();
    let mut app = AppState::new(opts.top_n);
    let mut alert_state = AlertState::new();
    let mut recent_alerts = crate::alerts::RecentAlerts::new();
    let mut battery_trend = crate::battery::BatteryTrend::new();
    let mut diagnosis_coalescer = crate::agent::DiagnosisCoalescer::new();

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
                let snapshot_json = serde_json::to_string(&snap).unwrap_or_default();

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
                for alert in &fired {
                    recent_alerts.record(&alert.key, &alert.message, now);
                }
                for alert in fired {
                    app.push_alert(format!("[{}] {}", alert.key, alert.message));
                    crate::alerts::notify(&alert);
                    let context = recent_alerts.context_excluding(&alert.key, now);
                    crate::agent::maybe_diagnose_alert_async(
                        &alert,
                        &snapshot_json,
                        &opts.agent_dir,
                        &opts.incidents_dir,
                        context.as_deref(),
                        &mut diagnosis_coalescer,
                        now,
                    );
                }
                (sys.global_cpu_usage(), mem_percent(&sys))
            } else {
                sys.refresh_cpu_usage();
                sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
                sys.refresh_memory();
                (sys.global_cpu_usage(), mem_percent(&sys))
            };
            history.push(cpu_pct, mem_pct);
        }

        terminal.draw(|f| draw(f, &sys, &history, &app))?;

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
                                terminal.draw(|f| draw(f, &sys, &history, &app))?;

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

fn mem_percent(sys: &System) -> f32 {
    if sys.total_memory() > 0 {
        sys.used_memory() as f32 / sys.total_memory() as f32 * 100.0
    } else {
        0.0
    }
}

fn draw(f: &mut Frame, sys: &System, history: &History, app: &AppState) {
    let root = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(4),
            Constraint::Length(7),
            Constraint::Min(6),
            Constraint::Length(1),
        ])
        .split(f.area());

    draw_header(f, root[0], sys, app);
    draw_sparklines(f, root[1], history);
    draw_process_table(f, root[2], sys, app.top_n);
    draw_footer(f, root[3], app);

    if app.input_mode {
        draw_input_popup(f, &app.input_buffer);
    } else if app.thinking {
        draw_thinking_popup(f);
    } else if let Some(answer) = &app.answer {
        draw_answer_popup(f, answer);
    }
}

fn draw_header(f: &mut Frame, area: Rect, sys: &System, app: &AppState) {
    let load = System::load_average();
    let mem_used_gb = sys.used_memory() as f64 / 1e9;
    let mem_total_gb = sys.total_memory() as f64 / 1e9;
    let swap_used_gb = sys.used_swap() as f64 / 1e9;

    let text = Line::from(vec![
        Span::styled(" vigil ", Style::default().fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "  load {:.1} {:.1} {:.1}   mem {:.1}/{:.1} GB   swap {:.1} GB   cpu {:.0}%   {}",
            load.one, load.five, load.fifteen, mem_used_gb, mem_total_gb, swap_used_gb,
            sys.global_cpu_usage(),
            battery_summary(app)
        )),
    ]);

    let block = Block::default().borders(Borders::ALL).title(" system ");
    // Wrap instead of the default clip: on a narrow terminal this line can
    // exceed the width, and silently truncating (losing e.g. the battery
    // ETA at the tail) is worse than spilling onto a second line.
    f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(block), area);
}

fn battery_summary(app: &AppState) -> String {
    let Some(pct) = app.battery_pct else {
        return "battery n/a".to_string();
    };
    match app.battery_charging {
        Some(true) => format!("battery {pct}% (charging)"),
        Some(false) => match app.battery_eta_secs {
            Some(secs) => format!("battery {pct}% (draining, ~{} left)", crate::battery::format_eta(Duration::from_secs(secs))),
            None => format!("battery {pct}% (draining)"),
        },
        None => format!("battery {pct}%"),
    }
}

fn draw_sparklines(f: &mut Frame, area: Rect, history: &History) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let cpu_data: Vec<u64> = history.cpu.iter().copied().collect();
    let cpu_last = cpu_data.last().copied().unwrap_or(0);
    let cpu_spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(format!(" CPU {cpu_last}% ")))
        .data(&cpu_data)
        .max(100)
        .style(Style::default().fg(Color::Green));
    f.render_widget(cpu_spark, cols[0]);

    let mem_data: Vec<u64> = history.mem.iter().copied().collect();
    let mem_last = mem_data.last().copied().unwrap_or(0);
    let mem_spark = Sparkline::default()
        .block(Block::default().borders(Borders::ALL).title(format!(" MEM {mem_last}% ")))
        .data(&mem_data)
        .max(100)
        .style(Style::default().fg(Color::Magenta));
    f.render_widget(mem_spark, cols[1]);
}

fn draw_process_table(f: &mut Frame, area: Rect, sys: &System, top_n: usize) {
    let mut procs: Vec<&sysinfo::Process> = sys.processes().values().collect();
    procs.sort_by(|a, b| b.cpu_usage().partial_cmp(&a.cpu_usage()).unwrap());

    let rows: Vec<Row> = procs
        .iter()
        .take(top_n)
        .map(|p| {
            let mem_mb = p.memory() as f64 / 1e6;
            Row::new(vec![
                Cell::from(p.pid().to_string()),
                Cell::from(p.name().to_string_lossy().to_string()),
                Cell::from(format!("{:>6.1}%", p.cpu_usage())),
                Cell::from(format!("{mem_mb:>8.1} MB")),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(12),
    ];

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["PID", "NAME", "CPU", "MEM"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" top processes (by CPU) "));

    f.render_widget(table, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &AppState) {
    let hint = " q/esc: quit   a: ask agent ";
    let text = if let Some(recent) = app.alert_log.front() {
        Line::from(vec![
            Span::styled(hint, Style::default().fg(Color::DarkGray)),
            Span::styled(format!("   ⚠ {recent}"), Style::default().fg(Color::Yellow)),
        ])
    } else {
        Line::from(vec![Span::styled(hint, Style::default().fg(Color::DarkGray))])
    };
    f.render_widget(Paragraph::new(text), area);
}

fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn draw_input_popup(f: &mut Frame, buffer: &str) {
    let area = centered_rect(70, 15, f.area());
    f.render_widget(Clear, area);
    let text = Paragraph::new(format!("{buffer}_"))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" ask agent (Enter to send, Esc to cancel) ")
                .style(Style::default().fg(Color::Cyan)),
        );
    f.render_widget(text, area);
}

fn draw_thinking_popup(f: &mut Frame) {
    let area = centered_rect(40, 15, f.area());
    f.render_widget(Clear, area);
    let text = Paragraph::new("Asking the agent...").block(
        Block::default()
            .borders(Borders::ALL)
            .title(" vigil-agent ")
            .style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(text, area);
}

fn draw_answer_popup(f: &mut Frame, answer: &Result<String, String>) {
    let area = centered_rect(80, 60, f.area());
    f.render_widget(Clear, area);
    let (title, style, body) = match answer {
        Ok(text) => (" agent answer (Esc/Enter to close) ", Style::default().fg(Color::Green), text.as_str()),
        Err(err) => (" agent error (Esc/Enter to close) ", Style::default().fg(Color::Red), err.as_str()),
    };
    let text = Paragraph::new(body)
        .wrap(Wrap { trim: false })
        .block(Block::default().borders(Borders::ALL).title(title).style(style));
    f.render_widget(text, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::Terminal;

    fn buffer_text(buf: &Buffer) -> String {
        buf.content.iter().map(|c| c.symbol()).collect()
    }

    #[test]
    fn history_caps_at_max_length_and_keeps_most_recent() {
        let mut h = History::new();
        for i in 0..(HISTORY_LEN as u64 + 10) {
            h.push(i as f32, i as f32);
        }
        assert_eq!(h.cpu.len(), HISTORY_LEN);
        assert_eq!(h.mem.len(), HISTORY_LEN);
        assert_eq!(*h.cpu.back().unwrap(), HISTORY_LEN as u64 + 9);
    }

    #[test]
    fn draw_renders_header_sparklines_table_and_footer() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let mut history = History::new();
        history.push(42.0, 55.0);
        let app = AppState::new(5);

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("system"));
        assert!(text.contains("PID"));
        assert!(text.contains("NAME"));
        assert!(text.contains("CPU"));
        assert!(text.contains("MEM"));
        assert!(text.contains("quit"));
        assert!(text.contains("ask agent"));
        assert!(text.contains("battery n/a"), "no battery sample fetched yet");
    }

    #[test]
    fn header_shows_draining_battery_with_eta() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let history = History::new();
        let mut app = AppState::new(5);
        app.battery_pct = Some(37);
        app.battery_charging = Some(false);
        app.battery_eta_secs = Some(42 * 60);

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("37%"));
        assert!(text.contains("draining"));
        assert!(text.contains("42m"));
    }

    #[test]
    fn header_shows_charging_battery() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let history = History::new();
        let mut app = AppState::new(5);
        app.battery_pct = Some(80);
        app.battery_charging = Some(true);

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("80%"));
        assert!(text.contains("charging"));
    }

    #[test]
    fn sparkline_titles_show_latest_value() {
        let backend = TestBackend::new(80, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut history = History::new();
        history.push(10.0, 20.0);
        history.push(33.0, 77.0);

        terminal
            .draw(|f| {
                let area = f.area();
                draw_sparklines(f, area, &history);
            })
            .unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("33"));
        assert!(text.contains("77"));
    }

    #[test]
    fn input_popup_shows_typed_question() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let history = History::new();
        let mut app = AppState::new(5);
        app.input_mode = true;
        app.input_buffer = "why is disk space low?".to_string();

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("why is disk space low?"));
        assert!(text.contains("to send"));
    }

    #[test]
    fn answer_popup_shows_agent_response() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let history = History::new();
        let mut app = AppState::new(5);
        app.answer = Some(Ok("Disk is almost full because of Docker".to_string()));

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Docker"));
        assert!(text.contains("agent answer"));
    }

    #[test]
    fn answer_popup_shows_error_state() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let history = History::new();
        let mut app = AppState::new(5);
        app.answer = Some(Err("uv not found".to_string()));

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("uv not found"));
        assert!(text.contains("agent error"));
    }

    #[test]
    fn footer_surfaces_most_recent_alert() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let history = History::new();
        let mut app = AppState::new(5);
        app.push_alert("[low_disk:/] low disk space".to_string());

        terminal.draw(|f| draw(f, &sys, &history, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("low disk space"));
    }

    #[test]
    fn alert_log_caps_at_configured_length() {
        let mut app = AppState::new(5);
        for i in 0..(ALERT_LOG_LEN + 3) {
            app.push_alert(format!("alert {i}"));
        }
        assert_eq!(app.alert_log.len(), ALERT_LOG_LEN);
        assert_eq!(app.alert_log.front().unwrap(), &format!("alert {}", ALERT_LOG_LEN + 2));
    }
}
