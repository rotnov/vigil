use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Sparkline, Table, Wrap};
use ratatui::Frame;
use std::collections::VecDeque;
use std::time::Duration;
use sysinfo::System;

const HISTORY_LEN: usize = 120;
pub(crate) const ALERT_CHECK_INTERVAL: Duration = Duration::from_secs(10);
const ALERT_LOG_LEN: usize = 5;

pub struct UiOptions {
    pub interval: Duration,
    pub top_n: usize,
    pub notify: bool,
    pub cooldown: Duration,
    pub agent_dir: String,
    pub incidents_dir: String,
}

pub(crate) struct History {
    cpu: VecDeque<u64>,
    mem: VecDeque<u64>,
}

impl History {
    pub(crate) fn new() -> Self {
        Self {
            cpu: VecDeque::with_capacity(HISTORY_LEN),
            mem: VecDeque::with_capacity(HISTORY_LEN),
        }
    }

    pub(crate) fn push(&mut self, cpu_pct: f32, mem_pct: f32) {
        push_capped(&mut self.cpu, cpu_pct.round() as u64, HISTORY_LEN);
        push_capped(&mut self.mem, mem_pct.round() as u64, HISTORY_LEN);
    }
}

fn push_capped(buf: &mut VecDeque<u64>, v: u64, cap: usize) {
    if buf.len() == cap {
        buf.pop_front();
    }
    buf.push_back(v);
}

const PROC_TREND_LEN: usize = 10;
/// A process's memory needs to move by more than this, oldest-to-newest
/// sample in its tracked window, to read as a real trend rather than
/// ordinary sample-to-sample noise.
const PROC_TREND_THRESHOLD: f64 = 0.10;

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
enum Trend {
    Up,
    Down,
    Stable,
}

impl Trend {
    fn arrow(self) -> &'static str {
        match self {
            Trend::Up => "↑",
            Trend::Down => "↓",
            Trend::Stable => "→",
        }
    }
}

/// Per-process memory history, tracked across ticks — answers "is this
/// process growing" at a glance, without needing to ask the agent. A
/// process can rank at the top of `top_mem` on a single snapshot without
/// that telling you whether it just spiked or has been climbing for an
/// hour; this is the same distinction `agent::build_diagnosis_question`'s
/// watch-log pointer exists to make for the agent's own answers.
pub(crate) struct ProcTrends {
    history: std::collections::HashMap<u32, VecDeque<u64>>,
}

impl ProcTrends {
    pub(crate) fn new() -> Self {
        Self { history: std::collections::HashMap::new() }
    }

    pub(crate) fn record(&mut self, sys: &System) {
        let seen: std::collections::HashSet<u32> = sys.processes().keys().map(|p| p.as_u32()).collect();
        self.history.retain(|pid, _| seen.contains(pid));
        for (pid, p) in sys.processes() {
            let buf = self.history.entry(pid.as_u32()).or_insert_with(|| VecDeque::with_capacity(PROC_TREND_LEN));
            push_capped(buf, p.memory(), PROC_TREND_LEN);
        }
    }

    /// `None` until enough samples have accumulated for `pid` to say
    /// anything (a just-started or just-noticed process).
    fn mem_trend(&self, pid: u32) -> Option<Trend> {
        let buf = self.history.get(&pid)?;
        if buf.len() < 2 {
            return None;
        }
        Some(classify_trend(*buf.front().unwrap(), *buf.back().unwrap()))
    }
}

/// Pure comparison, split out of `ProcTrends::mem_trend` so it's testable
/// without a real `sysinfo::System` (which can't be handed fabricated
/// per-PID memory values in a unit test).
fn classify_trend(oldest: u64, newest: u64) -> Trend {
    if oldest == 0 {
        return Trend::Stable;
    }
    let change = (newest as f64 - oldest as f64) / oldest as f64;
    if change > PROC_TREND_THRESHOLD {
        Trend::Up
    } else if change < -PROC_TREND_THRESHOLD {
        Trend::Down
    } else {
        Trend::Stable
    }
}


/// Snapshot of interactive state needed to render a frame. Kept separate
/// from the event loop so `draw` stays a pure function of (system, history,
/// app) — that's what makes it testable with `TestBackend`.
pub(crate) struct AppState {
    top_n: usize,
    pub(crate) input_mode: bool,
    pub(crate) input_buffer: String,
    pub(crate) thinking: bool,
    pub(crate) answer: Option<Result<String, String>>,
    alert_log: VecDeque<String>,
    /// Only refreshed on the slower alert-check cadence (battery info needs
    /// a `pmset` shell-out) — `None` until the first such refresh happens.
    pub(crate) battery_pct: Option<u8>,
    pub(crate) battery_charging: Option<bool>,
    pub(crate) battery_eta_secs: Option<u64>,
}

