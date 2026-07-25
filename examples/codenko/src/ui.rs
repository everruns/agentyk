//! The terminal UI, built on `tuika`'s [`AsyncRunner`].
//!
//! The loop is the runner's single `tokio::select!`: input events and a tick
//! both land in `App::update`, which drains the agent channel and mutates
//! state. Nothing here awaits a turn — the agent runs in its own task (see
//! [`crate::agent`]), so the UI stays responsive while a tool is running and
//! esc can interrupt it.
//!
//! Rows are laid out in `App::update` rather than in the view because
//! wrapping needs a width and the view callback only gets `&App`. Tracking the
//! terminal size in state keeps the two in agreement.

use std::ops::ControlFlow;
use std::time::Duration;

use agentyk::{CancellationToken, EventData, ToolCall};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};
use tokio::sync::{mpsc, oneshot};
use tuika::{
    AsyncRunner, Boxed, Dimension, Element, Event, Flex, KeyCode, KeyHints, RunnerConfig, Scroll,
    ScrollState, Signal, Spinner, StatusBar, Text, TextInputState, Theme, element,
};

use crate::agent::{AppEvent, Prompt, TurnOutcome};
use crate::transcript::{Entry, ToolState, Transcript, summarize_arguments};

/// Fast enough for a smooth spinner, slow enough to stay idle-cheap. Also the
/// interval at which agent events are drained.
const TICK: Duration = Duration::from_millis(80);

const HEADER_ROWS: u16 = 1;
const HINTS_ROWS: u16 = 1;
const COMPOSER_ROWS: u16 = 3;
const APPROVAL_ROWS: u16 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Idle,
    Working,
}

/// A gated tool call waiting on the operator.
struct Pending {
    call: ToolCall,
    reply: oneshot::Sender<bool>,
}

pub struct App {
    transcript: Transcript,
    input: TextInputState,
    scroll: ScrollState,
    status: Status,
    pending: Option<Pending>,
    /// Cancels the in-flight turn. Present only while one is running.
    cancel: Option<CancellationToken>,
    prompts: mpsc::UnboundedSender<Prompt>,
    events: mpsc::UnboundedReceiver<AppEvent>,
    /// Last painted terminal size, used to lay out `rows`.
    size: (u16, u16),
    /// The transcript wrapped to the current width — rebuilt whenever the
    /// transcript or the width changes, not on every frame.
    rows: Vec<Line<'static>>,
    /// Whether this turn already recorded a `turn.failed` event, so the
    /// task's outcome doesn't report the same failure a second time.
    reported_failure: bool,
    banner: String,
    quit: bool,
}

impl App {
    pub fn new(
        prompts: mpsc::UnboundedSender<Prompt>,
        events: mpsc::UnboundedReceiver<AppEvent>,
        banner: String,
        size: (u16, u16),
    ) -> Self {
        let mut app = Self {
            transcript: Transcript::new(),
            input: TextInputState::new(),
            scroll: ScrollState::new(),
            status: Status::Idle,
            pending: None,
            cancel: None,
            prompts,
            events,
            size,
            rows: Vec::new(),
            reported_failure: false,
            banner,
            quit: false,
        };
        app.transcript.notice(
            "Ask for a change, a review, or a command. Mutating tools ask before they run.",
        );
        app.relayout();
        app
    }

    /// Run until the operator quits. The terminal is restored on the way out,
    /// including on error or panic (the runner's `TerminalSession` guard).
    pub async fn run(mut self) -> std::io::Result<()> {
        let runner = AsyncRunner::new(RunnerConfig { tick_rate: TICK });
        // `view` and `update` borrow the same value at different times, which
        // is why the runner takes the state as an argument rather than a
        // closure capture.
        runner
            .run(
                &Theme::default(),
                &mut self,
                |app, frame| app.view(frame),
                async |app, signal| app.update(signal),
            )
            .await
    }

    // ---------------------------------------------------------------- update

    fn update(&mut self, signal: Signal) -> ControlFlow<()> {
        // Agent events arrive between signals, so drain on every one.
        self.drain_agent_events();
        if let Signal::Event(event) = &signal {
            self.handle_input(event);
        }
        self.relayout();
        match self.quit {
            true => ControlFlow::Break(()),
            false => ControlFlow::Continue(()),
        }
    }

