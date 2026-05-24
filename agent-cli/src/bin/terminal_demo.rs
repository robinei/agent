use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use agent_cli::markdown::MarkdownEmitter;
use agent_cli::terminal::{Span, TermEvent, Terminal};
use crossterm::style::{Color, ContentStyle};

// Markdown content emitted token by token to exercise the streaming renderer.
const DEMO_CONTENT: &str = "\
# Welcome to the demo\n\
\n\
This is **bold text** and this is *italic text* in a paragraph.\n\
\n\
Inline code looks like `let x = 42;` in a sentence.\n\
\n\
## Code block (3-backtick fence)\n\
\n\
```rust\n\
fn greet(name: &str) -> String {\n\
    format!(\"Hello, {}!\", name)\n\
}\n\
```\n\
\n\
## Deeper fence (4-backtick, contains 3-backtick)\n\
\n\
````markdown\n\
Use ```triple backticks``` for code.\n\
````\n\
\n\
## Table with inline styles\n\
\n\
| Name | Role | Notes |\n\
|------|------|-------|\n\
| Alice | **Admin** | Has `sudo` access |\n\
| Bob | *Editor* | Can publish |\n\
| Carol | Viewer | Read-only |\n\
\n\
Plain text to finish.\n\
";

fn main() -> io::Result<()> {
    // Send the demo content one character at a time over a channel to simulate
    // streaming LLM tokens arriving at variable speed.
    let (tx, rx) = mpsc::channel::<char>();
    thread::spawn(move || {
        for c in DEMO_CONTENT.chars() {
            thread::sleep(Duration::from_millis(3));
            if tx.send(c).is_err() {
                break;
            }
        }
    });

    let mut term = Terminal::new("> ")?;
    let mut md = MarkdownEmitter::new();
    let start = std::time::Instant::now();
    let mut content_done = false;
    term.set_spinner_active(true)?;

    loop {
        let cyan = ContentStyle { foreground_color: Some(Color::Cyan), ..ContentStyle::default() };
        term.set_status(&[
            Span::plain(format!("{}s", start.elapsed().as_secs())),
            Span::plain("  "),
            Span::styled("streaming markdown demo", cyan),
        ])?;

        match term.poll(Duration::ZERO)? {
            Some(TermEvent::Submit(text)) => {
                let yellow = ContentStyle { foreground_color: Some(Color::Yellow), ..ContentStyle::default() };
                term.append(&[Span::styled(format!("You said: {}\r\n", text), yellow)])?;
                term.flush_append()?;
            }
            Some(TermEvent::Cancel) => break,
            _ => {}
        }

        if !content_done {
            // Drain all available chars each tick so Disconnected is seen
            // immediately once the sender finishes, stopping the spinner promptly.
            loop {
                match rx.try_recv() {
                    Ok(c) => {
                        let tw = term.cols() as usize;
                        md.push(&c.to_string(), &mut |spans| term.append(spans), tw)?;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        let tw = term.cols() as usize;
                        md.flush(&mut |spans| term.append(spans), tw)?;
                        content_done = true;
                        term.set_spinner_active(false)?;
                        break;
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(5));
    }

    term.teardown()?;
    Ok(())
}
