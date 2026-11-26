//! Carrels, the panel view: the conversation beside tiles that show what calx
//! is doing, such as the tool log, diffs, context use, and calx itself.
//! Named layouts swap the tiles. Below [`WIDE`] columns it becomes the
//! Reading room: the conversation and one side panel, switched with Tab.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::{Duration, Instant};

use anyhow::Result;
use calx_core::agent::{Agent, Approval, DECLINED, MAX_STEPS};
use calx_core::message::{Event, Exposure, Item};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as Input, EventStream, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use futures_util::StreamExt;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Paragraph, Wrap};
use ratatui::{DefaultTerminal, Frame};
use tokio::sync::oneshot;

use crate::marginalia::one_line;
use crate::status::{self, Tally};

/// The narrowest terminal that shows the tiles; narrower ones get the Reading room.
pub const WIDE: u16 = 140;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Panel {
    Diff,
    Tools,
    Stats,
    Context,
    Inspect,
    Prefs,
}

impl Panel {
    /// The side panels in the order Tab visits them.
    const ALL: [Panel; 6] = [
        Panel::Diff,
        Panel::Tools,
        Panel::Stats,
        Panel::Context,
        Panel::Inspect,
        Panel::Prefs,
    ];

    fn title(self) -> &'static str {
        match self {
            Panel::Diff => "Diff",
            Panel::Tools => "Tools",
            Panel::Stats => "Stats",
            Panel::Context => "Context",
            Panel::Inspect => "Self",
            Panel::Prefs => "Prefs",
        }
    }
}

/// Named layouts: the tile under the conversation, then the right column's two.
pub const LAYOUTS: [(&str, [Panel; 3]); 3] = [
    ("coding", [Panel::Diff, Panel::Tools, Panel::Stats]),
    ("research", [Panel::Context, Panel::Tools, Panel::Stats]),
    ("inspect", [Panel::Context, Panel::Inspect, Panel::Tools]),
];

/// What the body of the screen shows at a width.
#[derive(Debug, PartialEq, Eq)]
pub enum Arrangement {
    Tiles([Panel; 3]),
    ReadingRoom(Panel),
}

pub fn arrange(width: u16, layout: usize, side: Panel) -> Arrangement {
    if width >= WIDE {
        Arrangement::Tiles(LAYOUTS[layout].1)
    } else {
        Arrangement::ReadingRoom(side)
    }
}