    fn drain_agent_events(&mut self) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                AppEvent::Agent(event) => {
                    match &event.data {
                        EventData::TurnStarted => self.reported_failure = false,
                        EventData::TurnFailed { .. } => self.reported_failure = true,
                        _ => {}
                    }
                    self.transcript.apply(&event);
                }
                AppEvent::TurnEnded(outcome) => {
                    self.status = Status::Idle;
                    self.cancel = None;
                    // A failure inside the turn already produced a notice from
                    // the event stream; this covers the ones that can't (a
                    // failure before or around the turn itself).
                    if let TurnOutcome::Failed(error) = &outcome
                        && !self.reported_failure
                    {
                        self.transcript.notice(format!("turn failed: {error}"));
                    }
                }
                AppEvent::Approval { call, reply } => {
                    // One prompt at a time: parallel gated calls queue behind
                    // the hook, which is exactly the serialization we want.
                    self.pending = Some(Pending { call, reply });
                }
            }
        }
    }

    fn handle_input(&mut self, event: &Event) {
        match event {
            Event::Resize { width, height } => self.size = (*width, *height),
            Event::Mouse(_) => {
                self.scroll
                    .handle(event, self.rows.len(), self.viewport_rows() as usize);
            }
            // Quit is checked before the approval prompt: the prompt ignores
            // every other key, so putting it first would trap the operator
            // there with `n` as the only way out.
            Event::Key(key)
                if key.ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('d')) =>
            {
                self.quit = true;
            }
            Event::Key(key) if self.pending.is_some() => self.answer_approval(key.code),
            Event::Key(key) if key.plain() && key.code == KeyCode::Esc => self.interrupt(),
            Event::Key(key) if key.plain() && key.code == KeyCode::Enter => self.submit(),
            Event::Key(key)
                if key.plain() && matches!(key.code, KeyCode::PageUp | KeyCode::PageDown) =>
            {
                self.scroll
                    .handle(event, self.rows.len(), self.viewport_rows() as usize);
            }
            // Ctrl+J would insert a newline; codenko's composer is one line.
            Event::Key(key) if key.ctrl && key.code == KeyCode::Char('j') => {}
            _ => {
                self.input.handle(event);
                // A pasted newline would break the single-line composer.
                if self.input.line_count() > 1 {
                    let flattened = self.input.text().replace('\n', " ");
                    self.input.set_text(&flattened);
                }
            }
        }
    }

    fn answer_approval(&mut self, code: KeyCode) {
        let allow = match code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => true,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => false,
            _ => return,
        };
        if let Some(pending) = self.pending.take() {
            // A closed receiver means the turn is already gone; the hook
            // fails closed on its own.
            let _ = pending.reply.send(allow);
        }
    }

    fn interrupt(&mut self) {
        match &self.cancel {
            Some(token) => token.cancel(),
            // Idle: esc clears a half-typed prompt.
            None => self.input.clear(),
        }
    }

    fn submit(&mut self) {
        let text = self.input.text().trim().to_string();
        if text.is_empty() || self.status == Status::Working {
            return;
        }
        let cancel = CancellationToken::new();
        let prompt = Prompt {
            text,
            cancel: cancel.clone(),
        };
        if self.prompts.send(prompt).is_err() {
            self.transcript.notice("the agent task is gone");
            self.quit = true;
            return;
        }
        self.input.clear();
        self.cancel = Some(cancel);
        self.status = Status::Working;
        // Follow the newest output again after sending.
        self.scroll
            .jump_to_bottom(self.rows.len(), self.viewport_rows() as usize);
    }

    // ---------------------------------------------------------------- layout

    fn footer_rows(&self) -> u16 {
        match self.pending.is_some() {
            true => APPROVAL_ROWS,
            false => COMPOSER_ROWS,
        }
    }

    fn viewport_rows(&self) -> u16 {
        self.size
            .1
            .saturating_sub(HEADER_ROWS + HINTS_ROWS + self.footer_rows())
            .max(1)
    }

    /// Re-wrap the transcript and reconcile the scroll offset. Cheap enough to
    /// run per signal; the alternative is wrapping inside every frame.
    fn relayout(&mut self) {
        self.rows = render_transcript(&self.transcript, self.size.0, &Theme::default());
        self.scroll
            .clamp(self.rows.len(), self.viewport_rows() as usize);
    }

    // ------------------------------------------------------------------ view

    fn view(&self, frame: u64) -> Element {
        let theme = Theme::default();
        let viewport = self.viewport_rows() as usize;
        // Hand `Scroll` only the visible window: O(viewport) per frame instead
        // of cloning the whole transcript.
        let window: Vec<Line<'static>> = self
            .rows
            .iter()
            .skip(self.scroll.offset())
            .take(viewport)
            .cloned()
            .collect();

        element(
            Flex::column()
                .child(Dimension::Fixed(HEADER_ROWS), self.header(frame, &theme))
                .child(
                    Dimension::Flex(1),
                    element(Scroll::windowed(window, self.rows.len(), &self.scroll)),
                )
                .child(
                    Dimension::Fixed(self.footer_rows()),
                    self.footer(frame, &theme),
                )
                .child(Dimension::Fixed(HINTS_ROWS), self.hints()),
        )
    }

    fn header(&self, frame: u64, theme: &Theme) -> Element {
        let mut left = vec![Span::styled(
            " codenko ",
            Style::default()
                .fg(theme.selection_fg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
        )];
        left.push(Span::styled(
            format!(" {}", self.banner),
            theme.muted_style(),
        ));

        let right = match self.status {
            Status::Idle => vec![Span::styled("ready ", theme.muted_style())],
            Status::Working => vec![
                Span::styled(
                    Spinner::new(frame).glyph().to_string(),
                    theme.accent_style(),
                ),
                Span::styled(" working ", theme.muted_style()),
            ],
        };
        element(
            StatusBar::new()
                .left(left)
                .right(right)
                .background(Style::default().bg(theme.surface)),
        )
    }

    fn footer(&self, frame: u64, theme: &Theme) -> Element {
        match &self.pending {
            Some(pending) => approval_panel(pending, theme),
            None => composer(&self.input, self.size.0, frame, theme),
        }
    }

    fn hints(&self) -> Element {
        let hints: &[(&str, &str)] = match (self.pending.is_some(), self.status) {
            (true, _) => &[("y", "allow"), ("n", "deny")],
            (false, Status::Working) => &[("esc", "interrupt"), ("ctrl+c", "quit")],
            (false, Status::Idle) => &[
                ("enter", "send"),
                ("pgup/pgdn", "scroll"),
                ("ctrl+c", "quit"),
            ],
        };
        element(KeyHints::new(hints.iter().copied()))
    }
}

