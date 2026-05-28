use std::io;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span as RSpan;
use unicode_width::UnicodeWidthStr;

type Span = RSpan<'static>;

/// Streaming markdown renderer. Feed LLM tokens via `push`; call `flush` at
/// end-of-turn to drain any content held back waiting for a closing marker.
///
/// Plain text and code-block content stream through immediately (subject to
/// `Terminal`'s own flush interval). Only short inline constructs — bold,
/// italic, inline code — are buffered until their closing marker arrives.
/// Tables are fully buffered until the table ends so that column widths can
/// be computed from actual content before rendering.
///
/// # Missing features (not implemented, accepted for this use case)
///
/// Inline:
/// - `[text](url)` links / `![alt](url)` images
/// - Auto-links (bare URLs)
/// - Backslash escapes
/// - HTML entities (`&amp;`, …)
///
/// Block:
/// - `> blockquote`
/// - `-` / `*` / `1.` lists (ordered & unordered)
/// - Indented code blocks (4 spaces)
pub struct MarkdownEmitter {
    state: State,
    line_start: bool,
    inline: InlineParser,
    segment: String,
    term_width: usize,
}

// ── Shared inline parser (used by both MarkdownEmitter and CellParser) ─────

#[derive(Clone, Copy, PartialEq)]
enum FormatKind {
    Bold,
    Italic,
    Strikethrough,
}

struct FormatFrame {
    kind: FormatKind,
    buf: String,
}

/// Temporary state types used during delimiter-run processing.
enum InlineState {
    Normal,
    PendingStars { count: usize },
    InlineCode { buf: String },
    PendingTag { buf: String },
    PendingTildes { count: usize },
}

struct InlineParser {
    state: InlineState,
    /// Stack of active formatting spans (bold, italic, strikethrough).
    /// The top of the stack is the innermost active format.
    formats: Vec<FormatFrame>,
    spans: Vec<Span>,
    segment: String,
}

impl FormatKind {
    fn delim_open(&self) -> &'static str {
        match self {
            FormatKind::Bold => "**",
            FormatKind::Italic => "*",
            FormatKind::Strikethrough => "~~",
        }
    }
    fn style(&self) -> Style {
        match self {
            FormatKind::Bold => bold_style(),
            FormatKind::Italic => italic_style(),
            FormatKind::Strikethrough => strikethrough_style(),
        }
    }
}

impl InlineParser {
    fn new() -> Self {
        Self { state: InlineState::Normal, formats: Vec::new(), spans: Vec::new(), segment: String::new() }
    }

    /// Append text to the current output buffer.
    /// If a format is active, text goes to the innermost frame's buf;
    /// otherwise it goes to the plain segment.
    fn push_text(&mut self, s: &str) {
        if let Some(top) = self.formats.last_mut() {
            top.buf.push_str(s);
        } else {
            self.segment.push_str(s);
        }
    }

    fn push_char(&mut self, c: char) {
        if let Some(top) = self.formats.last_mut() {
            top.buf.push(c);
        } else {
            self.segment.push(c);
        }
    }

    /// Flush plain-text segment to spans.
    fn flush_segment(&mut self) {
        if !self.segment.is_empty() {
            let s = std::mem::take(&mut self.segment);
            self.spans.push(Span::raw(s));
        }
    }

    /// Combined style from all active formats on the stack
    /// (innermost attributes ORed over outer ones).
    fn cumulative_style(&self) -> Style {
        self.formats.iter().fold(Style::default(), |acc, f| acc.patch(f.kind.style()))
    }

    /// Open a new format: flush any trailing plain text, then push the frame.
    /// If a parent format is active, flush its accumulated text first so that
    /// spans appear in source order (parent before child).
    fn open_format(&mut self, kind: FormatKind) {
        if let Some(parent) = self.formats.last_mut() {
            let text = std::mem::take(&mut parent.buf);
            if !text.is_empty() {
                self.spans.push(Span::styled(text, self.cumulative_style()));
            }
        }
        self.flush_segment();
        self.formats.push(FormatFrame { kind, buf: String::new() });
    }

    /// Try to close the innermost format matching `kind`.
    /// Returns true if a format was closed.
    fn try_close_format(&mut self, kind: FormatKind) -> bool {
        // Only close if the top of stack matches
        if self.formats.last().map(|f| f.kind == kind).unwrap_or(false) {
            // Capture style BEFORE pop — cumulative_style iterates current stack
            let style = self.cumulative_style();
            let mut frame = self.formats.pop().unwrap();
            let text = std::mem::take(&mut frame.buf);
            if !text.is_empty() {
                self.spans.push(Span::styled(text, style));
            }
            true
        } else {
            false
        }
    }

    /// Resolve N consecutive `*` or `_` delimiter characters.
    /// Tries to close matching formats from innermost out, then opens new ones.
    fn resolve_stars(&mut self, count: usize) {
        let mut n = count;

        // If top is Italic and n is odd, close Italic with 1 star.
        if n % 2 == 1
            && self.formats.last().map(|f| f.kind == FormatKind::Italic).unwrap_or(false)
        {
            self.try_close_format(FormatKind::Italic);
            n -= 1;
        }

        // Close Bold with pairs. If an unclosed Italic sits between the cursor
        // and a Bold below it, collapse that Italic as literal (* + its buf)
        // into the Bold frame, then close the Bold. This handles the common
        // pattern `**bold *text**` where the inner * has no matching closer.
        while n >= 2 {
            if self.try_close_format(FormatKind::Bold) {
                n -= 2;
            } else if self.formats.len() >= 2
                && self.formats.last().map(|f| f.kind == FormatKind::Italic).unwrap_or(false)
                && self.formats[self.formats.len() - 2].kind == FormatKind::Bold
            {
                let italic = self.formats.pop().unwrap();
                let parent = self.formats.last_mut().unwrap();
                parent.buf.push('*');
                parent.buf.push_str(&italic.buf);
            } else {
                break;
            }
        }

        // Open remaining: pairs → Bold, odd single → Italic.
        while n >= 2 {
            self.open_format(FormatKind::Bold);
            n -= 2;
        }
        if n >= 1 {
            self.open_format(FormatKind::Italic);
        }
    }

    /// Resolve N consecutive `~` characters.
    fn resolve_tildes(&mut self, count: usize) {
        let mut n = count;
        while n >= 2 {
            if !self.try_close_format(FormatKind::Strikethrough) {
                self.open_format(FormatKind::Strikethrough);
            }
            n -= 2;
        }
        if n > 0 {
            self.push_char('~');
        }
    }