/// What calx knows about itself, gathered before a turn borrows the agent.
pub struct Info {
    pub model: String,
    pub effort: Option<String>,
    pub session_id: String,
    pub system: String,
    pub shell: String,
    pub cwd: String,
    pub home: String,
    /// Each tool's name, legs, and whether it asks first.
    pub tools: Vec<(&'static str, Exposure, bool)>,
    pub tool_tokens: u64,
    pub started: Instant,
}

impl Info {
    pub fn new(agent: &Agent, shell: &str, cwd: &str) -> Self {
        let specs = agent.toolbox.specs();
        let tools = specs
            .iter()
            .filter_map(|s| {
                let t = agent.toolbox.get(s.name)?;
                Some((s.name, t.exposure(), t.needs_approval()))
            })
            .collect();
        let tool_tokens = specs
            .iter()
            .map(|s| status::estimate(&s.description) + status::estimate(&s.parameters.to_string()))
            .sum();
        Self {
            model: agent.model.clone(),
            effort: agent.effort.clone(),
            session_id: agent.session_id.clone(),
            system: agent.system.clone(),
            shell: shell.to_string(),
            cwd: cwd.to_string(),
            home: calx_core::calx_home()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            tools,
            tool_tokens,
            started: Instant::now(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Running,
    Waiting,
    Ok,
    Failed,
    Declined,
}

struct ToolRow {
    seq: usize,
    name: String,
    summary: String,
    state: State,
    duration_ms: Option<u64>,
    output: String,
    preview: Option<String>,
}

enum Entry {
    User {
        seq: usize,
        text: String,
    },
    Thinking(String),
    Reply(String),
    /// Points at the tool row with this `seq`.
    Tool(usize),
    Note {
        text: String,
        error: bool,
    },
}

/// Estimated tokens by kind, for the Context panel. The model counts the real
/// total; these show where it goes.
#[derive(Default)]
struct Mix {
    messages: u64,
    reasoning: u64,
    tool_output: u64,
}

pub struct App {
    pub yes: bool,
    /// Tools the user answered "always" for, this session.
    pub always: HashSet<String>,
    pub tally: Tally,
    info: Info,
    entries: Vec<Entry>,
    tools: Vec<ToolRow>,
    mix: Mix,
    input: String,
    /// Conversation lines scrolled up from the bottom.
    scroll: u16,
    layout: usize,
    side: Panel,
    pending: Option<(Approval, oneshot::Sender<bool>)>,
    show_why: bool,
    /// Set while a turn runs: when it started and its current request.
    running: Option<(Instant, usize)>,
    /// Items sent with the latest request.
    sent_items: usize,
    items: usize,
}

enum Outcome {
    Done,
    Failed(anyhow::Error),
    Cancelled,
}

impl App {
    pub fn new(agent: &Agent, info: Info, yes: bool, tally: Tally) -> Self {
        let mut app = Self {
            yes,
            always: HashSet::new(),
            tally,
            info,
            entries: Vec::new(),
            tools: Vec::new(),
            mix: Mix::default(),
            input: String::new(),
            scroll: 0,
            layout: 0,
            side: Panel::Diff,
            pending: None,
            show_why: false,
            running: None,
            sent_items: 0,
            items: agent.items.len(),
        };
        app.load_history(agent);
        app
    }

    /// Fills the views from a resumed session's items.
    fn load_history(&mut self, agent: &Agent) {
        for (i, item) in agent.items.iter().enumerate() {
            let seq = i + 1;
            match item {
                Item::User { text } => {
                    self.mix.messages += status::estimate(text);
                    self.entries.push(Entry::User {
                        seq,
                        text: text.clone(),
                    });
                }
                Item::Assistant { text, .. } => {
                    self.mix.messages += status::estimate(text);
                    self.entries.push(Entry::Reply(text.clone()));
                }
                Item::Reasoning { summary, .. } => {
                    self.mix.reasoning += status::estimate(summary);
                    if !summary.is_empty() {
                        self.entries.push(Entry::Thinking(summary.clone()));
                    }
                }
                Item::ToolCall { call, .. } => {
                    let summary = serde_json::from_str(&call.arguments)
                        .ok()
                        .zip(agent.toolbox.get(&call.name))
                        .map(|(args, tool)| tool.summary(&args))
                        .unwrap_or_else(|| call.arguments.clone());
                    self.tools.push(ToolRow {
                        seq,
                        name: call.name.clone(),
                        summary,
                        state: State::Ok,
                        duration_ms: None,
                        output: String::new(),
                        preview: None,
                    });
                    self.entries.push(Entry::Tool(seq));
                }
                Item::ToolResult { call_id, output } => {
                    self.mix.tool_output += status::estimate(output);
                    let call_seq = agent.items[..i].iter().rposition(
                        |it| matches!(it, Item::ToolCall { call, .. } if &call.id == call_id),
                    );
                    if let Some(row) = call_seq.and_then(|at| self.row(at + 1)) {
                        row.state = if output == DECLINED {
                            State::Declined
                        } else if output.starts_with("Error:") {
                            State::Failed
                        } else {
                            State::Ok
                        };
                        row.output = output.clone();
                    }
                }
            }
        }
        if !agent.items.is_empty() {
            self.note(
                format!("Resumed session ({} items).", agent.items.len()),
                false,
            );
        }
    }

    fn row(&mut self, seq: usize) -> Option<&mut ToolRow> {
        self.tools.iter_mut().rev().find(|r| r.seq == seq)
    }

    fn note(&mut self, text: String, error: bool) {
        self.entries.push(Entry::Note { text, error });
    }

    fn observe(&mut self, event: Event) {
        self.tally.observe(&event);
        match event {
            Event::RequestStarted { step, items } => {
                if let Some((_, current)) = &mut self.running {
                    *current = step;
                }
                self.sent_items = items;
                self.items = items;
            }
            Event::ReasoningDelta(delta) => {
                self.mix.reasoning += status::estimate(&delta);
                match self.entries.last_mut() {
                    Some(Entry::Thinking(text)) => text.push_str(&delta),
                    _ => self.entries.push(Entry::Thinking(delta)),
                }
            }
            Event::TextDelta(delta) => {
                self.mix.messages += status::estimate(&delta);
                match self.entries.last_mut() {
                    Some(Entry::Reply(text)) => text.push_str(&delta),
                    _ => self.entries.push(Entry::Reply(delta)),
                }
            }
            Event::RequestFinished { .. } => {}
            Event::ToolStarted {
                seq, name, summary, ..
            } => {
                self.items = self.items.max(seq);
                self.tools.push(ToolRow {
                    seq,
                    name,
                    summary,
                    state: State::Running,
                    duration_ms: None,
                    output: String::new(),
                    preview: None,
                });
                self.entries.push(Entry::Tool(seq));
            }
            Event::ToolFinished {
                seq,
                output,
                is_error,
                duration_ms,
                ..
            } => {
                self.mix.tool_output += status::estimate(&output);
                if let Some(row) = self.row(seq) {
                    row.state = match (output == DECLINED, is_error) {
                        (true, _) => State::Declined,
                        (_, true) => State::Failed,
                        _ => State::Ok,
                    };
                    row.duration_ms = Some(duration_ms);
                    row.output = output;
                }
            }
        }
    }

    fn ask(&mut self, ask: Approval, answer: oneshot::Sender<bool>) {
        if let Some(row) = self.row(ask.seq) {
            row.state = State::Waiting;
            row.preview = ask.preview.clone();
        }
        if ask.preview.is_some() {
            self.side = Panel::Diff;
        }
        self.scroll = 0;
        self.show_why = false;
        self.pending = Some((ask, answer));
    }

    fn answer(&mut self, yes: bool, always: bool) {
        if let Some((ask, tx)) = self.pending.take() {
            if always {
                self.always.insert(ask.name.clone());
            }
            if let Some(row) = self.row(ask.seq) {
                row.state = State::Running;
            }
            let _ = tx.send(yes);
        }
    }

    /// Keys that work while a turn runs and when idle. Returns whether it used the key.
    fn common_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::PageUp => self.scroll = self.scroll.saturating_add(10),
            KeyCode::PageDown => self.scroll = self.scroll.saturating_sub(10),
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_add(1)
            }
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.scroll = self.scroll.saturating_sub(1)
            }
            KeyCode::Tab => self.side = cycle(&Panel::ALL, self.side, 1),
            KeyCode::BackTab => self.side = cycle(&Panel::ALL, self.side, Panel::ALL.len() - 1),
            KeyCode::Char('l') if ctrl => self.layout = (self.layout + 1) % LAYOUTS.len(),
            _ => return false,
        }
        true
    }

