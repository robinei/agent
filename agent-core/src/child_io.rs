use tokio::io::{AsyncBufReadExt, BufReader};

/// Reads JSON lines from a child process's stdout.
/// Each call to `next_line` returns a full line until EOF.
///
/// INTENTIONAL: we read one complete JSON line per call. Never poll
/// `read_line` inside a `select!` — it is not cancel-safe. The caller is
/// expected to run this in a dedicated forwarder task that reads complete
/// lines and sends them into an mpsc channel.
pub struct ChildLines {
    reader: BufReader<tokio::process::ChildStdout>,
}

impl ChildLines {
    pub fn new(stdout: tokio::process::ChildStdout) -> Self {
        Self {
            reader: BufReader::new(stdout),
        }
    }

    /// Read the next line. Returns `Ok(None)` on EOF.
    pub async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        let mut buf = String::new();
        match self.reader.read_line(&mut buf).await? {
            0 => Ok(None),
            _ => {
                // Trim trailing newline/carriage return but keep interior
                let trimmed = buf.trim_end().to_string();
                Ok(Some(trimmed))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[tokio::test]
    async fn test_child_lines_basic() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("echo '{\"hello\":\"world\"}'")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let stdout = child.stdout.take().expect("take stdout");
        let mut lines = ChildLines::new(stdout);

        let line = lines.next_line().await.expect("read line");
        assert_eq!(line, Some(r#"{"hello":"world"}"#.to_string()));

        let line = lines.next_line().await.expect("read line after");
        assert_eq!(line, None);

        child.wait().await.expect("wait");
    }

    #[tokio::test]
    async fn test_child_lines_multiple() {
        let mut child = tokio::process::Command::new("sh")
            .arg("-c")
            .arg("echo 'line1'; echo 'line2'; echo 'line3'")
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn sh");

        let stdout = child.stdout.take().expect("take stdout");
        let mut lines = ChildLines::new(stdout);

        assert_eq!(lines.next_line().await.unwrap(), Some("line1".into()));
        assert_eq!(lines.next_line().await.unwrap(), Some("line2".into()));
        assert_eq!(lines.next_line().await.unwrap(), Some("line3".into()));
        assert_eq!(lines.next_line().await.unwrap(), None);

        child.wait().await.expect("wait");
    }
}