/// The composer: a one-line input with a blinking block caret.
///
/// Rendered by hand rather than with `TextInput` because the runner owns the
/// frame and never exposes the terminal cursor, so the caret has to be drawn
/// as a styled cell.
fn composer(input: &TextInputState, width: u16, frame: u64, theme: &Theme) -> Element {
    let text: Vec<char> = input.text().chars().collect();
    let (_, cursor) = input.cursor();
    // Boxed eats 2 border + 2 padding columns; the prompt takes 2 more, plus
    // one spare so the caret is visible at end of line.
    let available = width.saturating_sub(7).max(1) as usize;
    let start = cursor.saturating_sub(available);
    let visible: String = text.iter().skip(start).take(available).collect();
    let caret_at = cursor - start;

    // Blink on the frame counter: the runner advances it once per signal, so
    // the caret pulses at roughly twice a second at this tick rate.
    let caret_style = match frame / 6 % 2 {
        0 => Style::default().fg(theme.background).bg(theme.accent_alt),
        _ => theme.text_style(),
    };
    let mut spans = vec![Span::styled("❯ ", theme.accent_style())];
    let chars: Vec<char> = visible.chars().collect();
    spans.push(Span::styled(
        chars.iter().take(caret_at).collect::<String>(),
        theme.text_style(),
    ));
    spans.push(Span::styled(
        chars.get(caret_at).copied().unwrap_or(' ').to_string(),
        caret_style,
    ));
    spans.push(Span::styled(
        chars.iter().skip(caret_at + 1).collect::<String>(),
        theme.text_style(),
    ));

    element(
        Boxed::new(element(Text::new(vec![Line::from(spans)]))).border_color(theme.border_focused),
    )
}