    /// Handles a key during a turn. Returns true to cancel the turn.
    fn running_key(&mut self, key: KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if key.code == KeyCode::Esc || (ctrl && key.code == KeyCode::Char('c')) {
            return true;
        }
        if self.common_key(&key) || self.pending.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Char('y') => self.answer(true, false),
            KeyCode::Char('a') => self.answer(true, true),
            KeyCode::Char('n') => self.answer(false, false),
            KeyCode::Char('?') => self.show_why = !self.show_why,
            _ => {}
        }
        false
    }

    // Drawing.

    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let [header, body, footer, input] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);

        let arrangement = arrange(area.width, self.layout, self.side);
        let where_ = match &arrangement {
            Arrangement::Tiles(_) => format!("layout: {} · Ctrl-L next", LAYOUTS[self.layout].0),
            Arrangement::ReadingRoom(_) => "reading room · Tab next panel".to_string(),
        };
        let effort = self.info.effort.as_deref().unwrap_or("no reasoning");
        self.split_line(
            f,
            header,
            vec![dim(format!(
                "calx · {} · {} · {effort} · {}",
                &self.info.session_id[..8],
                self.info.model,
                self.info.cwd
            ))],
            vec![dim(where_)],
        );

        match arrangement {
            Arrangement::Tiles([under, top, bottom]) => {
                let [left, right] =
                    Layout::horizontal([Constraint::Percentage(56), Constraint::Fill(1)])
                        .areas(body);
                let [conv, low] =
                    Layout::vertical([Constraint::Fill(1), Constraint::Percentage(34)]).areas(left);
                let [up, down] =
                    Layout::vertical([Constraint::Percentage(55), Constraint::Fill(1)])
                        .areas(right);
                self.conversation(f, conv);
                self.panel(f, low, under, false);
                self.panel(f, up, top, false);
                self.panel(f, down, bottom, false);
            }
            Arrangement::ReadingRoom(side) => {
                let [conv, panel] =
                    Layout::horizontal([Constraint::Percentage(62), Constraint::Fill(1)])
                        .areas(body);
                self.conversation(f, conv);
                self.panel(f, panel, side, true);
            }
        }

        self.footer(f, footer);
        self.input_line(f, input);
    }

    /// One line with text on the left and on the right.
    fn split_line(&self, f: &mut Frame, area: Rect, left: Vec<Span>, right: Vec<Span>) {
        f.render_widget(
            Paragraph::new(Line::from(right)).alignment(Alignment::Right),
            area,
        );
        f.render_widget(Paragraph::new(Line::from(left)), area);
    }

    fn footer(&self, f: &mut Frame, area: Rect) {
        let t = &self.tally;
        let ctx = t.last.input_tokens;
        let cache = t
            .cache_percent()
            .map(|p| format!(" · cache {p}%"))
            .unwrap_or_default();
        let left = vec![
            dim(format!("§{} · ctx ", self.items + 1)),
            Span::raw(status::tokens(ctx)),
            dim(format!(" {}%{cache}", ctx * 100 / status::CONTEXT_WINDOW)),
        ];
        let open = status::legs(t.exposure)
            .iter()
            .filter(|(_, on)| *on)
            .count();
        let color = if open == 3 { Color::Red } else { Color::Yellow };
        let mut right = Vec::new();
        for (name, on) in status::legs(t.exposure) {
            right.push(dim(format!("  {name} ")));
            right.push(if on {
                Span::styled("●", Style::new().fg(color))
            } else {
                dim("○")
            });
        }
        self.split_line(f, area, left, right);
    }

    fn input_line(&self, f: &mut Frame, area: Rect) {
        let line = match (&self.pending, self.running) {
            (Some((ask, _)), _) => Line::from(vec![
                bold(question(&ask.name)),
                Span::raw("  "),
                bold("y"),
                Span::raw(" yes  "),
                bold("n"),
                Span::raw(" no  "),
                bold("a"),
                Span::raw(" always this session  "),
                bold("?"),
                Span::raw(" why  "),
                dim("· Esc stops the turn"),
            ]),
            (None, Some((started, step))) => Line::from(dim(format!(
                "working · request {step} · {} · Esc stops · PgUp scrolls",
                status::duration(started.elapsed().as_millis() as u64)
            ))),
            (None, None) => {
                let room = area.width.saturating_sub(3) as usize;
                let chars: Vec<char> = self.input.chars().collect();
                let shown: String = chars[chars.len().saturating_sub(room)..].iter().collect();
                let x = area.x + 2 + shown.chars().count() as u16;
                f.set_cursor_position((x, area.y));
                Line::from(vec![
                    Span::styled("> ", Style::new().fg(Color::Cyan)),
                    Span::raw(shown),
                ])
            }
        };
        f.render_widget(Paragraph::new(line), area);
    }

    fn conversation(&self, f: &mut Frame, area: Rect) {
        let block = Block::bordered()
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(" Conversation ", Style::new().fg(Color::Cyan)));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let paragraph =
            Paragraph::new(Text::from(self.conversation_lines())).wrap(Wrap { trim: false });
        let total = paragraph.line_count(inner.width) as u16;
        let top = total
            .saturating_sub(inner.height)
            .saturating_sub(self.scroll);
        f.render_widget(paragraph.scroll((top, 0)), inner);
    }

    fn conversation_lines(&self) -> Vec<Line<'_>> {
        let mut lines = Vec::new();
        for entry in &self.entries {
            match entry {
                Entry::User { seq, text } => {
                    lines.push(Line::default());
                    let mut first = true;
                    for l in text.lines() {
                        let margin = if first {
                            format!("§{seq:<4}")
                        } else {
                            " ".repeat(5)
                        };
                        let label = if first { "you " } else { "    " };
                        lines.push(Line::from(vec![dim(margin), bold(label), Span::raw(l)]));
                        first = false;
                    }
                }
                Entry::Thinking(text) => {
                    for (i, l) in text.trim().lines().enumerate() {
                        let label = if i == 0 { "     think " } else { "          " };
                        lines.push(Line::from(dim(format!("{label}{l}"))));
                    }
                }
                Entry::Reply(text) => {
                    lines.push(Line::default());
                    for l in text.lines() {
                        lines.push(Line::from(vec![Span::raw("     "), Span::raw(l)]));
                    }
                }
                Entry::Tool(seq) => {
                    let Some(row) = self.tools.iter().rev().find(|r| r.seq == *seq) else {
                        continue;
                    };
                    let mut spans = vec![
                        dim(format!("§{seq:<4}")),
                        Span::styled(format!("▸ {} ", row.name), Style::new().fg(Color::Cyan)),
                        Span::raw(one_line(&row.summary, 70)),
                        Span::raw("  "),
                    ];
                    spans.extend(state_spans(row));
                    lines.push(Line::from(spans));
                    let waiting = self.pending.as_ref().is_some_and(|(a, _)| a.seq == *seq);
                    if waiting {
                        for l in row.preview.iter().flat_map(|p| p.lines()).take(12) {
                            lines.push(Line::from(vec![Span::raw("       "), diff_span(l)]));
                        }
                        if let Some((ask, _)) = &self.pending
                            && self.show_why
                        {
                            lines.push(Line::from(dim(format!(
                                "       {}",
                                status::why(&ask.name, ask.exposure)
                            ))));
                        }
                    } else if let Some(first) = row.output.lines().next()
                        && row.state != State::Declined
                    {
                        let style = if row.state == State::Failed {
                            Style::new().fg(Color::Red)
                        } else {
                            Style::new().fg(Color::DarkGray)
                        };
                        lines.push(Line::from(Span::styled(
                            format!("       {}", one_line(first, 100)),
                            style,
                        )));
                    }
                }
                Entry::Note { text, error } => {
                    let style = if *error {
                        Style::new().fg(Color::Red)
                    } else {
                        Style::new().fg(Color::DarkGray)
                    };
                    lines.push(Line::from(Span::styled(format!("     {text}"), style)));
                }
            }
        }
        lines
    }

    fn panel(&self, f: &mut Frame, area: Rect, panel: Panel, tabs: bool) {
        let title = if tabs {
            let mut spans = vec![Span::raw(" ")];
            for p in Panel::ALL {
                let style = if p == panel {
                    Style::new().fg(Color::Cyan)
                } else {
                    Style::new().fg(Color::DarkGray)
                };
                spans.push(Span::styled(format!("{} ", p.title()), style));
            }
            Line::from(spans)
        } else {
            Line::from(Span::styled(
                format!(" {} ", panel.title()),
                Style::new().fg(Color::Gray),
            ))
        };
        let block = Block::bordered()
            .border_style(Style::new().fg(Color::DarkGray))
            .title(title);
        let inner = block.inner(area);
        f.render_widget(block, area);
        let width = inner.width as usize;
        let (lines, stick_to_bottom) = match panel {
            Panel::Diff => (self.diff_lines(), false),
            Panel::Tools => (self.tool_lines(width), true),
            Panel::Stats => (self.stats_lines(width), false),
            Panel::Context => (self.context_lines(width), false),
            Panel::Inspect => (self.self_lines(width), false),
            Panel::Prefs => (self.prefs_lines(width), false),
        };
        let paragraph = Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false });
        let top = if stick_to_bottom {
            (paragraph.line_count(inner.width) as u16).saturating_sub(inner.height)
        } else {
            0
        };
        f.render_widget(paragraph.scroll((top, 0)), inner);
    }

    /// The pending change, or else the latest one, then the earlier ones.
    fn diff_lines(&self) -> Vec<Line<'_>> {
        let with_preview: Vec<&ToolRow> =
            self.tools.iter().filter(|r| r.preview.is_some()).collect();
        let Some(shown) = with_preview
            .iter()
            .find(|r| r.state == State::Waiting)
            .or(with_preview.last())
        else {
            return vec![
                dim("No changes yet. Edits and writes show here before you approve them.").into(),
            ];
        };
        let preview = shown.preview.as_deref().unwrap_or_default();
        let added = preview.lines().filter(|l| l.starts_with('+')).count();
        let removed = preview.lines().filter(|l| l.starts_with('-')).count();
        let mut lines = vec![Line::from(vec![
            bold(format!("{} ", shown.summary)),
            dim(format!("§{} ", shown.seq)),
            Span::styled(format!("+{added} "), Style::new().fg(Color::Green)),
            Span::styled(format!("−{removed}  "), Style::new().fg(Color::Red)),
        ])];
        lines[0].spans.extend(state_spans(shown));
        lines.extend(preview.lines().map(|l| Line::from(diff_span(l))));
        let earlier: Vec<&&ToolRow> = with_preview.iter().filter(|r| r.seq != shown.seq).collect();
        if !earlier.is_empty() {
            lines.push(Line::default());
            lines.push(dim("earlier this run").into());
            for r in earlier.iter().rev().take(8) {
                let mut spans = vec![
                    dim(format!("§{:<4}", r.seq)),
                    Span::raw(format!("{}  ", r.summary)),
                ];
                spans.extend(state_spans(r));
                lines.push(Line::from(spans));
            }
        }
        lines
    }

    fn tool_lines(&self, width: usize) -> Vec<Line<'_>> {
        if self.tools.is_empty() {
            return vec![dim("No tool calls yet.").into()];
        }
        // § 5, tool 7, time 9, state 8.
        let target = width.saturating_sub(5 + 7 + 9 + 8).max(8);
        let mut lines = vec![
            dim(format!(
                "{:<5}{:<7}{:<target$}{:>8} {}",
                "§", "tool", "target", "time", "state"
            ))
            .into(),
        ];
        for r in &self.tools {
            let time = r.duration_ms.map(status::duration).unwrap_or_default();
            let (state, color) = match r.state {
                State::Running => ("running", Color::DarkGray),
                State::Waiting => ("waiting", Color::Yellow),
                State::Ok => ("ok", Color::Green),
                State::Failed => ("failed", Color::Red),
                State::Declined => ("declined", Color::DarkGray),
            };
            lines.push(Line::from(vec![
                Span::raw(format!(
                    "{:<5}{:<7}{:<target$}{:>8} ",
                    r.seq,
                    one_line(&r.name, 6),
                    one_line(&r.summary, target.saturating_sub(2)),
                    time
                )),
                Span::styled(state, Style::new().fg(color)),
            ]));
        }
        lines
    }

    fn stats_lines(&self, width: usize) -> Vec<Line<'_>> {
        let t = &self.tally;
        let row = |label: &str, value: String| kv(label, value, width);
        vec![
            row("requests this turn", t.turn_requests.to_string()),
            row("requests this run", t.session_requests.to_string()),
            row(
                "model / tool time this turn",
                format!(
                    "{} / {}",
                    status::duration(t.turn_model_ms),
                    status::duration(t.turn_tool_ms)
                ),
            ),
            row(
                "cache hit, last request",
                t.cache_percent()
                    .map(|p| format!("{p}%"))
                    .unwrap_or("–".into()),
            ),
            row(
                "in · cached · out, this run",
                format!(
                    "{} · {} · {}",
                    status::tokens(t.session.input_tokens),
                    status::tokens(t.session.cached_input_tokens),
                    status::tokens(t.session.output_tokens)
                ),
            ),
            row("input tokens per request", sparkline(&t.history, 24)),
        ]
    }

    fn context_lines(&self, width: usize) -> Vec<Line<'_>> {
        let total = self.tally.last.input_tokens;
        let system = status::estimate(&self.info.system) + self.info.tool_tokens;
        let known = system + self.mix.messages + self.mix.reasoning + self.mix.tool_output;
        let mut parts = vec![
            ("system, tools", system),
            ("messages", self.mix.messages),
            ("reasoning text", self.mix.reasoning),
            ("tool output", self.mix.tool_output),
        ];
        if total > known {
            parts.push(("other, encrypted", total - known));
        }
        let max = parts.iter().map(|p| p.1).max().unwrap_or(1).max(1);
        let bar_room = width.saturating_sub(16 + 8).clamp(4, 24) as u64;
        let mut lines = vec![Line::from(vec![
            bold(if total > 0 {
                status::tokens(total)
            } else {
                "–".into()
            }),
            dim(format!(
                " of {} (assumed) · last request",
                status::tokens(status::CONTEXT_WINDOW)
            )),
        ])];
        for (label, n) in parts {
            let bar = "█".repeat(((n * bar_room).div_ceil(max)) as usize);
            lines.push(Line::from(vec![
                Span::raw(format!("{label:<16}")),
                Span::styled(
                    format!("{bar:<w$}", w = bar_room as usize),
                    Style::new().fg(Color::Blue),
                ),
                Span::raw(format!(" ~{}", status::tokens(n))),
            ]));
        }
        if let Some(big) = self
            .tools
            .iter()
            .max_by_key(|r| r.output.len())
            .filter(|r| !r.output.is_empty())
        {
            lines.push(
                dim(format!(
                    "largest: §{} {} output ~{}",
                    big.seq,
                    big.name,
                    status::tokens(status::estimate(&big.output))
                ))
                .into(),
            );
        }
        lines.push(dim("Parts are estimates at 4 characters a token.").into());
        lines
    }

    /// How calx is running, to understand it and to improve it.
    fn self_lines(&self, width: usize) -> Vec<Line<'_>> {
        let i = &self.info;
        let row = |label: &str, value: String| kv(label, value, width);
        let mut lines = vec![
            row(
                "calx",
                format!("{} · pid {}", env!("CARGO_PKG_VERSION"), std::process::id()),
            ),
            row(
                "up",
                status::duration(i.started.elapsed().as_millis() as u64),
            ),
            row("session", i.session_id.clone()),
            row("items", self.items.to_string()),
            row("last request sent", format!("§1–§{}", self.sent_items)),
            row("step limit per prompt", MAX_STEPS.to_string()),
            row("home", i.home.clone()),
            Line::default(),
            dim("tools (P private · U untrusted · E egress)").into(),
        ];
        for (name, e, asks) in &i.tools {
            let legs = format!(
                "{}{}{}",
                if e.private { "P" } else { "·" },
                if e.untrusted { "U" } else { "·" },
                if e.egress { "E" } else { "·" }
            );
            let policy = if self.yes {
                "runs (-y)"
            } else if !asks {
                "runs"
            } else if self.always.contains(*name) {
                "runs (always)"
            } else {
                "asks"
            };
            lines.push(kv(name, format!("{legs}  {policy}"), width));
        }
        lines.push(Line::default());
        lines.push(
            dim(format!(
                "system prompt, ~{} tokens",
                status::tokens(status::estimate(&i.system))
            ))
            .into(),
        );
        lines.extend(i.system.lines().map(|l| Line::from(Span::raw(l))));
        lines
    }

    fn prefs_lines(&self, width: usize) -> Vec<Line<'_>> {
        let i = &self.info;
        let row = |label: &str, value: String| kv(label, value, width);
        let mut always: Vec<&str> = self.always.iter().map(String::as_str).collect();
        always.sort();
        vec![
            row("model", i.model.clone()),
            row("effort", i.effort.clone().unwrap_or("none".into())),
            row("approvals", if self.yes { "none (-y)".into() } else { "ask".into() }),
            row("always this session", if always.is_empty() { "–".into() } else { always.join(", ") }),
            row("shell", i.shell.clone()),
            row("layout", LAYOUTS[self.layout].0.to_string()),
            row("context window", format!("{} (assumed)", status::tokens(status::CONTEXT_WINDOW))),
            Line::default(),
            dim("Read-only for now. Set them with --model or CALX_MODEL, --effort or CALX_EFFORT, -y, and CALX_SHELL.").into(),
        ]
    }
}

