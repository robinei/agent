use std::io;
use std::time::Duration;

use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent,
            KeyEventKind, KeyModifiers, KeyboardEnhancementFlags, MouseEventKind,
            PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};
use tui_textarea::TextArea;

use crate::app::{AppMode, AppState, CreateTreeStep};

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Debug)]
pub enum AppEvent {
    Submit(String),
    Cancel,
    Resize,
    HistoryPrev,
    HistoryNext,
    ToggleThinking,
    ScrollUp(u16),
    ScrollDown(u16),
    ScrollToTop,
    ScrollToBottom,
    SelectUp,
    SelectDown,
    Confirm,
    NewTree,
}

pub struct App {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
    pub textarea: TextArea<'static>,
}

impl App {
    pub fn new() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture,
            crossterm::terminal::Clear(crossterm::terminal::ClearType::All))?;
        // Best-effort: enable kitty keyboard protocol so Shift+Enter is distinguishable.
        let _ = execute!(stdout, PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
        ));
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;

        let mut textarea = TextArea::default();
        textarea.set_cursor_line_style(Style::default());
        textarea.set_placeholder_text("Type a message… (Enter to send, Shift+Enter for newline)");

        Ok(Self { terminal, textarea })
    }

    pub fn teardown(mut self) -> io::Result<()> {
        terminal::disable_raw_mode()?;
        let _ = execute!(self.terminal.backend_mut(), PopKeyboardEnhancementFlags);
        execute!(self.terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture)?;
        Ok(())
    }

    pub fn width(&self) -> u16 {
        self.terminal.size().map(|r| r.width).unwrap_or(80)
    }

    pub fn draw(&mut self, state: &mut AppState) -> io::Result<()> {
        self.terminal.draw(|frame| {
            match &state.mode {
                AppMode::Chat => draw_chat(frame, state, &self.textarea),
                AppMode::SelectTree { .. } => draw_select_tree(frame, state),
                AppMode::CreateTree { .. } => draw_create_tree(frame, state, &self.textarea),
            }
        })?;
        Ok(())
    }

    pub fn poll_event(&mut self, state: &mut AppState, timeout: Duration) -> io::Result<Option<AppEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        let ev = event::read()?;
        match &state.mode {
            AppMode::Chat => self.handle_chat_event(ev),
            AppMode::SelectTree { .. } => Ok(handle_select_event(ev)),
            AppMode::CreateTree { .. } => self.handle_create_event(ev),
        }
    }

    fn handle_chat_event(&mut self, ev: Event) -> io::Result<Option<AppEvent>> {
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
                match (code, modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(Some(AppEvent::Cancel)),
                    (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
                        return Ok(Some(AppEvent::ToggleThinking));
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        let text = self.textarea.lines().join("\n");
                        let text = text.trim().to_string();
                        if !text.is_empty() {
                            self.textarea = TextArea::default();
                            self.textarea.set_cursor_line_style(Style::default());
                            self.textarea.set_placeholder_text("Type a message… (Enter to send, Shift+Enter for newline)");
                            return Ok(Some(AppEvent::Submit(text)));
                        }
                        return Ok(None);
                    }
                    (KeyCode::Enter, KeyModifiers::SHIFT) => {
                        self.textarea.insert_newline();
                        return Ok(None);
                    }
                    (KeyCode::Up, KeyModifiers::NONE) => {
                        if self.textarea.lines().len() <= 1 {
                            let cursor_row = self.textarea.cursor().0;
                            if cursor_row == 0 {
                                return Ok(Some(AppEvent::HistoryPrev));
                            }
                        }
                    }
                    (KeyCode::Down, KeyModifiers::NONE) => {
                        if self.textarea.lines().len() <= 1 {
                            let (row, _) = self.textarea.cursor();
                            if row + 1 >= self.textarea.lines().len() {
                                return Ok(Some(AppEvent::HistoryNext));
                            }
                        }
                    }
                    (KeyCode::PageUp, _) => return Ok(Some(AppEvent::ScrollUp(10))),
                    (KeyCode::PageDown, _) => return Ok(Some(AppEvent::ScrollDown(10))),
                    (KeyCode::Home, _) => return Ok(Some(AppEvent::ScrollToTop)),
                    (KeyCode::End, _) => return Ok(Some(AppEvent::ScrollToBottom)),
                    _ => {}
                }
                self.textarea.input(ev);
                Ok(None)
            }
            Event::Key(_) => Ok(None),
            Event::Mouse(m) => match m.kind {
                MouseEventKind::ScrollUp => Ok(Some(AppEvent::ScrollUp(3))),
                MouseEventKind::ScrollDown => Ok(Some(AppEvent::ScrollDown(3))),
                _ => Ok(None),
            },
            Event::Resize(_, _) => Ok(Some(AppEvent::Resize)),
            _ => Ok(None),
        }
    }

    fn handle_create_event(&mut self, ev: Event) -> io::Result<Option<AppEvent>> {
        match ev {
            Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
                match (code, modifiers) {
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => return Ok(Some(AppEvent::Cancel)),
                    (KeyCode::Enter, KeyModifiers::NONE) => return Ok(Some(AppEvent::Confirm)),
                    _ => {}
                }
                self.textarea.input(ev);
                Ok(None)
            }
            Event::Key(_) => Ok(None),
            Event::Resize(_, _) => Ok(Some(AppEvent::Resize)),
            _ => Ok(None),
        }
    }
}

