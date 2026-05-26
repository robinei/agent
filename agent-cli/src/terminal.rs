use std::io::{self, Write};
use std::time::{Duration, Instant};

const SPINNER_FRAMES: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
pub(crate) const SPINNER_INTERVAL: Duration = Duration::from_millis(80);

use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{
        self, Event, KeyCode, KeyEvent, KeyModifiers, KeyboardEnhancementFlags,
        PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute, queue,
    style::{Color, ContentStyle, Print, ResetColor, SetBackgroundColor, SetStyle},
    terminal::{self, Clear, ClearType, DisableLineWrap, EnableLineWrap, ScrollUp},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    Submit(String),
    Cancel,
    Resize,
    /// Up arrow pressed while already on the top display row of the input.
    HistoryPrev,
    /// Down arrow pressed while already on the bottom display row of the input.
    HistoryNext,
}

/// A styled text chunk for `Terminal::append`. Build with `Span::plain` or
/// `Span::styled`; use `crossterm::style::ContentStyle` to set colors/attributes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: ContentStyle,
}

impl Span {
    pub fn plain(text: impl Into<String>) -> Self {
        Self { text: text.into(), style: ContentStyle::default() }
    }

    pub fn styled(text: impl Into<String>, style: ContentStyle) -> Self {
        Self { text: text.into(), style }
    }
}

pub struct Terminal {
    stdout: io::Stdout,
    input: String,
    input_cursor: usize, // byte offset into input
    kill_buffer: String,
    status: Vec<Span>,
    write_row: u16,
    write_col: u16,
    // Newlines are deferred: write_row isn't advanced on a trailing \n until the next
    // non-empty content arrives, preventing a blank gap above the owned region.
    pending_newline: bool,
    tw: u16,
    th: u16,
    owned_height: u16,
    prompt: String,
    append_buf: Vec<Span>,
    last_flush: Instant,
    /// How often buffered append content is pushed to the screen. Default 16 ms (~60 fps).
    pub flush_interval: Duration,
    spinner_active: bool,
    spinner_frame: usize,
    last_spinner_tick: Instant,
    torn_down: bool,
}

impl Terminal {
    pub fn new(prompt: &str) -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let (tw, th) = terminal::size()?;
        let prompt_s = prompt.to_string();
        let owned_height = 2u16; // 1 input row (empty) + 1 status row

        let mut term = Self {
            stdout: io::stdout(),
            input: String::new(),
            input_cursor: 0,
            kill_buffer: String::new(),
            status: Vec::new(),
            write_row: th.saturating_sub(owned_height + 1),
            write_col: 0,
            pending_newline: false,
            tw,
            th,
            owned_height,
            prompt: prompt_s,
            append_buf: Vec::new(),
            last_flush: Instant::now(),
            flush_interval: Duration::from_millis(16),
            spinner_active: false,
            spinner_frame: 0,
            last_spinner_tick: Instant::now(),
            torn_down: false,
        };

        execute!(term.stdout, DisableLineWrap)?;
        // Best-effort: terminals that don't speak the Kitty keyboard protocol ignore
        // this silently, so Shift+Enter just won't be distinguishable there.
        let _ = execute!(
            term.stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        );
        for _ in 0..term.owned_height {
            write!(term.stdout, "\r\n")?;
        }
        term.stdout.flush()?;
        term.render_owned_impl(true, term.th.saturating_sub(term.owned_height))?;

