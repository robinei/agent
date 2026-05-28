use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use agent_core::types::{DiagnosticsFile, NotificationLevel};

use crate::markdown::MarkdownEmitter;

// ── Raw history content (width-agnostic) ──────────────────────────────────

pub enum HistoryItem {
    User(String),
    Assistant {
        content: String,
        thinking: String,
    },
    ToolResult {
        tool: String,
        args: String,
        output: String,
        exit: i32,
    },
    Notification {
        level: NotificationLevel,
        message: String,
    },
    Diagnostics {
        source: String,
        files: Vec<DiagnosticsFile>,
    },
    SessionEnd {
        status: String,
        summary: String,
    },
    FileChanged {
        path: String,
        kind: String,
    },
    Info(String),
}

impl HistoryItem {
    pub fn render(&self, width: u16) -> RenderedLines {
        match self {
            HistoryItem::User(text) => {
                let line = Line::from(vec![
                    Span::styled("> ", Style::new().fg(Color::Green)),
                    Span::raw(text.clone()),
                ]);
                RenderedLines { content: vec![line], thinking: vec![] }
            }

            HistoryItem::Assistant { content, thinking } => {
                let content_lines = render_markdown(content, width as usize);
                let thinking_lines = render_thinking(thinking);
                RenderedLines { content: content_lines, thinking: thinking_lines }
            }

            HistoryItem::ToolResult { tool, args, output, exit } => {
                let mut lines = Vec::new();
                let bold = Style::new().add_modifier(Modifier::BOLD);
                let dim = Style::new().fg(Color::DarkGray);
                let exit_style = if *exit == 0 { dim } else { Style::new().fg(Color::Red) };

                let mut header = vec![Span::styled(format!("  ⚙ {}", tool), bold)];
                if !args.is_empty() {
                    header.push(Span::raw(format!("  {}", args.replace('\n', " "))));
                }
                header.push(Span::styled(format!("  (exit: {})", exit), exit_style));
                lines.push(Line::from(header));

                if !(*exit == 0 && tool == "read") {
                    for text_line in output.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("  │ ", dim),
                            Span::raw(text_line.to_string()),
                        ]));
                    }
                }
                RenderedLines { content: lines, thinking: vec![] }
            }

            HistoryItem::Notification { level, message } => {
                use NotificationLevel::*;
                let (style, prefix) = match level {
                    Info => (Style::new().fg(Color::Yellow), "  "),
                    Warning => (Style::new().fg(Color::Rgb(190, 90, 90)), "  "),
                    Error => (Style::new().fg(Color::Red).add_modifier(Modifier::BOLD), "  Error: "),
                    Fatal => (Style::new().fg(Color::Red).add_modifier(Modifier::BOLD), "  Fatal: "),
                };
                let line = Line::from(vec![Span::styled(format!("{}{}", prefix, message), style)]);
                RenderedLines { content: vec![line], thinking: vec![] }
            }

            HistoryItem::Diagnostics { source, files } => {
                use agent_core::types::{DiagnosticSeverity, lang_display};
                let mut lines = Vec::new();
                let dim = Style::new().fg(Color::DarkGray);
                let bold = Style::new().add_modifier(Modifier::BOLD);

                let new_errors: usize = files.iter()
                    .flat_map(|f| &f.diagnostics)
                    .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::Error)))
                    .count();
                let new_warnings: usize = files.iter()
                    .flat_map(|f| &f.diagnostics)
                    .filter(|d| matches!(d.severity, Some(DiagnosticSeverity::Warning)))
                    .count();
                let header_color = if new_errors > 0 { Color::Red }
                    else if new_warnings > 0 { Color::Yellow }
                    else { Color::DarkGray };

                lines.push(Line::from(vec![
                    Span::styled(format!("  ◈ {}", lang_display(source)),
                        Style::new().fg(header_color)),
                ]));

                for file in files {
                    let display_path = std::path::Path::new(&file.path)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&file.path);
                    lines.push(Line::from(vec![Span::styled(format!("    {}", display_path), bold)]));

                    let line_width = file.diagnostics.iter()
                        .map(|d| (d.range.start.line + 1).to_string().len())
                        .max()
                        .unwrap_or(1);

                    for diag in &file.diagnostics {
                        let (col_style, label) = sev_style_label(diag.severity);
                        let first_line = diag.message.lines().next().unwrap_or("");
                        let msg: String = if first_line.chars().count() > 72 {
                            format!("{}…", first_line.chars().take(71).collect::<String>())
                        } else {
                            first_line.to_string()
                        };
                        lines.push(Line::from(vec![
                            Span::styled(format!("      {} ", label), col_style),
                            Span::styled(
                                format!("{:>width$}  {}", diag.range.start.line + 1, msg,
                                    width = line_width),
                                dim),
                        ]));
                    }

                    let summary = seen_summary(file.seen_errors, file.seen_warnings);
                    if !summary.is_empty() {
                        lines.push(Line::from(vec![
                            Span::styled(format!("      ({})", summary), dim),
                        ]));
                    }
                }
                RenderedLines { content: lines, thinking: vec![] }
            }

            HistoryItem::SessionEnd { status, summary } => {
                let bold = Style::new().add_modifier(Modifier::BOLD);
                let msg = if summary.is_empty() {
                    format!("📝 Session ended ({})", status)
                } else {
                    format!("📝 Session ended ({}): {}", status, summary)
                };
                RenderedLines {
                    content: vec![Line::from(vec![Span::styled(msg, bold)])],
                    thinking: vec![],
                }
            }

            HistoryItem::FileChanged { path, kind } => {
                let line = Line::from(vec![
                    Span::raw(format!("  📄 {} ({})", path, kind)),
                ]);
                RenderedLines { content: vec![line], thinking: vec![] }
            }

            HistoryItem::Info(text) => {
                let lines = text.lines()
                    .map(|l| Line::from(vec![Span::raw(l.to_string())]))
                    .collect();
                RenderedLines { content: lines, thinking: vec![] }
            }
        }
    }
}

