use std::io;

use crossterm::style::{Attribute, Color, ContentStyle};

use crate::terminal::{Span, Terminal};

/// Streaming markdown renderer. Feed LLM tokens via `push`; call `flush` at
/// end-of-turn to drain any content held back waiting for a closing marker.
///
/// Plain text and code-block content stream through immediately (subject to
/// `Terminal`'s own flush interval). Only short inline constructs — bold,
/// italic, inline code — are buffered until their closing marker arrives.
/// Tables are fully buffered until the table ends so that column widths can
/// be computed from actual content before rendering.
pub struct MarkdownEmitter {
    state: State,
    line_start: bool,
    segment: String,
}

// ── Cell-level inline parser ───────────────────────────────────────────────

struct CellParser {
    state: CellState,
    spans: Vec<Span>,
    segment: String,
}

enum CellState {
    Normal,
    PendingStars { count: usize },
    Bold { buf: String },
    BoldClose { buf: String },
    Italic { buf: String },
    InlineCode { buf: String },
}

enum CellFeed {
    Continue,
    CellEnd, // hit '|'
    RowEnd,  // hit '\n'
}

impl CellParser {
    fn new() -> Self {
        Self { state: CellState::Normal, spans: Vec::new(), segment: String::new() }
    }

    fn flush_segment(&mut self) {
        if !self.segment.is_empty() {
            let s = std::mem::take(&mut self.segment);
            self.spans.push(Span::plain(s));
        }
    }

    fn push_span(&mut self, span: Span) {
        self.flush_segment();
        self.spans.push(span);
    }

    fn feed(&mut self, c: char) -> CellFeed {
        let state = std::mem::replace(&mut self.state, CellState::Normal);
        match state {
            CellState::Normal => match c {
                '|' => {
                    self.flush_segment();
                    return CellFeed::CellEnd;
                }
                '\n' => {
                    self.flush_segment();
                    return CellFeed::RowEnd;
                }
                '*' => {
                    self.flush_segment();
                    self.state = CellState::PendingStars { count: 1 };
                }
                '`' => {
                    self.flush_segment();
                    self.state = CellState::InlineCode { buf: String::new() };
                }
                _ => {
                    self.segment.push(c);
                    self.state = CellState::Normal;
                }
            },
            CellState::PendingStars { count } => match c {
                '*' => {
                    self.state = CellState::PendingStars { count: count + 1 };
                }
                '|' => {
                    self.segment.push_str(&"*".repeat(count));
                    self.flush_segment();
                    return CellFeed::CellEnd;
                }
                '\n' => {
                    self.segment.push_str(&"*".repeat(count));
                    self.flush_segment();
                    return CellFeed::RowEnd;
                }
                _ if count == 1 => {
                    self.state = CellState::Italic { buf: c.to_string() };
                }
                _ if count == 2 => {
                    self.state = CellState::Bold { buf: c.to_string() };
                }
                _ => {
                    self.segment.push_str(&"*".repeat(count));
                    self.segment.push(c);
                    self.state = CellState::Normal;
                }
            },
            CellState::Bold { mut buf } => match c {
                '*' => {
                    self.state = CellState::BoldClose { buf };
                }
                '|' => {
                    self.segment.push_str("**");
                    self.segment.push_str(&buf);
                    self.flush_segment();
                    return CellFeed::CellEnd;
                }
                '\n' => {
                    self.segment.push_str("**");
                    self.segment.push_str(&buf);
                    self.flush_segment();
                    return CellFeed::RowEnd;
                }
                _ => {
                    buf.push(c);
                    self.state = CellState::Bold { buf };
                }
            },
            CellState::BoldClose { buf } => match c {
                '*' => {
                    self.push_span(Span::styled(buf, bold_style()));
                    self.state = CellState::Normal;
                }
                '|' => {
                    self.segment.push_str("**");
                    self.segment.push_str(&buf);
                    self.segment.push('*');
                    self.flush_segment();
                    return CellFeed::CellEnd;
                }
                '\n' => {
                    self.segment.push_str("**");
                    self.segment.push_str(&buf);
                    self.segment.push('*');
                    self.flush_segment();
                    return CellFeed::RowEnd;
                }
                _ => {
                    let mut new_buf = buf;
                    new_buf.push('*');
                    new_buf.push(c);
                    self.state = CellState::Bold { buf: new_buf };
                }
            },
            CellState::Italic { mut buf } => match c {
                '*' => {
                    self.push_span(Span::styled(buf, italic_style()));
                    self.state = CellState::Normal;
                }
                '|' => {
                    self.segment.push('*');
                    self.segment.push_str(&buf);
                    self.flush_segment();
                    return CellFeed::CellEnd;
                }
                '\n' => {
                    self.segment.push('*');
                    self.segment.push_str(&buf);
                    self.flush_segment();
                    return CellFeed::RowEnd;
                }
                _ => {
                    buf.push(c);
                    self.state = CellState::Italic { buf };
                }
            },
            CellState::InlineCode { mut buf } => match c {
                '`' => {
                    self.push_span(Span::styled(buf, inline_code_style()));
                    self.state = CellState::Normal;
                }
                '|' => {
                    self.segment.push('`');
                    self.segment.push_str(&buf);
                    self.flush_segment();
                    return CellFeed::CellEnd;
                }
                '\n' => {
                    self.segment.push('`');
                    self.segment.push_str(&buf);
                    self.flush_segment();
                    return CellFeed::RowEnd;
                }
                _ => {
                    buf.push(c);
                    self.state = CellState::InlineCode { buf };
                }
            },
        }
        CellFeed::Continue
    }