    fn feed(&mut self, c: char) {
        let state = std::mem::replace(&mut self.state, InlineState::Normal);
        match state {
            InlineState::Normal => match c {
                '*' | '_' => { self.state = InlineState::PendingStars { count: 1 }; }
                '`' => { self.flush_segment(); self.state = InlineState::InlineCode { buf: String::new() }; }
                '<' => { self.flush_segment(); self.state = InlineState::PendingTag { buf: "<".to_string() }; }
                '~' => { self.state = InlineState::PendingTildes { count: 1 }; }
                _ => { self.push_char(c); self.state = InlineState::Normal; }
            },
            InlineState::PendingStars { count } => match c {
                '*' | '_' => { self.state = InlineState::PendingStars { count: count + 1 }; }
                _ => { self.resolve_stars(count); self.feed(c); }
            },
            InlineState::InlineCode { mut buf } => match c {
                '`' => {
                    self.flush_segment();
                    let style = inline_code_style().patch(self.cumulative_style());
                    self.spans.push(Span::styled(buf, style));
                    self.state = InlineState::Normal;
                }
                _ => { buf.push(c); self.state = InlineState::InlineCode { buf }; }
            },
            InlineState::PendingTildes { count } => match c {
                '~' => { self.state = InlineState::PendingTildes { count: count + 1 }; }
                _ => { self.resolve_tildes(count); self.feed(c); }
            },
            InlineState::PendingTag { mut buf } => {
                buf.push(c);
                match buf.as_str() {
                    "<br>" | "<br/>" | "<br />" => {
                        self.flush_segment();
                        self.segment.push('\n');
                        self.flush_segment();
                        self.state = InlineState::Normal;
                    }
                    _ if buf == "<" || buf == "<b" || buf == "<br" || buf == "<br/" || buf == "<br " || buf == "<br /" => {
                        self.state = InlineState::PendingTag { buf };
                    }
                    _ => { self.push_text(&buf); self.state = InlineState::Normal; }
                }
            }
        }
    }

    /// Return completed spans without destroying in-progress state.
    /// Safe to call between tokens during streaming.
    fn flush_completed(&mut self) -> Vec<Span> {
        self.flush_segment();
        std::mem::take(&mut self.spans)
    }

    /// Drain all state — recovers unclosed constructs as literal text.
    /// Only call at end-of-turn.
    fn drain(&mut self) -> Vec<Span> {
        // Resolve pending delimiters against the format stack first,
        // so that e.g. `**bold**` at end-of-turn properly closes the Bold
        // instead of dumping `**` as literal text.
        let state = std::mem::replace(&mut self.state, InlineState::Normal);
        match state {
            InlineState::PendingStars { count } => {
                self.resolve_stars(count);
            }
            InlineState::PendingTildes { count } => {
                self.resolve_tildes(count);
            }
            InlineState::InlineCode { buf } => {
                self.segment.push('`');
                self.segment.push_str(&buf);
            }
            InlineState::PendingTag { buf } => {
                self.segment.push_str(&buf);
            }
            InlineState::Normal => {}
        }
        // Unwind remaining format stack: recover each frame as literal text,
        // emitting outermost-first (source order) by reversing the pop order.
        let mut frames: Vec<FormatFrame> = std::iter::from_fn(|| self.formats.pop()).collect();
        for frame in frames.iter_mut().rev() {
            let text = std::mem::take(&mut frame.buf);
            self.segment.push_str(frame.kind.delim_open());
            self.segment.push_str(&text);
        }
        self.flush_segment();
        std::mem::take(&mut self.spans)
    }

    fn finish(mut self) -> Vec<Span> {
        self.drain()
    }
}

// ── Cell-level parser (uses InlineParser) ──────────────────────────────────

struct CellParser {
    inline: InlineParser,
}

enum CellFeed {
    Continue,
    CellEnd,
    RowEnd,
}

impl CellParser {
    fn new() -> Self {
        Self { inline: InlineParser::new() }
    }

    fn feed(&mut self, c: char) -> CellFeed {
        match c {
            '|' => {
                self.inline.flush_segment();
                return CellFeed::CellEnd;
            }
            '\n' => {
                self.inline.flush_segment();
                return CellFeed::RowEnd;
            }
            _ => {
                self.inline.feed(c);
                CellFeed::Continue
            }
        }
    }

    fn finish(self) -> Vec<Span> {
        self.inline.finish()
    }
}

// ── Top-level state machine ────────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Alignment {
    Left,
    Center,
    Right,
}

enum State {
    Normal,
    /// N backticks seen; waiting for the next char to decide their meaning.
    PendingBackticks { count: usize, at_line_start: bool },
    /// Inside the lang tag after an opening fence; consumed silently.
    FenceLang { fence_len: usize },
    /// Streaming code block. `close_cand` holds potential closing fence backticks.
    CodeBlock { fence_len: usize, close_cand: usize },
    PendingHeader { level: u8 },
    Header { level: u8, buf: String },
    /// 1+ dashes seen at line start; waiting for more or a newline.
    PendingHR { count: usize },
    /// Collecting the first `|`-initiated row; waiting for a separator to confirm.
    PendingTable { cells: Vec<Vec<Span>>, cell: CellParser },
    /// First row done; buffering the potential separator line (`|---|---|`).
    PendingTableSep { header_cells: Vec<Vec<Span>>, sep_buf: String },
    /// Confirmed table. All rows are buffered; rendered in one pass when the
    /// table ends so column widths can be computed from actual content.
    Table {
        alignments: Vec<Alignment>,
        header: Vec<Vec<Span>>,
        rows: Vec<Vec<Vec<Span>>>,
        current_row: Vec<Vec<Span>>,
        cell: CellParser,
    },
}

impl MarkdownEmitter {
    pub fn new() -> Self {
        Self { state: State::Normal, line_start: true, inline: InlineParser::new(), segment: String::new(), term_width: 80 }
    }

    /// Feed the next token. May call `Terminal::append` zero or more times.
    pub fn push(&mut self, token: &str, emit: &mut impl FnMut(&[Span]) -> io::Result<()>, term_width: usize) -> io::Result<()> {
        self.term_width = term_width;
        for c in token.chars() {
            self.step(c, emit)?;
        }
        self.flush_inline(emit)
    }

    /// Emit all buffered content as plain text and call `Terminal::flush_append`.
    /// Call at end-of-turn so incomplete constructs don't disappear.
    pub fn flush(&mut self, emit: &mut impl FnMut(&[Span]) -> io::Result<()>, term_width: usize) -> io::Result<()> {
        self.term_width = term_width;
        let state = std::mem::replace(&mut self.state, State::Normal);
        match state {
            State::PendingBackticks { count, .. } => {
                self.segment.push_str(&"`".repeat(count));
            }
            State::FenceLang { .. } => {}
            State::CodeBlock { close_cand, .. } => {
                if close_cand > 0 {
                    self.segment.push_str(&"`".repeat(close_cand));
                }
                let s = std::mem::take(&mut self.segment);
                if !s.is_empty() {
                    emit(&[Span::styled(s, code_style())])?;
                }
                return Ok(());
            }
            State::PendingHeader { level } => {
                self.segment.push_str(&"#".repeat(level as usize));
            }
            State::Header { buf, .. } => {
                self.segment.push_str(&buf);
            }
            State::PendingHR { count } => {
                self.segment.push_str(&"-".repeat(count));
            }
            State::PendingTable { cells, cell } => {
                self.segment.push('|');
                for cell_spans in &cells {
                    for span in cell_spans { self.segment.push_str(&span.content); }
                    self.segment.push('|');
                }
                for span in &cell.finish() { self.segment.push_str(&span.content); }
            }
            State::PendingTableSep { header_cells, sep_buf } => {
                self.segment.push('|');
                for cell_spans in &header_cells {
                    for span in cell_spans { self.segment.push_str(&span.content); }
                    self.segment.push('|');
                }
                self.segment.push('\n');
                self.segment.push_str(&sep_buf);
            }
            State::Table { alignments, header, mut rows, current_row, cell } => {
                let mut last_row = current_row;
                let finished = trim_cell_spans(cell.finish());
                if !finished.is_empty() { last_row.push(finished); }
                if !last_row.is_empty() { rows.push(last_row); }
                render_buffered_table(&header, &rows, &alignments, emit, term_width)?;
                return Ok(());
            }
            State::Normal => {
                let spans = self.inline.drain();
                for s in &spans {
                    emit(&[s.clone()])?;
                }
            }
        }
        let s = std::mem::take(&mut self.segment);
        if !s.is_empty() {
            emit(&[Span::raw(s)])?;
        }
        Ok(())
    }