impl AppState {
    pub(crate) fn new(top_n: usize) -> Self {
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

    pub(crate) fn push_alert(&mut self, line: String) {
        if self.alert_log.len() == ALERT_LOG_LEN {
            self.alert_log.pop_back();
        }
        self.alert_log.push_front(line);
    }
}

/// The pre-filled question the `w` key sends about whichever process
/// currently ranks #1 by CPU — pure so the exact wording is unit-tested
/// without going through a real key event / System.
pub(crate) fn why_question(name: &str, pid: u32, cpu_pct: f32, mem_bytes: u64) -> String {
    format!(
        "Why is {name} (pid {pid}) using {cpu_pct:.0}% CPU and {:.0} MB memory right now?",
        mem_bytes as f64 / 1e6
    )
}

/// Coverage exemption (see AGENTS.md's testing section): unlike
/// `collect_disks`'s equivalent guard, `total_memory() == 0` isn't
/// structurally ruled out by anything upstream (no filter guarantees it) --
/// it's a real defensive check against a division by zero, just one that
/// never actually happens on real Mac hardware, so a unit test can't reach
/// the `else` branch without a fake `System` sysinfo doesn't offer.
pub(crate) fn mem_percent(sys: &System) -> f32 {
    if sys.total_memory() > 0 {
        sys.used_memory() as f32 / sys.total_memory() as f32 * 100.0
    } else {
        0.0
    }
}

pub(crate) fn draw(f: &mut Frame, sys: &System, history: &History, trends: &ProcTrends, app: &AppState) {
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
    draw_process_table(f, root[2], sys, trends, app.top_n);
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

fn draw_process_table(f: &mut Frame, area: Rect, sys: &System, trends: &ProcTrends, top_n: usize) {
    let mut procs: Vec<&sysinfo::Process> = sys.processes().values().collect();
    procs.sort_by(|a, b| b.cpu_usage().partial_cmp(&a.cpu_usage()).unwrap());

    let rows: Vec<Row> = procs
        .iter()
        .take(top_n)
        .map(|p| {
            let mem_mb = p.memory() as f64 / 1e6;
            // Whether this process has been growing/shrinking, not just
            // what it's using right now — see ProcTrends's own doc comment
            // for why. Blank (not "→") until enough samples exist to say.
            let arrow = trends.mem_trend(p.pid().as_u32()).map(Trend::arrow).unwrap_or(" ");
            Row::new(vec![
                Cell::from(p.pid().to_string()),
                Cell::from(p.name().to_string_lossy().to_string()),
                Cell::from(format!("{:>6.1}%", p.cpu_usage())),
                Cell::from(format!("{mem_mb:>8.1} MB {arrow}")),
            ])
        })
        .collect();

    let widths = [
        Constraint::Length(8),
        Constraint::Min(20),
        Constraint::Length(8),
        Constraint::Length(14),
    ];

    let table = Table::new(rows, widths)
        .header(
            Row::new(vec!["PID", "NAME", "CPU", "MEM"])
                .style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .block(Block::default().borders(Borders::ALL).title(" top processes (by CPU) — mem trend over last 10 samples "));

    f.render_widget(table, area);
}

fn draw_footer(f: &mut Frame, area: Rect, app: &AppState) {
    let hint = " q/esc: quit   a: ask agent   w: why is the top process using so much ";
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
    fn classify_trend_stable_within_threshold() {
        assert_eq!(classify_trend(1000, 1050), Trend::Stable); // +5%
        assert_eq!(classify_trend(1000, 950), Trend::Stable); // -5%
    }

    #[test]
    fn classify_trend_up_beyond_threshold() {
        assert_eq!(classify_trend(1000, 1200), Trend::Up); // +20%
    }

    #[test]
    fn classify_trend_down_beyond_threshold() {
        assert_eq!(classify_trend(1000, 800), Trend::Down); // -20%
    }

    #[test]
    fn classify_trend_treats_zero_oldest_as_stable() {
        // Can't compute a meaningful percent change from zero -- avoid a
        // division-by-zero producing a spurious "Up" for a process that
        // simply wasn't tracked before this sample.
        assert_eq!(classify_trend(0, 500), Trend::Stable);
    }

    #[test]
    fn mem_trend_is_none_with_fewer_than_two_samples() {
        let mut trends = ProcTrends::new();
        trends.history.insert(42, VecDeque::from([1000]));
        assert_eq!(trends.mem_trend(42), None);
    }

    #[test]
    fn mem_trend_is_none_for_an_unknown_pid() {
        let trends = ProcTrends::new();
        assert_eq!(trends.mem_trend(999), None);
    }

    #[test]
    fn mem_trend_compares_oldest_to_newest_in_the_window() {
        let mut trends = ProcTrends::new();
        // Dips in the middle don't matter -- only the window's endpoints.
        trends.history.insert(7, VecDeque::from([1000, 200, 1300]));
        assert_eq!(trends.mem_trend(7), Some(Trend::Up));
    }

    #[test]
    fn mem_percent_reports_a_plausible_percentage_on_this_machine() {
        let sys = System::new_all();
        let pct = mem_percent(&sys);
        assert!((0.0..=100.0).contains(&pct));
    }

    #[test]
    fn why_question_names_the_process_and_its_current_usage() {
        let q = why_question("pycharm", 37489, 176.3, 17_671_000_000);
        assert!(q.contains("pycharm"));
        assert!(q.contains("37489"));
        assert!(q.contains("176%"));
        assert!(q.contains("17671 MB"));
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
        let trends = ProcTrends::new();
        let mut history = History::new();
        history.push(42.0, 55.0);
        let app = AppState::new(5);

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

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
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.battery_pct = Some(37);
        app.battery_charging = Some(false);
        app.battery_eta_secs = Some(42 * 60);

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

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
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.battery_pct = Some(80);
        app.battery_charging = Some(true);

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

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
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.input_mode = true;
        app.input_buffer = "why is disk space low?".to_string();

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("why is disk space low?"));
        assert!(text.contains("to send"));
    }

    #[test]
    fn answer_popup_shows_agent_response() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.answer = Some(Ok("Disk is almost full because of Docker".to_string()));

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Docker"));
        assert!(text.contains("agent answer"));
    }

    #[test]
    fn answer_popup_shows_error_state() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.answer = Some(Err("uv not found".to_string()));

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("uv not found"));
        assert!(text.contains("agent error"));
    }

    #[test]
    fn footer_surfaces_most_recent_alert() {
        let backend = TestBackend::new(120, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.push_alert("[low_disk:/] low disk space".to_string());

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

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

    #[test]
    fn trend_arrow_renders_each_direction_distinctly() {
        assert_eq!(Trend::Up.arrow(), "↑");
        assert_eq!(Trend::Down.arrow(), "↓");
        assert_eq!(Trend::Stable.arrow(), "→");
    }

    #[test]
    fn proc_trends_record_tracks_real_processes_and_forgets_dead_ones() {
        let mut sys = System::new_all();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut trends = ProcTrends::new();
        trends.record(&sys);
        trends.record(&sys);

        // Every currently-live pid should now have at least 2 samples tracked.
        for pid in sys.processes().keys().take(5) {
            assert!(trends.history.get(&pid.as_u32()).is_some_and(|h| h.len() >= 2));
        }

        // A pid that has since disappeared is pruned on the next record().
        trends.history.insert(u32::MAX, VecDeque::from([1, 2]));
        trends.record(&sys);
        assert!(!trends.history.contains_key(&u32::MAX));
    }

    #[test]
    fn draw_thinking_popup_shows_while_awaiting_the_agent() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let sys = System::new_all();
        let trends = ProcTrends::new();
        let history = History::new();
        let mut app = AppState::new(5);
        app.thinking = true;

        terminal.draw(|f| draw(f, &sys, &history, &trends, &app)).unwrap();

        let text = buffer_text(terminal.backend().buffer());
        assert!(text.contains("Asking the agent"));
        assert!(text.contains("vigil-agent"));
    }

    #[test]
    fn battery_summary_shows_draining_without_an_eta_yet() {
        let app = AppState { battery_pct: Some(55), battery_charging: Some(false), battery_eta_secs: None, ..AppState::new(5) };
        let summary = battery_summary(&app);
        assert!(summary.contains("55%"));
        assert!(summary.contains("draining"));
        assert!(!summary.contains("left"), "no ETA yet, so no '~N left' should appear");
    }

    #[test]
    fn battery_summary_shows_percentage_with_unknown_charging_state() {
        let app = AppState { battery_pct: Some(64), battery_charging: None, ..AppState::new(5) };
        let summary = battery_summary(&app);
        assert_eq!(summary, "battery 64%");
    }
}