    fn finish(mut self) -> Vec<Span> {
        let state = std::mem::replace(&mut self.state, CellState::Normal);
        match state {
            CellState::Normal => {}
            CellState::PendingStars { count } => {
                self.segment.push_str(&"*".repeat(count));
            }
            CellState::Bold { buf } | CellState::BoldClose { buf } => {
                self.segment.push_str("**");
                self.segment.push_str(&buf);
            }
            CellState::Italic { buf } => {
                self.segment.push('*');
                self.segment.push_str(&buf);
            }
            CellState::InlineCode { buf } => {
                self.segment.push('`');
                self.segment.push_str(&buf);
            }
        }
        self.flush_segment();
        self.spans
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
    InlineCode { buf: String },
    PendingHeader { level: u8 },
    Header { level: u8, buf: String },
    PendingStars { count: usize },
    Bold { buf: String },
    BoldClose { buf: String },
    Italic { buf: String },
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
        Self { state: State::Normal, line_start: true, segment: String::new() }
    }

    /// Feed the next token. May call `Terminal::append` zero or more times.
    pub fn push(&mut self, token: &str, term: &mut Terminal) -> io::Result<()> {
        for c in token.chars() {
            self.step(c, term)?;
        }
        self.flush_segment(term)
    }

    /// Emit all buffered content as plain text and call `Terminal::flush_append`.
    /// Call at end-of-turn so incomplete constructs don't disappear.
    pub fn flush(&mut self, term: &mut Terminal) -> io::Result<()> {
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
                    term.append(&[Span::styled(s, code_style())])?;
                }
                return term.flush_append();
            }
            State::InlineCode { buf } => {
                self.segment.push('`');
                self.segment.push_str(&buf);
            }
            State::PendingHeader { level } => {
                self.segment.push_str(&"#".repeat(level as usize));
            }
            State::Header { buf, .. } => {
                self.segment.push_str(&buf);
            }
            State::PendingStars { count } => {
                self.segment.push_str(&"*".repeat(count));
            }
            State::Bold { buf } | State::BoldClose { buf } => {
                self.segment.push_str("**");
                self.segment.push_str(&buf);
            }
            State::Italic { buf } => {
                self.segment.push('*');
                self.segment.push_str(&buf);
            }
            State::PendingTable { cells, cell } => {
                self.segment.push('|');
                for cell_spans in &cells {
                    for span in cell_spans { self.segment.push_str(&span.text); }
                    self.segment.push('|');
                }
                for span in &cell.finish() { self.segment.push_str(&span.text); }
            }
            State::PendingTableSep { header_cells, sep_buf } => {
                self.segment.push('|');
                for cell_spans in &header_cells {
                    for span in cell_spans { self.segment.push_str(&span.text); }
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
                render_buffered_table(&header, &rows, &alignments, term)?;
                return term.flush_append();
            }
            State::Normal => {}
        }
        let s = std::mem::take(&mut self.segment);
        if !s.is_empty() {
            term.append(&[Span::plain(s)])?;
        }
        term.flush_append()
    }

    // ── segment helpers ────────────────────────────────────────────────────

    fn flush_segment(&mut self, term: &mut Terminal) -> io::Result<()> {
        if self.segment.is_empty() {
            return Ok(());
        }
        let s = std::mem::take(&mut self.segment);
        let code = matches!(self.state, State::CodeBlock { .. });
        if code {
            term.append(&[Span::styled(s, code_style())])
        } else {
            term.append(&[Span::plain(s)])
        }
    }

    // ── main dispatch ──────────────────────────────────────────────────────

    fn step(&mut self, c: char, term: &mut Terminal) -> io::Result<()> {
        let state = std::mem::replace(&mut self.state, State::Normal);
        match state {
            State::Normal => self.step_normal(c, term),
            State::PendingBackticks { count, at_line_start } => {
                self.step_pending_backticks(c, count, at_line_start, term)
            }
            State::FenceLang { fence_len } => { self.step_fence_lang(c, fence_len); Ok(()) }
            State::CodeBlock { fence_len, close_cand } => {
                self.step_code_block(c, fence_len, close_cand, term)
            }
            State::InlineCode { buf } => self.step_inline_code(c, buf, term),
            State::PendingHeader { level } => self.step_pending_header(c, level, term),
            State::Header { level, buf } => self.step_header(c, level, buf, term),
            State::PendingStars { count } => self.step_pending_stars(c, count, term),
            State::Bold { buf } => self.step_bold(c, buf, term),
            State::BoldClose { buf } => self.step_bold_close(c, buf, term),
            State::Italic { buf } => self.step_italic(c, buf, term),
            State::PendingTable { cells, cell } => self.step_pending_table(c, cells, cell),
            State::PendingTableSep { header_cells, sep_buf } => {
                self.step_pending_table_sep(c, header_cells, sep_buf)
            }
            State::Table { alignments, header, rows, current_row, cell } => {
                self.step_table(c, alignments, header, rows, current_row, cell, term)
            }
        }
    }

    // ── per-state step handlers ────────────────────────────────────────────

    fn step_normal(&mut self, c: char, term: &mut Terminal) -> io::Result<()> {
        match c {
            '#' if self.line_start => {
                self.flush_segment(term)?;
                self.state = State::PendingHeader { level: 1 };
            }
            '`' => {
                self.flush_segment(term)?;
                self.state = State::PendingBackticks { count: 1, at_line_start: self.line_start };
            }
            '*' => {
                self.flush_segment(term)?;
                self.state = State::PendingStars { count: 1 };
            }
            '|' if self.line_start => {
                self.flush_segment(term)?;
                self.state = State::PendingTable { cells: Vec::new(), cell: CellParser::new() };
                self.line_start = false;
            }
            '\n' => {
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.line_start = true;
            }
            _ => {
                self.segment.push(c);
                self.line_start = false;
                self.state = State::Normal;
            }
        }
        Ok(())
    }

    fn step_pending_backticks(
        &mut self,
        c: char,
        count: usize,
        at_line_start: bool,
        term: &mut Terminal,
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
            let mut buf = String::new();
            buf.push(c);
            self.state = State::InlineCode { buf };
            self.line_start = false;
        } else {
            self.segment.push_str(&"`".repeat(count));
            self.state = State::Normal;
            self.step(c, term)?;
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
        term: &mut Terminal,
    ) -> io::Result<()> {
        match c {
            '`' if self.line_start || close_cand > 0 => {
                self.state = State::CodeBlock { fence_len, close_cand: close_cand + 1 };
            }
            '\n' if close_cand >= fence_len => {
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            '\n' if close_cand > 0 => {
                self.segment.push_str(&"`".repeat(close_cand));
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.state = State::CodeBlock { fence_len, close_cand: 0 };
                self.line_start = true;
            }
            '\n' => {
                self.segment.push('\n');
                self.flush_segment(term)?;
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

    fn step_inline_code(&mut self, c: char, mut buf: String, term: &mut Terminal) -> io::Result<()> {
        match c {
            '`' => {
                term.append(&[Span::styled(buf, inline_code_style())])?;
                self.state = State::Normal;
            }
            '\n' => {
                self.segment.push('`');
                self.segment.push_str(&buf);
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => {
                buf.push(c);
                self.state = State::InlineCode { buf };
            }
        }
        Ok(())
    }

    fn step_pending_header(&mut self, c: char, level: u8, term: &mut Terminal) -> io::Result<()> {
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
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => {
                self.segment.push_str(&"#".repeat(level as usize));
                self.state = State::Normal;
                self.step(c, term)?;
            }
        }
        Ok(())
    }

    fn step_header(
        &mut self,
        c: char,
        level: u8,
        mut buf: String,
        term: &mut Terminal,
    ) -> io::Result<()> {
        match c {
            '\n' => {
                term.append(&[Span::styled(buf, header_style(level)), Span::plain("\n")])?;
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

    fn step_pending_stars(&mut self, c: char, count: usize, term: &mut Terminal) -> io::Result<()> {
        match c {
            '*' => {
                self.state = State::PendingStars { count: count + 1 };
            }
            '\n' => {
                self.segment.push_str(&"*".repeat(count));
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ if count == 1 => {
                let mut buf = String::new();
                buf.push(c);
                self.state = State::Italic { buf };
                self.line_start = false;
            }
            _ if count == 2 => {
                let mut buf = String::new();
                buf.push(c);
                self.state = State::Bold { buf };
                self.line_start = false;
            }
            _ => {
                self.segment.push_str(&"*".repeat(count));
                self.state = State::Normal;
                self.step(c, term)?;
            }
        }
        Ok(())
    }

    fn step_bold(&mut self, c: char, mut buf: String, term: &mut Terminal) -> io::Result<()> {
        match c {
            '*' => { self.state = State::BoldClose { buf }; }
            '\n' => {
                self.segment.push_str("**");
                self.segment.push_str(&buf);
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => { buf.push(c); self.state = State::Bold { buf }; }
        }
        Ok(())
    }

    fn step_bold_close(&mut self, c: char, buf: String, term: &mut Terminal) -> io::Result<()> {
        match c {
            '*' => {
                term.append(&[Span::styled(buf, bold_style())])?;
                self.state = State::Normal;
                self.line_start = false;
            }
            '\n' => {
                self.segment.push_str("**");
                self.segment.push_str(&buf);
                self.segment.push('*');
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => {
                let mut new_buf = buf;
                new_buf.push('*');
                new_buf.push(c);
                self.state = State::Bold { buf: new_buf };
            }
        }
        Ok(())
    }

    fn step_italic(&mut self, c: char, mut buf: String, term: &mut Terminal) -> io::Result<()> {
        match c {
            '*' => {
                term.append(&[Span::styled(buf, italic_style())])?;
                self.state = State::Normal;
                self.line_start = false;
            }
            '\n' => {
                self.segment.push('*');
                self.segment.push_str(&buf);
                self.segment.push('\n');
                self.flush_segment(term)?;
                self.state = State::Normal;
                self.line_start = true;
            }
            _ => { buf.push(c); self.state = State::Italic { buf }; }
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
            // Not a table — emit both lines as plain text.
            self.segment.push('|');
            for cell_spans in &header_cells {
                for span in cell_spans { self.segment.push_str(&span.text); }
                self.segment.push('|');
            }
            self.segment.push('\n');
            self.segment.push_str(&sep_buf);
            self.segment.push('\n');
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
        term: &mut Terminal,
    ) -> io::Result<()> {
        // First char of a new row determines whether the table continues.
        if self.line_start {
            if c == '|' {
                self.line_start = false;
                self.state = State::Table { alignments, header, rows, current_row, cell };
                return Ok(());
            }
            // Blank line or non-pipe char: table is complete — render it now.
            render_buffered_table(&header, &rows, &alignments, term)?;
            self.state = State::Normal;
            self.line_start = true;
            if c == '\n' {
                self.segment.push('\n');
            } else {
                return self.step(c, term);
            }
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
    term: &mut Terminal,
) -> io::Result<()> {
    let col_count = header.len().max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
    if col_count == 0 {
        return Ok(());
    }
    let empty: Vec<Span> = Vec::new();

    // Natural content width for each column (no padding yet).
    let col_widths: Vec<usize> = (0..col_count)
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

    let sep = table_sep_style();
    render_border_row(&col_widths, '┌', '─', '┬', '┐', sep.clone(), term)?;
    render_data_row(header, &col_widths, true, sep.clone(), term)?;
    render_border_row(&col_widths, '├', '─', '┼', '┤', sep.clone(), term)?;
    for row in rows {
        render_data_row(row, &col_widths, false, sep.clone(), term)?;
    }
    render_border_row(&col_widths, '└', '─', '┴', '┘', sep.clone(), term)
}

fn render_border_row(
    col_widths: &[usize],
    left: char,
    fill: char,
    mid: char,
    right: char,
    style: ContentStyle,
    term: &mut Terminal,
) -> io::Result<()> {
    let mut line = String::new();
    line.push(left);
    for (i, &w) in col_widths.iter().enumerate() {
        for _ in 0..w + 2 { line.push(fill); } // +2 for the spaces either side
        if i + 1 < col_widths.len() { line.push(mid); }
    }
    line.push(right);
    line.push('\n');
    term.append(&[Span::styled(line, style)])
}

fn render_data_row(
    cells: &[Vec<Span>],
    col_widths: &[usize],
    is_header: bool,
    sep_style: ContentStyle,
    term: &mut Terminal,
) -> io::Result<()> {
    let empty: Vec<Span> = Vec::new();
    for (i, &w) in col_widths.iter().enumerate() {
        term.append(&[Span::styled("│ ", sep_style.clone())])?;
        let cell = cells.get(i).unwrap_or(&empty);
        let truncated = truncate_spans(cell, w);
        let actual_len = cell_char_len(&truncated);
        if is_header {
            for span in &truncated {
                let style = ContentStyle {
                    attributes: span.style.attributes | Attribute::Bold,
                    ..span.style.clone()
                };
                term.append(&[Span::styled(span.text.clone(), style)])?;
            }
        } else {
            term.append(&truncated)?;
        }
        let pad = w.saturating_sub(actual_len);
        if pad > 0 {
            term.append(&[Span::plain(" ".repeat(pad))])?;
        }
        term.append(&[Span::plain(" ")])?;
    }
    term.append(&[Span::styled("│\n", sep_style)])
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
        first.text = first.text.trim_start().to_string();
    }
    if let Some(last) = spans.last_mut() {
        last.text = last.text.trim_end().to_string();
    }
    spans.retain(|s| !s.text.is_empty());
    spans
}

fn cell_char_len(spans: &[Span]) -> usize {
    spans.iter().map(|s| s.text.chars().count()).sum()
}

fn truncate_spans(spans: &[Span], max_chars: usize) -> Vec<Span> {
    let mut result = Vec::new();
    let mut remaining = max_chars;
    for span in spans {
        if remaining == 0 { break; }
        let chars: Vec<char> = span.text.chars().collect();
        if chars.len() <= remaining {
            result.push(span.clone());
            remaining -= chars.len();
        } else {
            let truncated: String = chars[..remaining].iter().collect();
            result.push(Span::styled(truncated, span.style.clone()));
            remaining = 0;
        }
    }
    result
}

// ── Style helpers ──────────────────────────────────────────────────────────

fn code_style() -> ContentStyle {
    ContentStyle { foreground_color: Some(Color::DarkGrey), ..ContentStyle::default() }
}

fn inline_code_style() -> ContentStyle {
    ContentStyle { foreground_color: Some(Color::Cyan), ..ContentStyle::default() }
}

fn header_style(level: u8) -> ContentStyle {
    let mut s = ContentStyle { foreground_color: Some(Color::White), ..ContentStyle::default() };
    if level <= 2 {
        s.attributes = Attribute::Bold.into();
    }
    s
}

fn bold_style() -> ContentStyle {
    ContentStyle { attributes: Attribute::Bold.into(), ..ContentStyle::default() }
}

fn italic_style() -> ContentStyle {
    ContentStyle { attributes: Attribute::Italic.into(), ..ContentStyle::default() }
}

fn table_sep_style() -> ContentStyle {
    ContentStyle { foreground_color: Some(Color::DarkGrey), ..ContentStyle::default() }
}