fn handle_select_event(ev: Event) -> Option<AppEvent> {
    match ev {
        Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => {
            match (code, modifiers) {
                (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(AppEvent::Cancel),
                (KeyCode::Up | KeyCode::Char('k'), _) => Some(AppEvent::SelectUp),
                (KeyCode::Down | KeyCode::Char('j'), _) => Some(AppEvent::SelectDown),
                (KeyCode::Enter, _) => Some(AppEvent::Confirm),
                (KeyCode::Char('n'), _) => Some(AppEvent::NewTree),
                _ => None,
            }
        }
        Event::Resize(_, _) => Some(AppEvent::Resize),
        _ => None,
    }
}

// ── Chat layout ───────────────────────────────────────────────────────────

fn draw_chat(frame: &mut Frame, state: &mut AppState, textarea: &TextArea) {
    let area = frame.area();
    let textarea_height = textarea.lines().len().max(1) as u16;
    let spinner_height = 1u16;
    let status_height = 1u16;
    let history_height = area.height.saturating_sub(textarea_height + spinner_height + status_height);

    let [history_area, spinner_area, input_area, status_area] = Layout::vertical([
        Constraint::Length(history_height),
        Constraint::Length(spinner_height),
        Constraint::Length(textarea_height),
        Constraint::Length(status_height),
    ]).areas(area);

    draw_history(frame, state, history_area);
    draw_spinner_line(frame, state, spinner_area);
    draw_input(frame, textarea, input_area);
    draw_status(frame, state, status_area);
}

fn draw_history(frame: &mut Frame, state: &mut AppState, area: Rect) {
    if area.height == 0 {
        return;
    }
    let lines = collect_visible_lines(state, area.width, area.height as usize);
    frame.render_widget(Paragraph::new(lines), area);
}

fn collect_visible_lines(state: &mut AppState, width: u16, max_rows: usize) -> Vec<Line<'static>> {
    let n = state.history.len();
    for i in 0..n {
        state.ensure_cached(i, width);
    }

    let mut all_lines: Vec<Line<'static>> = Vec::new();

    for i in 0..n {
        let cache = &state.cache[i];
        if i > 0 { all_lines.push(Line::default()); }
        if state.show_thinking && !cache.rendered.thinking.is_empty() {
            all_lines.extend_from_slice(&cache.rendered.thinking);
            all_lines.push(Line::default());
        }
        all_lines.extend_from_slice(&cache.rendered.content);
    }

    if let Some(active) = state.active.as_mut() {
        active.ensure_rendered(width);
        if !all_lines.is_empty() { all_lines.push(Line::default()); }
        if state.show_thinking {
            let has_thinking = !active.thinking_lines.is_empty() || !active.partial_thinking.is_empty();
            if has_thinking {
                all_lines.extend_from_slice(&active.thinking_lines);
                if !active.partial_thinking.is_empty() {
                    all_lines.push(Line::from(Span::styled(
                        active.partial_thinking.clone(),
                        Style::new().fg(Color::DarkGray),
                    )));
                }
                all_lines.push(Line::default());
            }
        }
        all_lines.extend_from_slice(&active.content_lines);
        if !active.partial_line.is_empty() {
            all_lines.push(Line::from(active.partial_line.clone()));
        }
    }

    let len = all_lines.len();
    // When scrolled up, compensate for new lines arriving below so the viewport
    // stays anchored. Skip this after non-streaming view changes (e.g. toggling
    // thinking, resize) where the line-count delta is not new content.
    if state.scroll_offset > 0 && !state.suppress_scroll_compensation {
        state.scroll_offset += len.saturating_sub(state.prev_len);
    }
    state.suppress_scroll_compensation = false;
    state.prev_len = len;

    if len == 0 { return vec![]; }

    // Clamp so we can't scroll past the top.
    let max_offset = len.saturating_sub(max_rows);
    state.scroll_offset = state.scroll_offset.min(max_offset);

    let end = len.saturating_sub(state.scroll_offset);
    let start = end.saturating_sub(max_rows);
    let visible = &all_lines[start..end];

    // Pad with blank lines above so content sticks to the bottom.
    if visible.len() < max_rows {
        let padding = max_rows - visible.len();
        let mut result = vec![Line::default(); padding];
        result.extend_from_slice(visible);
        result
    } else {
        visible.to_vec()
    }
}