/// The approval prompt, shown in place of the composer so the layout height
/// is the only thing that changes.
fn approval_panel(pending: &Pending, theme: &Theme) -> Element {
    let lines = vec![
        Line::from(vec![
            Span::styled(pending.call.name.clone(), theme.accent_style()),
            Span::styled(" wants to run", theme.muted_style()),
        ]),
        Line::from(Span::styled(
            summarize_arguments(&pending.call.arguments),
            Style::default().fg(theme.code.string),
        )),
        Line::from(Span::styled("[y] allow   [n] deny", theme.muted_style())),
    ];
    element(
        Boxed::new(element(Text::new(lines)))
            .title(Line::from(Span::styled(
                " approval ",
                theme.accent_style().add_modifier(Modifier::BOLD),
            )))
            .border_color(theme.accent),
    )
}

/// Flatten the transcript into wrapped, styled rows.
///
/// Separate from `App` and from any terminal so the rendering is exercised by
/// plain unit tests.
pub fn render_transcript(transcript: &Transcript, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    let width = width.max(20) as usize;
    let mut rows = Vec::new();
    for entry in transcript.entries() {
        if !rows.is_empty() {
            rows.push(Line::default());
        }
        match entry {
            Entry::User(text) => rows.extend(block(
                text,
                "❯ ",
                "  ",
                width,
                theme.accent_style().add_modifier(Modifier::BOLD),
            )),
            Entry::Assistant(text) => {
                rows.extend(block(text, "  ", "  ", width, theme.text_style()));
            }
            Entry::Notice(text) => {
                rows.extend(block(text, "· ", "  ", width, theme.muted_style()));
            }
            Entry::Tool {
                name,
                summary,
                output,
                state,
                ..
            } => {
                let (glyph, style) = match state {
                    ToolState::Running => ("◍", theme.accent_style()),
                    ToolState::Ok => ("✔", Style::default().fg(theme.code.string)),
                    ToolState::Failed => ("✖", Style::default().fg(theme.accent)),
                    ToolState::Denied => ("⊘", Style::default().fg(theme.accent_alt)),
                };
                rows.push(Line::from(vec![
                    Span::styled(format!("{glyph} "), style),
                    Span::styled(name.clone(), theme.text_style()),
                    Span::styled(format!(" {summary}"), theme.muted_style()),
                ]));
                if !output.trim().is_empty() {
                    // Tool output is reference material, not prose: keep it
                    // dim, indented, and capped so one build log can't bury
                    // the conversation.
                    let body = clip_lines(output, 12);
                    rows.extend(block(
                        &body,
                        "    ",
                        "    ",
                        width,
                        Style::default().fg(theme.dim),
                    ));
                }
            }
        }
    }
    rows
}

/// Wrap `text` to `width`, prefixing the first row with `first` and the rest
/// with `rest`, so continuation lines stay under their marker.
fn block(text: &str, first: &str, rest: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    let indent = first.chars().count().max(rest.chars().count());
    let inner = width.saturating_sub(indent).max(8);
    let mut rows = Vec::new();
    for source in text.lines() {
        let wrapped = match source.trim().is_empty() {
            true => vec![String::new()],
            false => textwrap::wrap(source, inner)
                .into_iter()
                .map(|line| line.into_owned())
                .collect(),
        };
        for line in wrapped {
            let prefix = match rows.is_empty() {
                true => first,
                false => rest,
            };
            rows.push(Line::from(vec![
                Span::raw(prefix.to_string()),
                Span::styled(line, style),
            ]));
        }
    }
    rows
}

/// Keep the first `keep` lines of `text`, noting how many were dropped.
fn clip_lines(text: &str, keep: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    if lines.len() <= keep {
        return text.trim_end().to_string();
    }
    let mut kept = lines[..keep].join("\n");
    kept.push_str(&format!("\n… {} more lines", lines.len() - keep));
    kept
}

#[cfg(test)]
mod tests {
    use super::*;
    // `Event` here is tuika's input event (from the glob above); the protocol
    // event needs its own name.
    use agentyk::{Event as ProtocolEvent, EventData, EventId, EventRequest, Message, SessionId};