/// Runs the panel view until the user quits.
pub async fn run(app: App, agent: &mut Agent, save: &dyn Fn(&Agent, usize)) -> Result<()> {
    let mut terminal = ratatui::init();
    let _ = crossterm::execute!(std::io::stdout(), EnableBracketedPaste);
    let result = main_loop(&RefCell::new(app), &mut terminal, agent, save).await;
    let _ = crossterm::execute!(std::io::stdout(), DisableBracketedPaste);
    ratatui::restore();
    result
}

async fn main_loop(
    app: &RefCell<App>,
    terminal: &mut DefaultTerminal,
    agent: &mut Agent,
    save: &dyn Fn(&Agent, usize),
) -> Result<()> {
    let mut keys = EventStream::new();
    loop {
        terminal.draw(|f| app.borrow().draw(f))?;
        let Some(input) = keys.next().await else {
            return Ok(());
        };
        let prompt = {
            let mut a = app.borrow_mut();
            match input? {
                Input::Paste(text) => {
                    a.input.push_str(&text.replace("\r\n", "\n"));
                    continue;
                }
                Input::Key(key) if key.kind == KeyEventKind::Press => {
                    if a.common_key(&key) {
                        continue;
                    }
                    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
                    match key.code {
                        KeyCode::Char('d') if ctrl && a.input.is_empty() => return Ok(()),
                        KeyCode::Char('c') if ctrl => {
                            if a.input.is_empty() {
                                return Ok(());
                            }
                            a.input.clear();
                            continue;
                        }
                        KeyCode::Char('u') if ctrl => {
                            a.input.clear();
                            continue;
                        }
                        KeyCode::Char(c) if !ctrl => {
                            a.input.push(c);
                            continue;
                        }
                        KeyCode::Backspace => {
                            a.input.pop();
                            continue;
                        }
                        KeyCode::Enter => std::mem::take(&mut a.input),
                        _ => continue,
                    }
                }
                _ => continue,
            }
        };
        match prompt.trim() {
            "" => {}
            "/exit" | "/quit" => return Ok(()),
            prompt => turn(app, terminal, &mut keys, agent, prompt, save).await?,
        }
    }
}