// ── Render cache ──────────────────────────────────────────────────────────

pub struct RenderedLines {
    pub content: Vec<Line<'static>>,
    pub thinking: Vec<Line<'static>>,
}

pub struct RenderCache {
    pub width: u16,
    pub rendered: RenderedLines,
}

// ── Active (streaming) item ───────────────────────────────────────────────

pub struct ActiveItem {
    pub content_text: String,
    pub thinking_text: String,
    pub in_thinking: bool,
    pub rendered_width: u16,
    pub content_lines: Vec<Line<'static>>,
    pub thinking_lines: Vec<Line<'static>>,
    pub partial_line: Vec<Span<'static>>,
    pub partial_thinking: String,
}

impl ActiveItem {
    pub fn new() -> Self {
        Self {
            content_text: String::new(),
            thinking_text: String::new(),
            in_thinking: false,
            rendered_width: 0,
            content_lines: Vec::new(),
            thinking_lines: Vec::new(),
            partial_line: Vec::new(),
            partial_thinking: String::new(),
        }
    }

    pub fn push_content_spans(&mut self, spans: &[Span<'static>]) {
        for span in spans {
            let text = span.content.as_ref();
            let mut parts = text.split('\n');
            if let Some(first) = parts.next() {
                if !first.is_empty() {
                    self.partial_line.push(Span::styled(first.to_string(), span.style));
                }
            }
            for part in parts {
                let line = Line::from(std::mem::take(&mut self.partial_line));
                self.content_lines.push(line);
                if !part.is_empty() {
                    self.partial_line.push(Span::styled(part.to_string(), span.style));
                }
            }
        }
    }

    pub fn push_thinking_chunk(&mut self, text: &str) {
        let dim = Style::new().fg(Color::DarkGray);
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            self.partial_thinking.push_str(first);
        }
        for part in parts {
            let line_text = std::mem::take(&mut self.partial_thinking);
            self.thinking_lines.push(Line::from(vec![Span::styled(line_text, dim)]));
            self.partial_thinking.push_str(part);
        }
    }

    pub fn ensure_rendered(&mut self, width: u16) {
        if self.rendered_width == width {
            return;
        }
        self.content_lines.clear();
        self.partial_line.clear();
        let mut md = MarkdownEmitter::new();
        let text = self.content_text.clone();
        let _ = md.push(&text, &mut |spans| {
            self.push_content_spans(spans);
            Ok(())
        }, width as usize);
        self.rendered_width = width;

        self.thinking_lines.clear();
        self.partial_thinking.clear();
        let thinking = self.thinking_text.clone();
        if !thinking.is_empty() {
            self.push_thinking_chunk(&thinking);
        }
    }
}

// ── Interactive mode ──────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
pub enum CreateTreeStep {
    Title,
    RepoPath,
    Model,
}