        Ok(term)
    }

    fn prompt_cols(&self) -> u16 {
        self.prompt.chars().count() as u16
    }

    // Input characters that fit on each display row. First and continuation rows are equal
    // because the continuation margin is the same width as the prompt.
    fn cols_per_row(&self) -> u16 {
        self.tw.saturating_sub(self.prompt_cols()).max(1)
    }

    // Total display rows occupied by the input (sum across all logical lines).
    fn input_display_rows(&self) -> u16 {
        let cols = self.cols_per_row() as usize;
        self.input
            .split('\n')
            .map(|line| line.chars().count().div_ceil(cols).max(1))
            .sum::<usize>() as u16
    }

    // Cursor position as (col, row_offset_from_top_of_owned_region).
    fn cursor_display_pos(&self) -> (u16, u16) {
        let prompt_cols = self.prompt_cols() as usize;
        let cols = self.cols_per_row() as usize;
        let mut remaining = self.input[..self.input_cursor].chars().count();
        let mut display_row = 0usize;

        for line in self.input.split('\n') {
            let line_len = line.chars().count();
            if remaining <= line_len {
                let col_in_row = remaining % cols;
                let row_in_line = remaining / cols;
                let line_rows = line_len.div_ceil(cols).max(1);
                // When `remaining` lands exactly on a row boundary that is past the
                // last display row of this line (cursor at end of a line whose length
                // is an exact multiple of cols), the naive formula would advance to a
                // non-existent row — which falls on the status bar. Clamp to the last
                // column of the last real row instead.
                if col_in_row == 0 && row_in_line > 0 && row_in_line >= line_rows {
                    return (
                        self.tw.saturating_sub(1),
                        (display_row + row_in_line - 1) as u16,
                    );
                }
                return (
                    (prompt_cols + col_in_row) as u16,
                    (display_row + row_in_line) as u16,
                );
            }
            display_row += line_len.div_ceil(cols).max(1);
            remaining -= line_len + 1; // +1 for the '\n'
        }
        (prompt_cols as u16, display_row as u16)
    }

    // Inverse of cursor_display_pos: given a visual (col, row), return the byte
    // offset of the nearest character. Clamps to the end of shorter rows.
    fn byte_offset_at_display_pos(&self, target_col: u16, target_row: u16) -> usize {
        let prompt_cols = self.prompt_cols() as usize;
        let cols = self.cols_per_row() as usize;
        // Strip the prompt/margin to get the content column; clamp to row width.
        let content_col = (target_col as usize)
            .saturating_sub(prompt_cols)
            .min(cols.saturating_sub(1));

        let mut current_row = 0usize;
        let mut byte_offset = 0usize;

        for line in self.input.split('\n') {
            let line_chars: Vec<char> = line.chars().collect();
            let line_char_len = line_chars.len();
            let line_display_rows = line_char_len.div_ceil(cols).max(1);

            if current_row + line_display_rows > target_row as usize {
                let row_within_line = target_row as usize - current_row;
                let char_offset = (row_within_line * cols + content_col).min(line_char_len);
                let bytes: usize = line_chars[..char_offset].iter().map(|c| c.len_utf8()).sum();
                return byte_offset + bytes;
            }

            current_row += line_display_rows;
            byte_offset += line.len() + 1; // +1 for the '\n'
        }

        self.input.len()
    }

    fn render_owned_impl(&mut self, show_cursor: bool, old_top: u16) -> io::Result<()> {
        let prompt_cols = self.prompt_cols();
        let cols = self.cols_per_row() as usize;
        let margin = " ".repeat(prompt_cols as usize);

        // Advance spinner frame when enough time has elapsed.
        if self.spinner_active && self.last_spinner_tick.elapsed() >= SPINNER_INTERVAL {
            self.spinner_frame = (self.spinner_frame + 1) % SPINNER_FRAMES.len();
            self.last_spinner_tick = Instant::now();
        }
        let prompt_display: String = if self.spinner_active {
            let frame = SPINNER_FRAMES[self.spinner_frame];
            let pad = (prompt_cols as usize).saturating_sub(1);
            format!("{}{}", frame, " ".repeat(pad))
        } else {
            self.prompt.clone()
        };

        let new_owned_height = self.input_display_rows() + 1; // +1 for status bar

        if new_owned_height > self.owned_height {
            // Owned region growing upward: scroll append content up to preserve it.
            let growth = new_owned_height - self.owned_height;
            queue!(self.stdout, ScrollUp(growth))?;
            self.write_row = self.write_row.saturating_sub(growth);
        }

        self.owned_height = new_owned_height;
        let top = self.th.saturating_sub(self.owned_height);

        // clear_from covers released rows (shrink) and newly claimed rows (grow).
        let clear_from = old_top.min(top);
        queue!(self.stdout, MoveTo(0, clear_from), Clear(ClearType::FromCursorDown))?;
        queue!(self.stdout, MoveTo(0, top))?;

        // Render each logical line; wrap each at cols_per_row with a continuation
        // margin.  Use MoveTo for every display row instead of \\r\\n so that
        // re-renders (e.g. on resize) don't commit lines to the scrollback.
        let mut display_row = 0u16;
        for line in self.input.split('\n') {
            let line_chars: Vec<char> = line.chars().collect();
            let n = line_chars.len();
            let row_count = n.div_ceil(cols).max(1);
            for r in 0..row_count {
                let chunk: String = line_chars[r * cols..((r + 1) * cols).min(n)].iter().collect();
                queue!(self.stdout, MoveTo(0, top + display_row))?;
                if r == 0 {
                    queue!(self.stdout, Print(format!("{}{}", prompt_display, chunk)))?;
                } else {
                    queue!(self.stdout, Print(format!("{}{}", margin, chunk)))?;
                }
                display_row += 1;
            }
        }

        // Status bar: render spans left-to-right, clipping at tw. Spans without an
        // explicit background fall back to DarkGrey so the bar is always filled.
        let mut remaining = self.tw as usize;
        queue!(self.stdout, MoveTo(0, self.th.saturating_sub(1)))?;
        for span in &self.status {
            if remaining == 0 {
                break;
            }
            let text: String = span.text.chars().take(remaining).collect();
            remaining = remaining.saturating_sub(span.text.chars().count());
            let mut style = span.style;
            if style.background_color.is_none() {
                style.background_color = Some(Color::DarkGrey);
            }
            queue!(self.stdout, SetStyle(style), Print(&text), ResetColor)?;
        }
        if remaining > 0 {
            queue!(
                self.stdout,
                SetBackgroundColor(Color::DarkGrey),
                Print(" ".repeat(remaining)),
                ResetColor,
            )?;
        }

        let (cursor_col, cursor_row_offset) = self.cursor_display_pos();
        if show_cursor {
            queue!(self.stdout, MoveTo(cursor_col, top + cursor_row_offset), Show)?;
        } else {
            queue!(self.stdout, MoveTo(cursor_col, top + cursor_row_offset))?;
        }

        // Single flush: all queued commands reach the terminal atomically, so the
        // cursor never visibly lands on an intermediate position during a redraw.
        self.stdout.flush()
    }

    fn render_owned(&mut self) -> io::Result<()> {
        self.render_owned_impl(true, self.th.saturating_sub(self.owned_height))
    }

    fn advance_row(&mut self) -> io::Result<()> {
        let boundary = self.th.saturating_sub(self.owned_height);
        if self.write_row + 1 >= boundary {
            // Queue ScrollUp + clear before render_owned_impl so all three are
            // flushed together in render_owned_impl's single flush call.
            queue!(self.stdout, ScrollUp(1))?;
            // ScrollUp moves owned-region content into write_row; clear it.
            queue!(self.stdout, MoveTo(0, self.write_row), Clear(ClearType::CurrentLine))?;
            self.render_owned_impl(false, self.th.saturating_sub(self.owned_height))?;
            // write_row stays at boundary-1: freshly cleared, ready for content.
        } else {
            self.write_row += 1;
        }
        self.write_col = 0;
        Ok(())
    }

    /// Buffer spans and flush to the screen once `flush_interval` has elapsed.
    /// Call `flush_append` explicitly to force-render any remainder (e.g. end of turn).
    pub fn append(&mut self, spans: &[Span]) -> io::Result<()> {
        self.append_buf.extend_from_slice(spans);
        if self.last_flush.elapsed() >= self.flush_interval {
            self.flush_append()
        } else {
            Ok(())
        }
    }

    /// Render any buffered spans immediately, resetting the flush timer.
    pub fn flush_append(&mut self) -> io::Result<()> {
        if self.append_buf.is_empty() {
            self.last_flush = Instant::now();
            return Ok(());
        }
        let spans = std::mem::take(&mut self.append_buf);
        let result = self.do_append(&spans);
        self.last_flush = Instant::now();
        result
    }

    fn do_append(&mut self, spans: &[Span]) -> io::Result<()> {
        // Queue Hide first; it reaches the terminal on the same flush as the first
        // Print, so the cursor is never visible at the write position.
        queue!(self.stdout, Hide)?;

        // Split all spans on '\n' into logical lines. Each line is a sequence of
        // (text, style) chunks printed consecutively; the terminal wraps them at tw.
        let mut logical_lines: Vec<Vec<(String, ContentStyle)>> = vec![Vec::new()];
        for span in spans {
            let parts: Vec<&str> = span.text.split('\n').collect();
            for (i, part) in parts.iter().enumerate() {
                if i > 0 {
                    logical_lines.push(Vec::new());
                }
                if !part.is_empty() {
                    logical_lines.last_mut().unwrap().push((part.to_string(), span.style));
                }
            }
        }

        let n = logical_lines.len();
        for (i, line) in logical_lines.iter().enumerate() {
            let has_content = !line.is_empty();

            // Realize a deferred newline only when non-empty content follows, or when
            // another \n follows immediately (consecutive blank lines).
            if self.pending_newline && (has_content || i < n - 1) {
                self.advance_row()?;
                self.pending_newline = false;
            }

            if has_content {
                // Enable line wrapping so the terminal wraps content naturally and
                // can reflow on resize. The owned region runs with DisableLineWrap.
                let total_chars: usize = line.iter().map(|(t, _)| t.chars().count()).sum();
                // Number of terminal display rows this content consumes (ceil division).
                // The terminal handles wrapping at self.tw; we track rows for overflow.
                let combined = self.write_col as usize + total_chars;
                let tw = self.tw as usize;
                let display_rows = if combined == 0 { 1u16 } else { combined.div_ceil(tw) as u16 };

                queue!(
                    self.stdout,
                    EnableLineWrap,
                    MoveTo(self.write_col, self.write_row),
                )?;
                for chunk in line {
                    queue!(self.stdout, SetStyle(chunk.1), Print(&chunk.0), ResetColor)?;
                }
                queue!(self.stdout, DisableLineWrap)?;

                // Cursor after terminal wrapping:
                // col = (write_col + total_chars) % tw
                // row = write_row + display_rows - 1   (the last row consumed)
                let final_col =
                    ((self.write_col as usize + total_chars) % self.tw as usize) as u16;
                self.write_col = final_col;
                self.write_row = self
                    .write_row
                    .saturating_add(display_rows.saturating_sub(1));

                // If wrapping pushed us into the owned region, scroll.
                let boundary = self.th.saturating_sub(self.owned_height);
                if self.write_row >= boundary {
                    let overflow = self.write_row - boundary + 1;
                    queue!(self.stdout, ScrollUp(overflow))?;
                    self.write_row = boundary.saturating_sub(1);
                    queue!(
                        self.stdout,
                        MoveTo(0, self.write_row),
                        Clear(ClearType::CurrentLine),
                    )?;
                    self.render_owned_impl(false, self.th.saturating_sub(self.owned_height))?;
                    self.write_col = 0;
                }
            }

            if i < n - 1 {
                self.pending_newline = true;
            }
        }

        let top = self.th.saturating_sub(self.owned_height);
        let (cursor_col, cursor_row_offset) = self.cursor_display_pos();
        queue!(self.stdout, MoveTo(cursor_col, top + cursor_row_offset), Show)?;
        self.stdout.flush()
    }

    pub fn set_status(&mut self, spans: &[Span]) -> io::Result<()> {
        self.status = spans.to_vec();
        self.render_owned()
    }

    pub fn poll(&mut self, timeout: Duration) -> io::Result<Option<TermEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }

        let ctrl = modifiers_has(KeyModifiers::CONTROL);
        let alt = modifiers_has(KeyModifiers::ALT);
        let shift = modifiers_has(KeyModifiers::SHIFT);

        match event::read()? {
            Event::Key(KeyEvent { code, modifiers, .. }) => match code {
                // --- cancel ---
                KeyCode::Char('c') if ctrl(modifiers) => Ok(Some(TermEvent::Cancel)),
                KeyCode::Char('d') if ctrl(modifiers) && self.input.is_empty() => {
                    Ok(Some(TermEvent::Cancel))
                }
                KeyCode::Esc => Ok(Some(TermEvent::Cancel)),

                // --- submit / newline ---
                // Shift+Enter inserts a newline into the input; plain Enter submits.
                // Shift+Enter requires the Kitty keyboard protocol; on terminals that
                // don't support it the key arrives as plain Enter and submits instead.
                KeyCode::Enter if shift(modifiers) => {
                    self.input.insert(self.input_cursor, '\n');
                    self.input_cursor += 1;
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Enter => {
                    let text = self.input.clone();
                    self.input.clear();
                    self.input_cursor = 0;
                    self.render_owned()?;
                    Ok(Some(TermEvent::Submit(text)))
                }

                // --- word movement ---
                KeyCode::Left
                    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.input_cursor = word_backward(&self.input, self.input_cursor);
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Right
                    if modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.input_cursor = word_forward(&self.input, self.input_cursor);
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('b') if alt(modifiers) => {
                    self.input_cursor = word_backward(&self.input, self.input_cursor);
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('f') if alt(modifiers) => {
                    self.input_cursor = word_forward(&self.input, self.input_cursor);
                    self.render_owned()?;
                    Ok(None)
                }

                // --- char movement ---
                KeyCode::Left => {
                    if self.input_cursor > 0 {
                        self.input_cursor = prev_char_boundary(&self.input, self.input_cursor);
                        self.render_owned()?;
                    }
                    Ok(None)
                }
                KeyCode::Right => {
                    if self.input_cursor < self.input.len() {
                        let c = self.input[self.input_cursor..].chars().next().unwrap();
                        self.input_cursor += c.len_utf8();
                        self.render_owned()?;
                    }
                    Ok(None)
                }

                // --- vertical movement / history ---
                KeyCode::Up => {
                    let (col, row) = self.cursor_display_pos();
                    if row > 0 {
                        self.input_cursor = self.byte_offset_at_display_pos(col, row - 1);
                        self.render_owned()?;
                        Ok(None)
                    } else {
                        Ok(Some(TermEvent::HistoryPrev))
                    }
                }
                KeyCode::Down => {
                    let (col, row) = self.cursor_display_pos();
                    let last_row = self.input_display_rows().saturating_sub(1);
                    if row < last_row {
                        self.input_cursor = self.byte_offset_at_display_pos(col, row + 1);
                        self.render_owned()?;
                        Ok(None)
                    } else {
                        Ok(Some(TermEvent::HistoryNext))
                    }
                }

                // --- line movement ---
                KeyCode::Char('a') if ctrl(modifiers) => {
                    self.input_cursor = 0;
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('e') if ctrl(modifiers) => {
                    self.input_cursor = self.input.len();
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Home => {
                    self.input_cursor = 0;
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::End => {
                    self.input_cursor = self.input.len();
                    self.render_owned()?;
                    Ok(None)
                }

                // --- kill/yank ---
                KeyCode::Char('k') if ctrl(modifiers) => {
                    self.kill_buffer = self.input[self.input_cursor..].to_string();
                    self.input.truncate(self.input_cursor);
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('u') if ctrl(modifiers) => {
                    self.kill_buffer = self.input[..self.input_cursor].to_string();
                    self.input.drain(..self.input_cursor);
                    self.input_cursor = 0;
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('w') if ctrl(modifiers) => {
                    let new = word_backward(&self.input, self.input_cursor);
                    self.kill_buffer = self.input[new..self.input_cursor].to_string();
                    self.input.drain(new..self.input_cursor);
                    self.input_cursor = new;
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Backspace if alt(modifiers) => {
                    let new = word_backward(&self.input, self.input_cursor);
                    self.kill_buffer = self.input[new..self.input_cursor].to_string();
                    self.input.drain(new..self.input_cursor);
                    self.input_cursor = new;
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('d') if alt(modifiers) => {
                    let new = word_forward(&self.input, self.input_cursor);
                    self.kill_buffer = self.input[self.input_cursor..new].to_string();
                    self.input.drain(self.input_cursor..new);
                    self.render_owned()?;
                    Ok(None)
                }
                KeyCode::Char('y') if ctrl(modifiers) => {
                    let yank = self.kill_buffer.clone();
                    self.input.insert_str(self.input_cursor, &yank);
                    self.input_cursor += yank.len();
                    self.render_owned()?;
                    Ok(None)
                }

                // --- single-char deletion ---
                KeyCode::Backspace => {
                    if self.input_cursor > 0 {
                        let prev = prev_char_boundary(&self.input, self.input_cursor);
                        self.input.remove(prev);
                        self.input_cursor = prev;
                        self.render_owned()?;
                    }
                    Ok(None)
                }
                KeyCode::Delete => {
                    if self.input_cursor < self.input.len() {
                        self.input.remove(self.input_cursor);
                        self.render_owned()?;
                    }
                    Ok(None)
                }
                KeyCode::Char('d') if ctrl(modifiers) => {
                    if self.input_cursor < self.input.len() {
                        self.input.remove(self.input_cursor);
                        self.render_owned()?;
                    }
                    Ok(None)
                }

                // --- insert ---
                KeyCode::Char(c) => {
                    self.input.insert(self.input_cursor, c);
                    self.input_cursor += c.len_utf8();
                    self.render_owned()?;
                    Ok(None)
                }

                _ => Ok(None),
            },
            Event::Resize(tw, th) => {
                // Save the real old top before updating dimensions, so
                // render_owned_impl can correctly clear the old owned region.
                let old_top = self.th.saturating_sub(self.owned_height);
                self.tw = tw;
                self.th = th;
                self.render_owned_impl(true, old_top)?;
                // Past content was printed at a different width; its visual
                // position is stale.  Reset write_row to just above the owned
                // region so the next append starts at a known-safe position
                // instead of potentially overwriting reflowed history lines.
                self.write_row =
                    self.th.saturating_sub(self.owned_height).saturating_sub(1);
                self.write_col = 0;
                Ok(Some(TermEvent::Resize))
            }
            _ => Ok(None),
        }
    }

    pub fn cols(&self) -> u16 {
        self.tw
    }

    /// Show or hide the activity spinner in the prompt position.
    /// While active the prompt character is replaced by an animated braille spinner.
    pub fn set_spinner_active(&mut self, active: bool) -> io::Result<()> {
        self.spinner_active = active;
        if !active {
            self.spinner_frame = 0;
        }
        self.render_owned()
    }

    pub fn input(&self) -> &str {
        &self.input
    }

    /// Re-render the owned region (input area + status bar).
    /// Advances the spinner frame if enough time has elapsed.
    pub fn refresh(&mut self) -> io::Result<()> {
        self.render_owned()
    }

    pub fn clear_input(&mut self) -> io::Result<()> {
        self.input.clear();
        self.input_cursor = 0;
        self.render_owned()
    }

    /// Replace the input buffer with `text` and move the cursor to the end.
    /// Use this to load history entries returned by `HistoryPrev`/`HistoryNext`.
    pub fn set_input(&mut self, text: &str) -> io::Result<()> {
        self.input = text.to_string();
        self.input_cursor = self.input.len();
        self.render_owned()
    }

    pub fn teardown(&mut self) -> io::Result<()> {
        if self.torn_down {
            return Ok(());
        }
        self.torn_down = true;
        let _ = execute!(self.stdout, PopKeyboardEnhancementFlags);
        execute!(
            self.stdout,
            MoveTo(0, self.th.saturating_sub(self.owned_height)),
            Clear(ClearType::FromCursorDown),
            EnableLineWrap,
        )?;
        terminal::disable_raw_mode()?;
        self.stdout.flush()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = self.teardown();
    }
}

fn modifiers_has(flag: KeyModifiers) -> impl Fn(KeyModifiers) -> bool {
    move |m| m.contains(flag)
}

fn prev_char_boundary(s: &str, byte_pos: usize) -> usize {
    let mut i = byte_pos - 1;
    while !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

// Move to the end of the next word (skip whitespace, then skip non-whitespace).
fn word_forward(s: &str, pos: usize) -> usize {
    let chars: Vec<(usize, char)> = s[pos..].char_indices().collect();
    let mut i = 0;
    while i < chars.len() && chars[i].1.is_whitespace() {
        i += 1;
    }
    while i < chars.len() && !chars[i].1.is_whitespace() {
        i += 1;
    }
    if i < chars.len() {
        pos + chars[i].0
    } else {
        s.len()
    }
}

// Move to the start of the previous word (skip whitespace backward, then skip non-whitespace).
fn word_backward(s: &str, pos: usize) -> usize {
    let chars: Vec<(usize, char)> = s[..pos].char_indices().collect();
    let mut i = chars.len();
    while i > 0 && chars[i - 1].1.is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].1.is_whitespace() {
        i -= 1;
    }
    if i < chars.len() {
        chars[i].0
    } else {
        0
    }
}