/// Runs one prompt while drawing and answering keys. Only terminal errors
/// return `Err`; a failed turn is shown in the conversation.
async fn turn(
    app: &RefCell<App>,
    terminal: &mut DefaultTerminal,
    keys: &mut EventStream,
    agent: &mut Agent,
    prompt: &str,
    save: &dyn Fn(&Agent, usize),
) -> Result<()> {
    let before = agent.items.len();
    {
        let mut a = app.borrow_mut();
        a.tally.start_turn();
        a.scroll = 0;
        a.running = Some((Instant::now(), 1));
        a.mix.messages += status::estimate(prompt);
        a.entries.push(Entry::User {
            seq: before + 1,
            text: prompt.to_string(),
        });
    }
    let outcome = {
        let mut on_event = |event: Event| app.borrow_mut().observe(event);
        let mut approve = async |ask: Approval| -> bool {
            {
                let a = app.borrow();
                if a.yes || a.always.contains(&ask.name) {
                    return true;
                }
            }
            let (tx, rx) = oneshot::channel();
            app.borrow_mut().ask(ask, tx);
            rx.await.unwrap_or(false)
        };
        let run = agent.run(prompt, &mut on_event, &mut approve);
        tokio::pin!(run);
        // Redraws streamed text and the clock between keys.
        let mut tick = tokio::time::interval(Duration::from_millis(80));
        loop {
            terminal.draw(|f| app.borrow().draw(f))?;
            tokio::select! {
                result = &mut run => break match result {
                    Ok(_) => Outcome::Done,
                    Err(e) => Outcome::Failed(e),
                },
                Some(input) = keys.next() => {
                    if let Input::Key(key) = input?
                        && key.kind == KeyEventKind::Press
                        && app.borrow_mut().running_key(key)
                    {
                        break Outcome::Cancelled;
                    }
                }
                _ = tick.tick() => {}
            }
        }
    };
    let mut a = app.borrow_mut();
    a.running = None;
    a.pending = None;
    match outcome {
        Outcome::Done => save(agent, before),
        Outcome::Failed(e) => {
            agent.items.truncate(before);
            a.note(format!("error: {e:#}"), true);
            a.note(
                "This turn was not saved; the next prompt starts from before it.".into(),
                false,
            );
        }
        Outcome::Cancelled => {
            agent.items.truncate(before);
            for row in a.tools.iter_mut() {
                if matches!(row.state, State::Running | State::Waiting) {
                    row.state = State::Declined;
                }
            }
            a.note("Stopped. This turn was not saved.".into(), false);
        }
    }
    a.items = agent.items.len();
    Ok(())
}