    // ── segment helpers ────────────────────────────────────────────────────

    fn flush_segment(&mut self, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        if self.segment.is_empty() {
            return Ok(());
        }
        let s = std::mem::take(&mut self.segment);
        emit(&[Span::raw(s)])
    }

    fn flush_code_segment(&mut self, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        if self.segment.is_empty() {
            return Ok(());
        }
        let s = std::mem::take(&mut self.segment);
        emit(&[Span::styled(s, code_style())])
    }

    fn flush_inline(&mut self, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        let spans = self.inline.flush_completed();
        for s in &spans {
            emit(&[s.clone()])?;
        }
        Ok(())
    }

    // ── main dispatch ──────────────────────────────────────────────────────

    fn step(&mut self, c: char, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        let state = std::mem::replace(&mut self.state, State::Normal);
        match state {
            State::Normal => self.step_normal(c, emit),
            State::PendingBackticks { count, at_line_start } => {
                self.step_pending_backticks(c, count, at_line_start, emit)
            }
            State::FenceLang { fence_len } => { self.step_fence_lang(c, fence_len); Ok(()) }
            State::CodeBlock { fence_len, close_cand } => {
                self.step_code_block(c, fence_len, close_cand, emit)
            }
            State::PendingHeader { level } => self.step_pending_header(c, level, emit),
            State::Header { level, buf } => self.step_header(c, level, buf, emit),
            State::PendingHR { count } => self.step_pending_hr(c, count, emit),
            State::PendingTable { cells, cell } => self.step_pending_table(c, cells, cell),
            State::PendingTableSep { header_cells, sep_buf } => {
                self.step_pending_table_sep(c, header_cells, sep_buf, emit)
            }
            State::Table { alignments, header, rows, current_row, cell } => {
                self.step_table(c, alignments, header, rows, current_row, cell, emit)
            }
        }
    }

    // ── per-state step handlers ────────────────────────────────────────────

    fn step_normal(&mut self, c: char, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        match c {
                '#' if self.line_start => {
                    self.flush_inline(emit)?;
                    self.state = State::PendingHeader { level: 1 };
                }
                '`' if self.line_start => {
                    self.flush_inline(emit)?;
                    self.state = State::PendingBackticks { count: 1, at_line_start: true };
                }
                '*' | '_' => {
                    self.inline.feed(c);
                    self.line_start = false;
                }
                '-' if self.line_start => {
                    self.flush_inline(emit)?;
                    self.state = State::PendingHR { count: 1 };
                }
                '|' if self.line_start => {
                    self.flush_inline(emit)?;
                    self.state = State::PendingTable { cells: Vec::new(), cell: CellParser::new() };
                    self.line_start = false;
                }
                '<' => {
                    self.inline.feed(c);
                    self.line_start = false;
                }
                '\n' => {
                    self.inline.feed('\n');
                    self.flush_inline(emit)?;
                    self.line_start = true;
                }
            _ => {
                self.inline.feed(c);
                self.line_start = false;
            }
        }
        Ok(())
    }

    fn step_pending_backticks(
        &mut self,
        c: char,
        count: usize,
        at_line_start: bool,
        emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
    ) -> io::Result<()> {
        if c == '`' {
            self.state = State::PendingBackticks { count: count + 1, at_line_start };
            return Ok(());
        }
        if at_line_start && count >= 3 {
            if c == '\n' {
                self.state = State::CodeBlock { fence_len: count, close_cand: 0 };
                self.line_start = true;
            } else {
                self.state = State::FenceLang { fence_len: count };
                self.step_fence_lang(c, count);
            }
        } else if count == 1 && c != '\n' {
            self.inline.feed('`');
            self.inline.feed(c);
            self.state = State::Normal;
            self.line_start = false;
        } else {
            self.segment.push_str(&"`".repeat(count));
            self.flush_segment(emit)?;
            self.state = State::Normal;
            self.step(c, emit)?;
        }
        Ok(())
    }

    fn step_fence_lang(&mut self, c: char, fence_len: usize) {
        if c == '\n' {
            self.state = State::CodeBlock { fence_len, close_cand: 0 };
            self.line_start = true;
        } else {
            self.state = State::FenceLang { fence_len };
        }
    }