pub enum AppMode {
    Chat,
    SelectTree {
        trees: Vec<agent_core::types::TreeMeta>,
        selected: usize,
    },
    CreateTree {
        step: CreateTreeStep,
        title: String,
        repo_path: String,
        model: String,
    },
}

// ── AppState ──────────────────────────────────────────────────────────────

pub struct AppState {
    pub mode: AppMode,
    pub history: Vec<HistoryItem>,
    pub cache: Vec<RenderCache>,
    pub active: Option<ActiveItem>,
    /// Lines scrolled up from the bottom. 0 = tracking bottom.
    pub scroll_offset: usize,
    /// Line count from the previous render — used to keep offset stable as content grows.
    pub prev_len: usize,
    /// When true, skip the scroll compensation for one render (set after non-streaming view changes).
    pub suppress_scroll_compensation: bool,
    pub show_thinking: bool,
    pub status: Line<'static>,
    pub spinner_active: bool,
    pub spinner_frame: usize,
}

impl AppState {
    pub fn new() -> Self {
        Self {
            mode: AppMode::Chat,
            history: Vec::new(),
            cache: Vec::new(),
            active: None,
            scroll_offset: 0,
            prev_len: 0,
            suppress_scroll_compensation: false,
            show_thinking: true,
            status: Line::default(),
            spinner_active: false,
            spinner_frame: 0,
        }
    }

    pub fn push_history(&mut self, item: HistoryItem) {
        self.history.push(item);
        self.cache.push(RenderCache { width: 0, rendered: RenderedLines { content: vec![], thinking: vec![] } });
    }

    pub fn ensure_cached(&mut self, idx: usize, width: u16) {
        if self.cache[idx].width != width {
            self.cache[idx].rendered = self.history[idx].render(width);
            self.cache[idx].width = width;
        }
    }

    pub fn active_or_new(&mut self) -> &mut ActiveItem {
        if self.active.is_none() {
            self.active = Some(ActiveItem::new());
        }
        self.active.as_mut().unwrap()
    }

    pub fn finalize_active(&mut self) {
        if let Some(active) = self.active.take() {
            let item = HistoryItem::Assistant {
                content: active.content_text,
                thinking: active.thinking_text,
            };
            self.push_history(item);
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        self.scroll_offset = 0;
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let mut md = MarkdownEmitter::new();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut partial: Vec<Span<'static>> = Vec::new();

    let push_spans = &mut |spans: &[Span<'static>]| -> std::io::Result<()> {
        for span in spans {
            let raw = span.content.as_ref();
            let mut parts = raw.split('\n');
            if let Some(first) = parts.next() {
                if !first.is_empty() {
                    partial.push(Span::styled(first.to_string(), span.style));
                }
            }
            for part in parts {
                lines.push(Line::from(std::mem::take(&mut partial)));
                if !part.is_empty() {
                    partial.push(Span::styled(part.to_string(), span.style));
                }
            }
        }
        Ok(())
    };

    let _ = md.push(text, push_spans, width);
    let _ = md.flush(push_spans, width);
    if !partial.is_empty() {
        lines.push(Line::from(partial));
    }
    lines
}

fn render_thinking(text: &str) -> Vec<Line<'static>> {
    if text.is_empty() {
        return vec![];
    }
    let dim = Style::new().fg(Color::DarkGray);
    text.lines()
        .map(|l| Line::from(vec![Span::styled(l.to_string(), dim)]))
        .collect()
}

fn sev_style_label(sev: Option<agent_core::types::DiagnosticSeverity>) -> (Style, &'static str) {
    use agent_core::types::DiagnosticSeverity::*;
    match sev {
        Some(Error) => (Style::new().fg(Color::Red), "error  "),
        Some(Warning) => (Style::new().fg(Color::Yellow), "warning"),
        Some(Information) => (Style::new().fg(Color::Cyan), "info   "),
        _ => (Style::new().fg(Color::DarkGray), "hint   "),
    }
}

fn seen_summary(errors: u32, warnings: u32) -> String {
    match (errors, warnings) {
        (0, 0) => String::new(),
        (e, 0) => format!("{} seen error{}", e, if e == 1 { "" } else { "s" }),
        (0, w) => format!("{} seen warning{}", w, if w == 1 { "" } else { "s" }),
        (e, w) => format!("{} seen error{}, {} seen warning{}",
            e, if e == 1 { "" } else { "s" },
            w, if w == 1 { "" } else { "s" }),
    }
}