    fn protocol_event(data: EventData) -> ProtocolEvent {
        EventRequest::new(SessionId::new(), data).into_event(EventId::new(), chrono::Utc::now(), 1)
    }

    #[test]
    fn transcript_rows_carry_markers_and_wrap() {
        let mut transcript = Transcript::new();
        transcript.apply(&protocol_event(EventData::InputMessage {
            message: Message::user("a fairly long request that needs wrapping at this width"),
        }));
        let rows = render_transcript(&transcript, 30, &Theme::default());
        let rendered: Vec<String> = rows
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rendered[0].starts_with("❯ "), "{rendered:?}");
        assert!(rendered.len() > 1, "expected wrapping: {rendered:?}");
        assert!(rendered[1].starts_with("  "), "{rendered:?}");
    }

    #[test]
    fn long_tool_output_is_clipped() {
        let body = (1..=40)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let clipped = clip_lines(&body, 12);
        assert!(clipped.contains("28 more lines"), "{clipped}");
        assert_eq!(clipped.lines().count(), 13);
    }

    #[test]
    fn block_hangs_continuations_under_the_marker() {
        let rows = block("one two three four five", "❯ ", "  ", 20, Style::default());
        assert!(rows.len() >= 2);
        assert_eq!(rows[1].spans[0].content.as_ref(), "  ");
    }

    /// An `App` plus the far ends of its channels, which have to stay alive:
    /// a dropped prompt receiver would make `submit` believe the agent task
    /// had died.
    struct Harness {
        app: App,
        _prompts: mpsc::UnboundedReceiver<Prompt>,
        _events: mpsc::UnboundedSender<AppEvent>,
    }

    fn harness() -> Harness {
        let (prompts, prompt_inbox) = mpsc::unbounded_channel();
        let (event_sender, events) = mpsc::unbounded_channel();
        Harness {
            app: App::new(prompts, events, "test".into(), (80, 24)),
            _prompts: prompt_inbox,
            _events: event_sender,
        }
    }

    fn key(code: KeyCode, ctrl: bool) -> Event {
        Event::Key(tuika::Key {
            code,
            ctrl,
            alt: false,
            shift: false,
        })
    }

    fn pending_approval(app: &mut App) -> oneshot::Receiver<bool> {
        let (reply, answer) = oneshot::channel();
        app.pending = Some(Pending {
            call: ToolCall {
                id: "call-1".into(),
                name: "run_command".into(),
                arguments: serde_json::json!({"command": "ls"}),
            },
            reply,
        });
        answer
    }

    #[test]
    fn an_approval_prompt_takes_only_its_own_keys() {
        let mut harness = harness();
        let app = &mut harness.app;
        let mut answer = pending_approval(app);

        // Typing while the prompt is up must not leak into the composer or
        // answer by accident.
        app.handle_input(&key(KeyCode::Char('x'), false));
        assert!(app.input.is_empty());
        assert!(app.pending.is_some());
        assert_eq!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty));

        app.handle_input(&key(KeyCode::Char('y'), false));
        assert!(app.pending.is_none());
        assert_eq!(answer.try_recv(), Ok(true));
    }

    #[test]
    fn denying_reports_the_decision() {
        let mut harness = harness();
        let mut answer = pending_approval(&mut harness.app);
        harness.app.handle_input(&key(KeyCode::Char('n'), false));
        assert_eq!(answer.try_recv(), Ok(false));
    }

    #[test]
    fn quit_wins_over_a_pending_prompt() {
        // Regression: with the prompt checked first, ctrl+c was swallowed and
        // `n` became the only way out of an approval.
        let mut harness = harness();
        let answer = pending_approval(&mut harness.app);
        harness.app.handle_input(&key(KeyCode::Char('c'), true));
        assert!(harness.app.quit);
        // Quitting drops the prompt, so the hook sees a closed channel and
        // fails closed rather than hanging the turn.
        drop(harness);
        assert!(answer.blocking_recv().is_err());
    }

    #[test]
    fn a_pasted_newline_cannot_split_the_composer() {
        let mut harness = harness();
        harness.app.handle_input(&Event::Paste("one\ntwo".into()));
        assert_eq!(harness.app.input.text(), "one two");
    }
}