    fn step_code_block(
        &mut self,
        c: char,
        fence_len: usize,
        close_cand: usize,
        emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
    ) -> io::Result<()> {
        match c {
            '`' if self.line_start || close_cand > 0 => {
                self.state = State::CodeBlock { fence_len, close_cand: close_cand + 1 };
            }
            '\n' if close_cand >= fence_len => {
                self.flush_code_segment(emit)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            '\n' if close_cand > 0 => {
                self.segment.push_str(&"`".repeat(close_cand));
                self.segment.push('\n');
                self.flush_code_segment(emit)?;
                self.state = State::CodeBlock { fence_len, close_cand: 0 };
                self.line_start = true;
            }
            '\n' => {
                self.segment.push('\n');
                self.flush_code_segment(emit)?;
                self.state = State::CodeBlock { fence_len, close_cand: 0 };
                self.line_start = true;
            }
            _ if close_cand > 0 => {
                self.segment.push_str(&"`".repeat(close_cand));
                self.segment.push(c);
                self.state = State::CodeBlock { fence_len, close_cand: 0 };
                self.line_start = false;
            }
            _ => {
                self.segment.push(c);
                self.state = State::CodeBlock { fence_len, close_cand: 0 };
                self.line_start = false;
            }
        }
        Ok(())
    }



    fn step_pending_header(&mut self, c: char, level: u8, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        match c {
            '#' if level < 6 => {
                self.state = State::PendingHeader { level: level + 1 };
            }
            ' ' => {
                self.state = State::Header { level, buf: String::new() };
            }
            '\n' => {
                self.segment.push_str(&"#".repeat(level as usize));
                self.segment.push('\n');
                self.flush_segment(emit)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => {
                self.segment.push_str(&"#".repeat(level as usize));
                self.flush_segment(emit)?;
                self.state = State::Normal;
                self.step(c, emit)?;
            }
        }
        Ok(())
    }

    fn step_header(
        &mut self,
        c: char,
        level: u8,
        mut buf: String,
        emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
    ) -> io::Result<()> {
        match c {
            '\n' => {
                emit(&[Span::styled(buf, header_style(level)), Span::raw("\n")])?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => {
                buf.push(c);
                self.state = State::Header { level, buf };
            }
        }
        Ok(())
    }

    fn step_pending_hr(&mut self, c: char, count: usize, emit: &mut impl FnMut(&[Span]) -> io::Result<()>) -> io::Result<()> {
        match c {
            '-' => {
                self.state = State::PendingHR { count: count + 1 };
            }
            '\n' => {
                if count >= 3 {
                    emit(&[Span::styled("─".repeat(self.term_width), table_sep_style())])?;
                } else {
                    self.segment.push_str(&"-".repeat(count));
                    self.segment.push('\n');
                    self.flush_segment(emit)?;
                }
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => {
                self.segment.push_str(&"-".repeat(count));
                self.flush_segment(emit)?;
                self.state = State::Normal;
                return self.step(c, emit);
            }
        }
        Ok(())
    }

    fn step_pending_table(
        &mut self,
        c: char,
        mut cells: Vec<Vec<Span>>,
        mut cell: CellParser,
    ) -> io::Result<()> {
        match cell.feed(c) {
            CellFeed::Continue => {
                self.state = State::PendingTable { cells, cell };
            }
            CellFeed::CellEnd => {
                let spans = trim_cell_spans(cell.finish());
                if !spans.is_empty() { cells.push(spans); }
                self.state = State::PendingTable { cells, cell: CellParser::new() };
            }
            CellFeed::RowEnd => {
                let spans = trim_cell_spans(cell.finish());
                if !spans.is_empty() { cells.push(spans); }
                self.state = State::PendingTableSep { header_cells: cells, sep_buf: String::new() };
                self.line_start = true;
            }
        }
        Ok(())
    }

    fn step_pending_table_sep(
        &mut self,
        c: char,
        header_cells: Vec<Vec<Span>>,
        mut sep_buf: String,
        emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
    ) -> io::Result<()> {
        if c != '\n' {
            sep_buf.push(c);
            self.state = State::PendingTableSep { header_cells, sep_buf };
            return Ok(());
        }

        if let Some(alignments) = parse_separator(&sep_buf) {
            self.state = State::Table {
                alignments,
                header: header_cells,
                rows: Vec::new(),
                current_row: Vec::new(),
                cell: CellParser::new(),
            };
        } else {
            self.segment.push('|');
            for cell_spans in &header_cells {
                for span in cell_spans { self.segment.push_str(&span.content); }
                self.segment.push('|');
            }
            self.segment.push('\n');
            self.segment.push_str(&sep_buf);
            self.segment.push('\n');
            self.flush_segment(emit)?;
            self.state = State::Normal;
        }
        self.line_start = true;
        Ok(())
    }

    fn step_table(
        &mut self,
        c: char,
        alignments: Vec<Alignment>,
        header: Vec<Vec<Span>>,
        mut rows: Vec<Vec<Vec<Span>>>,
        mut current_row: Vec<Vec<Span>>,
        mut cell: CellParser,
        emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
    ) -> io::Result<()> {
        // First char of a new row determines whether the table continues.
        if self.line_start {
            if c == '|' {
                self.line_start = false;
                self.state = State::Table { alignments, header, rows, current_row, cell };
                return Ok(());
            }
            if c == '\n' {
                // Blank line: table is complete — render it now.
                render_buffered_table(&header, &rows, &alignments, emit, self.term_width)?;
                self.state = State::Normal;
                self.segment.push('\n');
                return Ok(());
            }
            // Non-pipe, non-blank: multi-line cell continuation.
            if let Some(last_row) = rows.last_mut() {
                if let Some(last_cell) = last_row.last_mut() {
                    last_cell.push(Span::raw("\n".to_string()));
                }
            }
            self.line_start = false;
            let mut new_cell = CellParser::new();
            new_cell.feed(c);
            self.state = State::Table { alignments, header, rows, current_row: Vec::new(), cell: new_cell };
            return Ok(());
        }

        match cell.feed(c) {
            CellFeed::Continue => {
                self.state = State::Table { alignments, header, rows, current_row, cell };
            }
            CellFeed::CellEnd => {
                let spans = trim_cell_spans(cell.finish());
                if !spans.is_empty() { current_row.push(spans); }
                self.state = State::Table {
                    alignments, header, rows, current_row, cell: CellParser::new(),
                };
            }
            CellFeed::RowEnd => {
                let spans = trim_cell_spans(cell.finish());
                if !spans.is_empty() { current_row.push(spans); }
                if !current_row.is_empty() { rows.push(current_row); }
                self.state = State::Table {
                    alignments, header, rows,
                    current_row: Vec::new(),
                    cell: CellParser::new(),
                };
                self.line_start = true;
            }
        }
        Ok(())
    }
}

impl Default for MarkdownEmitter {
    fn default() -> Self {
        Self::new()
    }
}

// ── Table rendering ────────────────────────────────────────────────────────

fn render_buffered_table(
    header: &[Vec<Span>],
    rows: &[Vec<Vec<Span>>],
    _alignments: &[Alignment],
emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
    term_width: usize,
) -> io::Result<()> {
    let col_count = header.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return Ok(());
    }
    let empty: Vec<Span> = Vec::new();

    let natural: Vec<usize> = (0..col_count)
        .map(|i| {
            let h = cell_char_len(header.get(i).unwrap_or(&empty));
            let d = rows
                .iter()
                .map(|row| cell_char_len(row.get(i).unwrap_or(&empty)))
                .max()
                .unwrap_or(0);
            h.max(d).max(1)
        })
        .collect();

    let overhead = 3 * col_count + 1;
    let available = term_width.saturating_sub(overhead);
    let col_widths = distribute_widths(&natural, available);

    let sep = table_sep_style();
    render_border_row(&col_widths, '┌', '─', '┬', '┐', sep, emit)?;
    render_data_row(header, &col_widths, true, sep, emit)?;
    render_border_row(&col_widths, '├', '─', '┼', '┤', sep, emit)?;
    for (i, row) in rows.iter().enumerate() {
        render_data_row(row, &col_widths, false, sep, emit)?;
        if i + 1 < rows.len() {
            render_border_row(&col_widths, '├', '─', '┼', '┤', sep, emit)?;
        }
    }
    render_border_row(&col_widths, '└', '─', '┴', '┘', sep, emit)
}

fn render_border_row(
    col_widths: &[usize],
    left: char,
    fill: char,
    mid: char,
    right: char,
    style: Style,
    emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
) -> io::Result<()> {
    let mut line = String::new();
    line.push(left);
    for (i, &w) in col_widths.iter().enumerate() {
        for _ in 0..w + 2 { line.push(fill); } // +2 for the spaces either side
        if i + 1 < col_widths.len() { line.push(mid); }
    }
    line.push(right);
    line.push('\n');
    emit(&[Span::styled(line, style)])
}

fn render_data_row(
    cells: &[Vec<Span>],
    col_widths: &[usize],
    is_header: bool,
    sep_style: Style,
    emit: &mut impl FnMut(&[Span]) -> io::Result<()>,
) -> io::Result<()> {
    let empty: Vec<Span> = Vec::new();
    let col_count = col_widths.len();

    let wrapped: Vec<Vec<Vec<Span>>> = (0..col_count)
        .map(|i| wrap_cell(cells.get(i).unwrap_or(&empty), col_widths[i]))
        .collect();

    let row_height = wrapped.iter().map(|l| l.len()).max().unwrap_or(1);

    for line_idx in 0..row_height {
        for col in 0..col_count {
            emit(&[Span::styled("│ ", sep_style)])?;
            let line = wrapped[col].get(line_idx).cloned().unwrap_or_default();
            let actual_len = cell_char_len(&line);
            if is_header {
                for span in &line {
                    let style = span.style.add_modifier(Modifier::BOLD);
                    emit(&[Span::styled(span.content.as_ref().to_string(), style)])?;
                }
            } else {
                emit(&line)?;
            }
            let pad = col_widths[col].saturating_sub(actual_len);
            if pad > 0 {
                emit(&[Span::raw(" ".repeat(pad))])?;
            }
            emit(&[Span::raw(" ")])?;
        }
        emit(&[Span::styled("│\n", sep_style)])?;
    }
    Ok(())
}

fn distribute_widths(natural: &[usize], available: usize) -> Vec<usize> {
    let total_natural: usize = natural.iter().sum();
    if total_natural <= available {
        return natural.to_vec();
    }

    let n = natural.len();
    let mut widths: Vec<usize> = (0..n)
        .map(|i| 1.max(natural[i] * available / total_natural))
        .collect();

    loop {
        let total: usize = widths.iter().sum();
        if total <= available {
            break;
        }

        let mut excess = total - available;
        let mut idx: Vec<usize> = (0..n).filter(|&i| widths[i] > 1).collect();
        if idx.is_empty() {
            break;
        }
        idx.sort_by(|&a, &b| {
            let ratio_a = widths[a] as f64 / natural[a].max(1) as f64;
            let ratio_b = widths[b] as f64 / natural[b].max(1) as f64;
            ratio_b.partial_cmp(&ratio_a).unwrap()
        });

        for &i in &idx {
            if excess == 0 {
                break;
            }
            widths[i] -= 1;
            excess -= 1;
        }
    }

    widths
}

fn wrap_cell(spans: &[Span], max_width: usize) -> Vec<Vec<Span>> {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    let hard_lines: Vec<&str> = text.split('\n').collect();

    // Single hard line that fits — keep original styled spans
    if hard_lines.len() == 1 && text.width() <= max_width {
        return vec![spans.to_vec()];
    }

    let mut result: Vec<Vec<Span>> = Vec::new();
    for hard_line in &hard_lines {
        if hard_line.is_empty() {
            result.push(vec![Span::raw(String::new())]);
            continue;
        }
        if hard_line.width() <= max_width {
            result.push(vec![Span::raw(hard_line.to_string())]);
            continue;
        }
        let words: Vec<&str> = hard_line.split(' ').collect();
        let mut current = String::new();
        for word in &words {
            if current.is_empty() {
                current = word.to_string();
            } else if current.width() + 1 + word.width() <= max_width {
                current.push(' ');
                current.push_str(word);
            } else {
                result.push(vec![Span::raw(current)]);
                current = word.to_string();
            }
        }
        if !current.is_empty() {
            result.push(vec![Span::raw(current)]);
        }
    }

    if result.is_empty() {
        result.push(vec![Span::raw(String::new())]);
    }

    result
}

// ── Separator parsing and cell utilities ──────────────────────────────────

fn parse_separator(sep: &str) -> Option<Vec<Alignment>> {
    let s = sep.trim();
    if !s.starts_with('|') {
        return None;
    }
    let cols: Vec<&str> = s.split('|').skip(1).filter(|c| !c.trim().is_empty()).collect();
    if cols.is_empty() {
        return None;
    }
    let mut alignments = Vec::new();
    for col in &cols {
        let c = col.trim();
        if c.is_empty() || !c.chars().all(|ch| matches!(ch, '-' | ':')) || !c.contains('-') {
            return None;
        }
        let align = if c.starts_with(':') && c.ends_with(':') {
            Alignment::Center
        } else if c.ends_with(':') {
            Alignment::Right
        } else {
            Alignment::Left
        };
        alignments.push(align);
    }
    Some(alignments)
}

fn trim_cell_spans(mut spans: Vec<Span>) -> Vec<Span> {
    if let Some(first) = spans.first_mut() {
        let trimmed = first.content.trim_start().to_string();
        first.content = trimmed.into();
    }
    if let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_string();
        last.content = trimmed.into();
    }
    spans.retain(|s| !s.content.is_empty());
    spans
}

fn cell_char_len(spans: &[Span]) -> usize {
    let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
    text.split('\n').map(|l| l.width()).max().unwrap_or(0)
}

// ── Style helpers ──────────────────────────────────────────────────────────

fn code_style() -> Style {
    Style::new().fg(Color::DarkGray)
}

fn inline_code_style() -> Style {
    Style::new().fg(Color::Cyan)
}

fn header_style(level: u8) -> Style {
    let s = Style::new().fg(Color::White);
    if level <= 2 { s.add_modifier(Modifier::BOLD) } else { s }
}

fn bold_style() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn italic_style() -> Style {
    Style::new().add_modifier(Modifier::ITALIC)
}

fn strikethrough_style() -> Style {
    Style::new().add_modifier(Modifier::CROSSED_OUT)
}

fn table_sep_style() -> Style {
    Style::new().fg(Color::DarkGray)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect_spans(input: &str, term_width: usize) -> Vec<Span> {
        let mut md = MarkdownEmitter::new();
        let mut spans = Vec::new();
        md.push(input, &mut |s| { spans.extend_from_slice(s); Ok(()) }, term_width).unwrap();
        md.flush(&mut |s| { spans.extend_from_slice(s); Ok(()) }, term_width).unwrap();
        spans
    }

    #[test]
    fn test_br_tag_newline() {
        let spans = collect_spans("a<br>b", 80);
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::raw("\n"),
            Span::raw("b"),
        ]);
    }