fn cycle(all: &[Panel], current: Panel, by: usize) -> Panel {
    let at = all.iter().position(|p| *p == current).unwrap_or(0);
    all[(at + by) % all.len()]
}

fn question(tool: &str) -> &'static str {
    match tool {
        "edit" => "Apply this edit?",
        "write" => "Write this file?",
        "shell" => "Run this command?",
        _ => "Run this tool?",
    }
}

fn state_spans(row: &ToolRow) -> Vec<Span<'static>> {
    let time = row.duration_ms.map(status::duration);
    let (text, color) = match row.state {
        State::Running => ("running".to_string(), Color::DarkGray),
        State::Waiting => ("waiting for you".to_string(), Color::Yellow),
        State::Ok => (
            time.map(|t| format!("{t} ok")).unwrap_or("ok".into()),
            Color::Green,
        ),
        State::Failed => (
            time.map(|t| format!("{t} failed"))
                .unwrap_or("failed".into()),
            Color::Red,
        ),
        State::Declined => ("declined".to_string(), Color::DarkGray),
    };
    vec![Span::styled(text, Style::new().fg(color))]
}

fn diff_span(line: &str) -> Span<'_> {
    let color = match line.chars().next() {
        Some('+') => Color::Green,
        Some('-') => Color::Red,
        _ => Color::DarkGray,
    };
    Span::styled(line, Style::new().fg(color))
}