fn draw_spinner_line(frame: &mut Frame, state: &AppState, area: Rect) {
    let text = if state.spinner_active {
        SPINNER_FRAMES[state.spinner_frame % SPINNER_FRAMES.len()].to_string()
    } else {
        String::new()
    };
    frame.render_widget(Paragraph::new(text), area);
}

fn draw_input(frame: &mut Frame, textarea: &TextArea, area: Rect) {
    frame.render_widget(textarea, area);
}

fn draw_status(frame: &mut Frame, state: &AppState, area: Rect) {
    let para = Paragraph::new(state.status.clone())
        .style(Style::new().bg(Color::DarkGray).fg(Color::White));
    frame.render_widget(para, area);
}

// ── SelectTree layout ─────────────────────────────────────────────────────

fn draw_select_tree(frame: &mut Frame, state: &AppState) {
    let AppMode::SelectTree { trees, selected } = &state.mode else { return };

    let area = frame.area();
    let [list_area, help_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(2),
    ]).areas(area);

    let items: Vec<ListItem> = trees.iter().enumerate().map(|(i, t)| {
        let sid = if t.id.len() > 8 { &t.id[..8] } else { &t.id };
        let status = if t.leaf_id.is_some() { "active" } else { "empty" };
        let title = t.title.as_deref().unwrap_or("untitled");
        let content = format!("  {} — {} ({})", sid, title, status);
        let style = if i == *selected {
            Style::new().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::new()
        };
        ListItem::new(content).style(style)
    }).collect();

    let mut list_state = ListState::default().with_selected(Some(*selected));
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("Select a tree").border_type(BorderType::Rounded));

    frame.render_stateful_widget(list, list_area, &mut list_state);

    let help = Paragraph::new(Line::from(vec![
        Span::styled("↑↓/jk", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" navigate  "),
        Span::styled("Enter", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" select  "),
        Span::styled("n", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" new tree  "),
        Span::styled("Ctrl+C", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(" quit"),
    ])).style(Style::new().fg(Color::DarkGray));
    frame.render_widget(help, help_area);
}

// ── CreateTree layout ─────────────────────────────────────────────────────

fn draw_create_tree(frame: &mut Frame, state: &AppState, textarea: &TextArea) {
    let AppMode::CreateTree { step, title, repo_path, model } = &state.mode else { return };

    let area = frame.area();
    let [_, center, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(10),
        Constraint::Fill(1),
    ]).areas(area);

    let [_, form_area, _] = Layout::horizontal([
        Constraint::Fill(1),
        Constraint::Percentage(60),
        Constraint::Fill(1),
    ]).areas(center);

    let prompt = match step {
        CreateTreeStep::Title => "Tree title:",
        CreateTreeStep::RepoPath => "Repository path (optional):",
        CreateTreeStep::Model => "Model (optional):",
    };

    let hint = match step {
        CreateTreeStep::Title => format!("Title: {}", if title.is_empty() { "(empty → 'default')" } else { title }),
        CreateTreeStep::RepoPath => format!("Title: {}  |  Path: {}", title, if repo_path.is_empty() { "(none)" } else { repo_path }),
        CreateTreeStep::Model => format!("Title: {}  |  Path: {}  |  Model: {}", title, if repo_path.is_empty() { "(none)" } else { repo_path }, if model.is_empty() { "(default)" } else { model }),
    };

    let [prompt_area, input_area, hint_area, help_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ]).areas(form_area);

    frame.render_widget(
        Paragraph::new(prompt).style(Style::new().add_modifier(Modifier::BOLD)),
        prompt_area,
    );
    frame.render_widget(textarea, input_area);
    frame.render_widget(
        Paragraph::new(hint).style(Style::new().fg(Color::DarkGray)),
        hint_area,
    );
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Enter", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" next  "),
            Span::styled("Ctrl+C", Style::new().add_modifier(Modifier::BOLD)),
            Span::raw(" cancel"),
        ])).style(Style::new().fg(Color::DarkGray)),
        help_area,
    );
}