    #[test]
    fn test_br_slash_tag_newline() {
        let spans = collect_spans("a<br/>b", 80);
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::raw("\n"),
            Span::raw("b"),
        ]);
    }

    #[test]
    fn test_br_space_slash_tag_newline() {
        let spans = collect_spans("a<br />b", 80);
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::raw("\n"),
            Span::raw("b"),
        ]);
    }

    #[test]
    fn test_incomplete_br_flushed() {
        let spans = collect_spans("a<br", 80);
        // `<` at the start opens a tag; `<br` stays as PendingTag; flush emits `<br`.
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::raw("<br"),
        ]);
    }

    #[test]
    fn test_non_br_tag_preserved() {
        let spans = collect_spans("a<div>b</div>c", 80);
        // `<` triggers tag parsing; `<d` doesn't match br prefix → flushed as literal.
        // `>` resumes normal; `</` again triggers tag parsing, fails to match, flushed.
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::raw("<div>b"),
            Span::raw("</div>c"),
        ]);
    }

    #[test]
    fn test_br_at_end_of_input() {
        let spans = collect_spans("a<br>", 80);
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_br_in_table_cell() {
        let spans = collect_spans("| a<br>b | c |\n| --- | --- |\n| d | e |\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("a"), "text: {:?}", text);
        assert!(text.contains("b"), "text: {:?}", text);
        let a_pos = text.find('a').unwrap();
        let b_pos = text.find('b').unwrap();
        assert!(text[a_pos..b_pos].contains('\n'), "no newline between a and b in: {:?}", text);
    }

    #[test]
    fn test_multiline_cell_continuation() {
        let spans = collect_spans(
            "| A | B |\n| --- | --- |\n| x | y\nz | w |\n",
            80,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains('x'), "text: {:?}", text);
        assert!(text.contains('y'), "text: {:?}", text);
        assert!(text.contains('z'), "text: {:?}", text);
        assert!(text.contains('w'), "text: {:?}", text);
        let x_pos = text.find('x').unwrap();
        let z_pos = text.find('z').unwrap();
        assert!(text[x_pos..z_pos].contains('\n'), "no line break between x and z in: {:?}", text);
    }

    #[test]
    fn test_underscore_italic() {
        let spans = collect_spans("_italic_", 80);
        assert_eq!(spans, vec![Span::styled("italic", italic_style())]);
    }

    #[test]
    fn test_double_underscore_bold() {
        let spans = collect_spans("__bold__", 80);
        assert_eq!(spans, vec![Span::styled("bold", bold_style())]);
    }

    #[test]
    fn test_horizontal_rule() {
        let spans = collect_spans("---\n", 80);
        assert_eq!(spans.len(), 1);
        assert!(spans[0].content.as_ref().chars().all(|c| c == '─'));
        assert_eq!(spans[0].content.as_ref().chars().count(), 80);
    }

    #[test]
    fn test_short_dash_not_hr() {
        let spans = collect_spans("--\n", 80);
        assert_eq!(spans, vec![Span::raw("--\n")]);
    }

    #[test]
    fn test_strikethrough() {
        let spans = collect_spans("~~deleted~~", 80);
        assert_eq!(spans, vec![Span::styled("deleted", strikethrough_style())]);
    }

    #[test]
    fn test_single_tilde_not_strikethrough() {
        let spans = collect_spans("~not~", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "~not~");
    }

    // ── Headers ─────────────────────────────────────────────────────────────

    #[test]
    fn test_h1_header() {
        let spans = collect_spans("# Hello\n", 80);
        assert_eq!(spans, vec![
            Span::styled("Hello", header_style(1)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_h6_header() {
        let spans = collect_spans("###### Hello\n", 80);
        assert_eq!(spans, vec![
            Span::styled("Hello", header_style(6)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_h2_header() {
        let spans = collect_spans("## Hello\n", 80);
        assert_eq!(spans, vec![
            Span::styled("Hello", header_style(2)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_h3_header() {
        let spans = collect_spans("### Hello\n", 80);
        assert_eq!(spans, vec![
            Span::styled("Hello", header_style(3)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_h4_header() {
        let spans = collect_spans("#### Hello\n", 80);
        assert_eq!(spans, vec![
            Span::styled("Hello", header_style(4)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_h5_header() {
        let spans = collect_spans("##### Hello\n", 80);
        assert_eq!(spans, vec![
            Span::styled("Hello", header_style(5)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_headers_in_sequence() {
        let spans = collect_spans("# One\n## Two\n### Three\n", 80);
        assert_eq!(spans, vec![
            Span::styled("One", header_style(1)),
            Span::raw("\n"),
            Span::styled("Two", header_style(2)),
            Span::raw("\n"),
            Span::styled("Three", header_style(3)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_header_no_space_not_a_header() {
        let spans = collect_spans("#NoSpace\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "#NoSpace\n");
    }

    #[test]
    fn test_header_empty() {
        let spans = collect_spans("#\n", 80);
        assert_eq!(spans, vec![Span::raw("#\n")]);
    }

    #[test]
    fn test_header_excessive_hashes() {
        let spans = collect_spans("#######\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "#######\n");
    }

    // ── Bold / Italic with asterisks ────────────────────────────────────────

    #[test]
    fn test_asterisk_bold() {
        let spans = collect_spans("**bold**", 80);
        assert_eq!(spans, vec![Span::styled("bold", bold_style())]);
    }

    #[test]
    fn test_asterisk_italic() {
        let spans = collect_spans("*italic*", 80);
        assert_eq!(spans, vec![Span::styled("italic", italic_style())]);
    }

    #[test]
    fn test_mixed_star_underscore_bold() {
        let spans = collect_spans("_*bold*_", 80);
        assert_eq!(spans, vec![Span::styled("bold", bold_style())]);
    }

    // ── Inline code ─────────────────────────────────────────────────────────

    #[test]
    fn test_inline_code() {
        let spans = collect_spans("`code`", 80);
        assert_eq!(spans, vec![Span::styled("code", inline_code_style())]);
    }

    #[test]
    fn test_inline_code_with_spaces() {
        let spans = collect_spans("`code with spaces`", 80);
        assert_eq!(spans, vec![Span::styled("code with spaces", inline_code_style())]);
    }

    // ── Fenced code blocks ──────────────────────────────────────────────────

    #[test]
    fn test_fenced_code_block() {
        let spans = collect_spans("```\ncode\n```\n", 80);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "code\n");
        assert!(spans[0].style == code_style(), "expected code style, got {:?}", spans[0].style);
    }

    #[test]
    fn test_code_block_with_lang() {
        let spans = collect_spans("```rust\nfn main() {}\n```\n", 80);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "fn main() {}\n");
        assert!(spans[0].style == code_style(), "expected code style, got {:?}", spans[0].style);
    }

    #[test]
    fn test_nested_fences() {
        let spans = collect_spans("```\n``\n```\n", 80);
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].content.as_ref(), "``\n");
        assert!(spans[0].style == code_style(), "expected code style, got {:?}", spans[0].style);
    }

    #[test]
    fn test_unclosed_code_block() {
        let spans = collect_spans("```\ncode", 80);
        assert_eq!(spans, vec![Span::styled("code", code_style())]);
    }

    #[test]
    fn test_code_block_multi_line() {
        let spans = collect_spans("```\nline1\nline2\n```\n", 80);
        // Code block content streams line-by-line; each line is a separate styled span
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].content.as_ref(), "line1\n");
        assert_eq!(spans[1].content.as_ref(), "line2\n");
        assert!(spans[0].style == code_style(), "expected code style, got {:?}", spans[0].style);
    }

    // ── Horizontal rules ────────────────────────────────────────────────────

    #[test]
    fn test_dashes_with_text_not_hr() {
        let spans = collect_spans("---text\n", 80);
        // The `---` fallback is flushed as a separate span, then the text is an inline segment
        assert!(spans.len() >= 2);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "---text\n");
    }

    // ── Tables ──────────────────────────────────────────────────────────────

    #[test]
    fn test_basic_table() {
        let spans = collect_spans("| A | B |\n| --- | --- |\n| 1 | 2 |\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with('┌'), "expected table border, got: {:?}", text);
        assert!(text.contains("│ A │"), "expected header cell A, got: {:?}", text);
        assert!(text.contains("│ 1 │"), "expected data cell 1, got: {:?}", text);
        assert!(text.contains("└"), "expected bottom border, got: {:?}", text);
    }

    #[test]
    fn test_table_empty_cells() {
        let spans = collect_spans("| A |\n| --- |\n| |\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.starts_with('┌'), "expected table border, got: {:?}", text);
        assert!(text.contains('A'));
    }

    #[test]
    fn test_table_invalid_separator_fallback() {
        let spans = collect_spans("| A | B |\n| x | x |\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('┌'), "expected plain fallback, got: {:?}", text);
        assert!(text.contains('|'));
    }

    #[test]
    fn test_table_fallback_then_text_ordering() {
        let spans = collect_spans("| A | B |\n| x | x |\nhello\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        // Fallback content should appear BEFORE subsequent text
        let pipe_pos = text.find('|').unwrap();
        let hello_pos = text.find("hello").unwrap();
        assert!(pipe_pos < hello_pos, "fallback should come before text, got: {:?}", text);
        assert!(text.contains("|A|B|"), "expected pipe row, got: {:?}", text);
    }

    #[test]
    fn test_table_no_separator_fallback() {
        let spans = collect_spans("| A | B |\n| 1 | 2 |\n", 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(!text.contains('┌'), "expected plain fallback, got: {:?}", text);
        assert!(text.contains('|'));
    }

    // ── Unclosed constructs (flush behavior) ────────────────────────────────

    #[test]
    fn test_unclosed_bold_flushed() {
        let spans = collect_spans("**unclosed", 80);
        assert_eq!(spans, vec![Span::raw("**unclosed")]);
    }

    #[test]
    fn test_unclosed_italic_flushed() {
        let spans = collect_spans("*unclosed", 80);
        assert_eq!(spans, vec![Span::raw("*unclosed")]);
    }

    #[test]
    fn test_unclosed_inline_code_flushed() {
        let spans = collect_spans("`unclosed", 80);
        assert_eq!(spans, vec![Span::raw("`unclosed")]);
    }

    #[test]
    fn test_unclosed_strikethrough_flushed() {
        let spans = collect_spans("~~unclosed", 80);
        assert_eq!(spans, vec![Span::raw("~~unclosed")]);
    }

    // ── Edge cases ──────────────────────────────────────────────────────────

    #[test]
    fn test_mixed_inline_formatting() {
        let spans = collect_spans("**bold** and *italic* and `code`", 80);
        assert_eq!(spans, vec![
            Span::styled("bold", bold_style()),
            Span::raw(" and "),
            Span::styled("italic", italic_style()),
            Span::raw(" and "),
            Span::styled("code", inline_code_style()),
        ]);
    }

    #[test]
    fn test_empty_input() {
        let spans = collect_spans("", 80);
        assert!(spans.is_empty());
    }

    #[test]
    fn test_multiline_plain_text() {
        let spans = collect_spans("hello\nworld\n", 80);
        assert_eq!(spans, vec![
            Span::raw("hello\n"),
            Span::raw("world\n"),
        ]);
    }

    #[test]
    fn test_plain_text_no_trailing_newline() {
        let spans = collect_spans("hello", 80);
        assert_eq!(spans, vec![Span::raw("hello")]);
    }

    #[test]
    fn test_multiple_hrs() {
        let spans = collect_spans("---\n\n---\n", 80);
        let hr_count = spans.iter().filter(|s| s.content.contains('─')).count();
        assert_eq!(hr_count, 2, "should have 2 horizontal rules");
    }

    #[test]
    fn test_bold_at_line_start() {
        let spans = collect_spans("**bold** and more", 80);
        assert_eq!(spans, vec![
            Span::styled("bold", bold_style()),
            Span::raw(" and more"),
        ]);
    }

    #[test]
    fn test_underscores_mid_word_trigger_italic() {
        // Known behavior: _mid-word_ triggers italic (not GFM-compliant)
        let spans = collect_spans("a_b_", 80);
        assert_eq!(spans, vec![
            Span::raw("a"),
            Span::styled("b", italic_style()),
        ]);
    }

    #[test]
    fn test_header_bold_styling() {
        let h1 = collect_spans("# H1\n", 80);
        let h2 = collect_spans("## H2\n", 80);
        let h3 = collect_spans("### H3\n", 80);
        // header_style adds Attribute::Bold for level <= 2
        assert_eq!(h1[0].style, header_style(1), "H1 style mismatch");
        assert_eq!(h2[0].style, header_style(2), "H2 style mismatch");
        assert_eq!(h3[0].style, header_style(3), "H3 style mismatch");
    }

    #[test]
    fn test_header_with_trailing_hashes() {
        let spans = collect_spans("# H1 #\n", 80);
        assert_eq!(spans, vec![
            Span::styled("H1 #", header_style(1)),
            Span::raw("\n"),
        ]);
    }

    #[test]
    fn test_br_and_bold_combined() {
        let spans = collect_spans(
            "A <br> tag inserts a newline: line one\nline two<br />line three<br/>done.\n\nNow for a **code block** with a language tag:  ",
            80,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();

        // Each <br> variant inserts a newline
        assert!(text.contains("A \n tag"), "br should insert newline in: {:?}", text);
        assert!(text.contains("line two\nline three"), "br/ should insert newline in: {:?}", text);
        assert!(text.contains("line three\ndone"), "br/> should insert newline in: {:?}", text);

        // Blank line preserved
        assert!(text.contains("done.\n\nNow"), "blank line should be preserved in: {:?}", text);

        // Bold markers consumed, content present
        assert!(!text.contains("**code"), "bold markers should be consumed in: {:?}", text);
        assert!(text.contains("code block"), "bold content should be present in: {:?}", text);

        // Trailing whitespace preserved
        assert!(text.ends_with("  "), "trailing spaces should be preserved in: {:?}", text);
    }

    // ── Streaming / cross-token tests ───────────────────────────────────────

    fn collect_spans_streaming(tokens: &[&str], term_width: usize) -> Vec<Span> {
        let mut md = MarkdownEmitter::new();
        let mut spans = Vec::new();
        for token in tokens {
            md.push(token, &mut |s| { spans.extend_from_slice(s); Ok(()) }, term_width).unwrap();
        }
        md.flush(&mut |s| { spans.extend_from_slice(s); Ok(()) }, term_width).unwrap();
        spans
    }

    #[test]
    fn test_bold_across_tokens() {
        let spans = collect_spans_streaming(&["**bold", "**"], 80);
        assert_eq!(spans, vec![Span::styled("bold", bold_style())]);
    }

    #[test]
    fn test_italic_across_tokens() {
        let spans = collect_spans_streaming(&["*ita", "lic*"], 80);
        assert_eq!(spans, vec![Span::styled("italic", italic_style())]);
    }

    #[test]
    fn test_inline_code_across_tokens() {
        let spans = collect_spans_streaming(&["`co", "de`"], 80);
        assert_eq!(spans, vec![Span::styled("code", inline_code_style())]);
    }

    #[test]
    fn test_strikethrough_across_tokens() {
        let spans = collect_spans_streaming(&["~~dele", "ted~~"], 80);
        assert_eq!(spans, vec![Span::styled("deleted", strikethrough_style())]);
    }

    #[test]
    fn test_bold_and_italic_across_tokens() {
        let spans = collect_spans_streaming(
            &["**bold", " and *", "italic*", "**"],
            80,
        );
        // Nested italic inside bold: "bold and " is bold, "italic" is bold+italic.
        let bold_italic = Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC);
        assert_eq!(spans, vec![
            Span::styled("bold and ", bold_style()),
            Span::styled("italic", bold_italic),
        ]);
    }

    // ── Nested inline formatting ────────────────────────────────────────────

    fn bold_italic_style() -> Style {
        Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC)
    }

    fn italic_bold_style() -> Style {
        bold_italic_style()
    }

    fn bold_strikethrough_style() -> Style {
        Style::new().add_modifier(Modifier::BOLD | Modifier::CROSSED_OUT)
    }

    #[test]
    fn test_italic_inside_bold() {
        let spans = collect_spans("**bold *italic* text**", 80);
        assert_eq!(spans, vec![
            Span::styled("bold ", bold_style()),
            Span::styled("italic", bold_italic_style()),
            Span::styled(" text", bold_style()),
        ]);
    }

    #[test]
    fn test_bold_inside_italic() {
        let spans = collect_spans("*italic **bold** text*", 80);
        assert_eq!(spans, vec![
            Span::styled("italic ", italic_style()),
            Span::styled("bold", italic_bold_style()),
            Span::styled(" text", italic_style()),
        ]);
    }

    #[test]
    fn test_italic_inside_strikethrough() {
        let spans = collect_spans("~~strike *italic* text~~", 80);
        assert_eq!(spans, vec![
            Span::styled("strike ", strikethrough_style()),
            Span::styled("italic", Style::new().add_modifier(Modifier::CROSSED_OUT | Modifier::ITALIC)),
            Span::styled(" text", strikethrough_style()),
        ]);
    }

    #[test]
    fn test_bold_italic_combined_triple_stars() {
        let spans = collect_spans("***bold italic***", 80);
        let bold_italic = bold_italic_style();
        assert_eq!(spans, vec![
            Span::styled("bold italic", bold_italic),
        ]);
    }

    #[test]
    fn test_bold_strikethrough_nesting() {
        let spans = collect_spans("**bold ~~strike~~ text**", 80);
        assert_eq!(spans, vec![
            Span::styled("bold ", bold_style()),
            Span::styled("strike", bold_strikethrough_style()),
            Span::styled(" text", bold_style()),
        ]);
    }

    #[test]
    fn test_deep_nesting() {
        let spans = collect_spans("**bold *italic ~~strike~~* text**", 80);
        let bold_italic = bold_italic_style();
        let strike_inside = Style::new().add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::CROSSED_OUT);
        assert_eq!(spans, vec![
            Span::styled("bold ", bold_style()),
            Span::styled("italic ", bold_italic),
            Span::styled("strike", strike_inside),
            Span::styled(" text", bold_style()),
        ]);
    }

    #[test]
    fn test_nested_unclosed_at_end_of_turn() {
        // Bold opened but not closed at end-of-turn
        let spans = collect_spans("text **bold *italic*", 80);
        let bold_italic = bold_italic_style();
        // "italic" closes properly, then bold drains as literal at end
        assert_eq!(spans, vec![
            Span::raw("text "),
            Span::styled("bold ", bold_style()),
            Span::styled("italic", bold_italic),
            Span::raw("**"),
        ]);
    }

    // ── Bug-fix regression tests ──────────────────────────────────────────────

    #[test]
    fn test_bold_closes_through_unclosed_italic() {
        // ** must close Bold even when an unclosed * sits inside.
        // The stray * is recovered as a literal character within the bold span.
        let spans = collect_spans("**bold *text**", 80);
        assert_eq!(spans, vec![
            Span::styled("bold ", bold_style()),
            Span::styled("*text", bold_style()),
        ]);
    }

    #[test]
    fn test_inline_code_grandparent_style() {
        // Code inside bold inside strikethrough must inherit all ancestor attributes.
        let spans = collect_spans("~~**`code`**~~", 80);
        let expected = inline_code_style().add_modifier(Modifier::BOLD | Modifier::CROSSED_OUT);
        assert_eq!(spans, vec![Span::styled("code", expected)]);
    }

    #[test]
    fn test_drain_unclosed_source_order() {
        // Literal recovery for multiple unclosed formats must appear in source
        // order: the outermost opener (**) should precede the inner one (*b).
        let spans = collect_spans("**a *b", 80);
        assert_eq!(spans, vec![
            Span::styled("a ", bold_style()),
            Span::raw("***b"),
        ]);
    }

    #[test]
    fn test_html_tag_fallback_inside_bold() {
        // Unknown HTML tag text inside a bold span must be routed through the
        // format buffer so it renders bold, not as a plain interleaved span.
        let spans = collect_spans("**<q>text**", 80);
        assert_eq!(spans, vec![Span::styled("<q>text", bold_style())]);
    }

    #[test]
    fn test_thinking_tag_across_tokens() {
        // <think> isn't special to the markdown parser (thinking.rs handles it separately)
        // but ensure it doesn't break inline state
        let spans = collect_spans_streaming(
            &["text <thi", "nk>more text"],
            80,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "text <think>more text");
    }

    #[test]
    fn test_complex_streaming_with_br_and_bold() {
        let spans = collect_spans_streaming(
            &[
                "A <br> tag: **bo",
                "ld** and *it",
                "alic* and `co",
                "de`.",
            ],
            80,
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("A \n tag:"), "<br> not converted: {:?}", text);
        assert!(!text.contains("**bo"), "bold markers visible: {:?}", text);
        assert!(!text.contains("*it"), "italic markers visible: {:?}", text);
        assert!(!text.contains("`co"), "code markers visible: {:?}", text);
        assert!(text.contains("bold"), "bold content missing: {:?}", text);
        assert!(text.contains("italic"), "italic content missing: {:?}", text);
        assert!(text.contains("code"), "code content missing: {:?}", text);
    }

    // ── Comprehensive integration test ──────────────────────────────────────

    #[test]
    fn test_all_features_integration() {
        let input = "\
# H1

## H2

### H3

#### H4

##### H5

###### H6

Bold with **double asterisks**, also bold with __double underscores__.
Italic with *single asterisks*, also italic with _single underscores_.
_*Combined*_ via mixed star+underscore.

Inline code: `let x = 1;`

Strikethrough: ~~deleted~~

Line break: before<br>after<br/> and <br /> here.

Horizontal rule:

---

Fenced code block:
```
let code = true;
```

Table:
| Left | Center | Right |
| :--- | :---: | ---: |
| a | b | c |
";
        let spans = collect_spans(input, 80);
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();

        // All header levels present, markers consumed
        assert!(!text.contains("###### H6"), "H6 raw markers: {:?}", text);
        assert!(text.contains("H1"), "H1 missing");
        assert!(text.contains("H2"), "H2 missing");
        assert!(text.contains("H6"), "H6 missing");

        // Bold markers consumed
        assert!(!text.contains("**double"), "bold ** visible: {:?}", text);
        assert!(!text.contains("__double"), "bold __ visible: {:?}", text);
        assert!(text.contains("double asterisks"), "bold text missing: {:?}", text);

        // Italic markers consumed
        assert!(!text.contains("*single"), "italic * visible: {:?}", text);
        assert!(!text.contains("_single"), "italic _ visible: {:?}", text);

        // Combined consumed
        assert!(!text.contains("_*Combined"), "combined _* visible: {:?}", text);

        // Inline code markers consumed
        assert!(!text.contains("`let"), "inline code ` visible: {:?}", text);
        assert!(text.contains("let x = 1;"), "code content missing: {:?}", text);

        // Strikethrough markers consumed
        assert!(!text.contains("~~deleted"), "~~ markers visible: {:?}", text);
        assert!(text.contains("deleted"), "strikethrough content missing: {:?}", text);

        // <br> converted to newlines
        assert!(text.contains("before\nafter"), "<br> not converted: {:?}", text);

        // Horizontal rule — box-drawing char present
        assert!(text.contains('─'), "no horizontal rule: {:?}", text);

        // Code block content present, fence markers consumed
        assert!(!text.contains("```"), "code fence visible: {:?}", text);
        assert!(text.contains("let code = true;"), "code block content missing: {:?}", text);

        // Table renders with box-drawing characters
        assert!(text.contains('┌'), "no table border: {:?}", text);

        // HR count: 1 horizontal rule, plus table borders
        let hr_chars: usize = spans.iter().map(|s| s.content.chars().filter(|&c| c == '─').count()).sum();
        assert!(hr_chars > 0, "no ─ characters found");
    }

}