/// A label on the left and its value on the right of a `width`-wide row.
fn kv(label: &str, value: String, width: usize) -> Line<'static> {
    let gap = width
        .saturating_sub(label.chars().count() + value.chars().count())
        .max(1);
    Line::from(vec![
        Span::styled(label.to_string(), Style::new().fg(Color::DarkGray)),
        Span::raw(format!("{:gap$}{value}", "")),
    ])
}

/// The last `n` values as block characters scaled to the largest.
pub fn sparkline(values: &[u64], n: usize) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let shown = &values[values.len().saturating_sub(n)..];
    let max = shown.iter().copied().max().unwrap_or(0);
    if max == 0 {
        return "–".into();
    }
    shown.iter().map(|v| BARS[(v * 7 / max) as usize]).collect()
}

fn dim<'a>(text: impl Into<std::borrow::Cow<'a, str>>) -> Span<'a> {
    Span::styled(text, Style::new().fg(Color::DarkGray))
}

fn bold<'a>(text: impl Into<std::borrow::Cow<'a, str>>) -> Span<'a> {
    Span::styled(text, Style::new().add_modifier(Modifier::BOLD))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_terminals_get_the_reading_room() {
        assert_eq!(
            arrange(WIDE - 1, 2, Panel::Stats),
            Arrangement::ReadingRoom(Panel::Stats)
        );
        assert_eq!(
            arrange(WIDE, 2, Panel::Stats),
            Arrangement::Tiles(LAYOUTS[2].1)
        );
    }

    #[test]
    fn sparkline_scales_to_the_largest_of_the_last_values() {
        // The 1000 falls outside the last three, so 40 is the tallest bar.
        assert_eq!(sparkline(&[1000, 10, 20, 40], 3), "▂▄█");
        assert_eq!(sparkline(&[], 3), "–");
    }
}
