use std::collections::{HashMap, HashSet};
use std::os::fd::{IntoRawFd, RawFd};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use log::{info, warn};
use nix::fcntl::{fcntl, FcntlArg, OFlag};
use nix::unistd::read;

use agent_core::types::{
    Diagnostic, DiagnosticSeverity, DiagnosticsFile, LspServerConfig, Position, Range,
};

#[derive(Debug, thiserror::Error)]
pub enum LspError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("nix error: {0}")]
    Nix(#[from] nix::Error),
    #[error("spawn LSP {command}: {source}")]
    Spawn {
        command: String,
        #[source]
        source: std::io::Error,
    },
    #[error("no stdin for LSP process")]
    NoStdin,
    #[error("no stdout for LSP process")]
    NoStdout,
    #[error("{0}")]
    Other(String),
}

pub type LspResult<T> = std::result::Result<T, LspError>;

pub struct LspFileResult {
    pub path: String,
    pub diagnostics: Vec<Diagnostic>,
}

pub struct PendingLspTool {
    pub request_id: u64,
    pub lang_id: String,
    pub tool_name: String,
    pub tool_call_id: String,
}

pub struct LspWaitState {
    pub deadline: Instant,
    pub silence_until: Instant,
    pub silence_ms: u64,
    pub pending_tool_requests: Vec<PendingLspTool>,
    /// Maps (absolute path, tool_call_id) so diagnostics can be appended to
    /// the tool result that caused them.
    pub dirty_by_call: Vec<(std::path::PathBuf, String)>,
}

pub struct LspClient {
    stdin: std::process::ChildStdin,
    pub stdout_fd: RawFd,
    _child: std::process::Child,
    read_buf: Vec<u8>,
    next_id: u64,
    opened: HashSet<lsp_types::Url>,
    pub diagnostics: HashMap<lsp_types::Url, Vec<Diagnostic>>,
    pre_edit_diagnostics: HashMap<lsp_types::Url, Vec<Diagnostic>>,
    pub pending_responses: HashMap<u64, serde_json::Value>,
    root_uri: String,
    pub lang_id: String,
}

impl LspClient {
    pub fn spawn(
        lang_id: &str,
        command: &str,
        args: &[String],
        root_uri: &str,
        timeout_ms: u64,
    ) -> LspResult<Self> {
        let mut child = Command::new(command)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| LspError::Spawn {
                command: command.to_string(),
                source: e,
            })?;

        let stdin = child.stdin.take().ok_or(LspError::NoStdin)?;
        let stdout_fd = child.stdout.take().ok_or(LspError::NoStdout)?.into_raw_fd();

        fcntl(stdout_fd, FcntlArg::F_SETFL(OFlag::O_NONBLOCK))?;

        let mut client = LspClient {
            stdin,
            stdout_fd,
            _child: child,
            read_buf: Vec::new(),
            next_id: 1,
            opened: HashSet::new(),
            diagnostics: HashMap::new(),
            pre_edit_diagnostics: HashMap::new(),
            pending_responses: HashMap::new(),
            root_uri: root_uri.to_string(),
            lang_id: lang_id.to_string(),
        };

        let init_params = serde_json::json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "textDocument": {
                    "synchronization": {
                        "didSave": true,
                        "willSave": false,
                        "willSaveWaitUntil": false
                    }
                }
            }
        });
        let req_id = client.next_id;
        client.next_id += 1;
        client.write_request("initialize", init_params, req_id);

        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        let mut buf = Vec::new();
        loop {
            if Instant::now() >= deadline {
                return Err(LspError::Other("LSP initialize timed out".into()));
            }
            let mut tmp = [0u8; 4096];
            match read(stdout_fd, &mut tmp) {
                Ok(0) => {
                    return Err(LspError::Other(
                        "LSP process exited during initialize".into(),
                    ))
                }
                Ok(n) => buf.extend(&tmp[..n]),
                Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => return Err(LspError::Other(format!("read error: {}", e))),
            }

            while let Some(frame) = Self::parse_frame(&mut buf) {
                if let Some(id_val) = frame.get("id") {
                    if id_val.as_u64() == Some(req_id) {
                        client.write_notification("initialized", serde_json::json!({}));
                        return Ok(client);
                    }
                }
            }
        }
    }

    pub fn notify_saved(&mut self, path: &Path) {
        let url = lsp_types::Url::from_file_path(path)
            .unwrap_or_else(|_| lsp_types::Url::parse("file:///none").unwrap());
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                warn!("LSP notify_saved read {}: {}", path.display(), e);
                return;
            }
        };

        // Snapshot current diagnostics as the pre-edit baseline before sending the edit.
        self.pre_edit_diagnostics.insert(
            url.clone(),
            self.diagnostics.get(&url).cloned().unwrap_or_default(),
        );

        if self.opened.contains(&url) {
            let change_params = serde_json::json!({
                "textDocument": {
                    "uri": url.as_str(),
                    "version": 1
                },
                "contentChanges": [{
                    "text": content
                }]
            });
            self.write_notification("textDocument/didChange", change_params);

            let save_params = serde_json::json!({
                "textDocument": {
                    "uri": url.as_str()
                }
            });
            self.write_notification("textDocument/didSave", save_params);
        } else {
            let open_params = serde_json::json!({
                "textDocument": {
                    "uri": url.as_str(),
                    "languageId": self.lang_id,
                    "version": 1,
                    "text": content
                }
            });
            self.write_notification("textDocument/didOpen", open_params);
            self.opened.insert(url);
        }
    }

    pub fn send_request(&mut self, method: &str, params: serde_json::Value) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.write_request(method, params, id);
        id
    }

    pub fn read_available(&mut self) -> bool {
        let mut tmp = [0u8; 65536];
        let mut updated = false;
        loop {
            match read(self.stdout_fd, &mut tmp) {
                Ok(0) => {
                    info!("LSP client {} EOF", self.lang_id);
                    drop(std::mem::take(&mut self.read_buf));
                    break;
                }
                Ok(n) => {
                    let snippet = String::from_utf8_lossy(&tmp[..n.min(500)]);
                    eprintln!("[LSP_RAW][{}] read {} bytes: {:?}", self.lang_id, n, snippet);
                    self.read_buf.extend(&tmp[..n])
                }
                Err(e) if e == nix::errno::Errno::EAGAIN || e == nix::errno::Errno::EWOULDBLOCK => {
                    break
                }
                Err(e) => {
                    warn!("LSP read error: {}", e);
                    break;
                }
            }
        }

        eprintln!("[LSP_RAW][{}] read_buf len after poll: {}", self.lang_id, self.read_buf.len());

        while let Some(frame) = Self::parse_frame(&mut self.read_buf) {
            eprintln!("[LSP_FRAME][{}] parsed frame: {:?}", self.lang_id, &frame.to_string()[..frame.to_string().len().min(500)]);
            if let Some(method) = frame.get("method").and_then(|v| v.as_str()) {
                if method == "textDocument/publishDiagnostics" {
                    if let Some(params) = frame.get("params") {
                        if let Some(uri_str) = params.get("uri").and_then(|v| v.as_str()) {
                            if let Ok(url) = lsp_types::Url::parse(uri_str) {
                                let diags = Self::convert_diagnostics(params.get("diagnostics"));
                                log::info!(
                                    "[LSP] publishDiagnostics: {} --- {} diagnostics",
                                    uri_str,
                                    diags.len()
                                );
                                self.diagnostics.insert(url, diags);
                                updated = true;
                            }
                        }
                    }
                }
            } else if let Some(id_val) = frame.get("id") {
                if let Some(id) = id_val
                    .as_u64()
                    .or_else(|| id_val.as_i64().map(|i| i as u64))
                {
                    self.pending_responses.insert(id, frame);
                    updated = true;
                }
            }
        }
        updated
    }

    pub fn all_diagnostics(&self) -> Vec<LspFileResult> {
        let mut results = Vec::new();
        for (url, diags) in &self.diagnostics {
            if diags.is_empty() {
                continue;
            }
            let path = url.to_string();
            results.push(LspFileResult {
                path,
                diagnostics: diags.clone(),
            });
        }
        results
    }

    /// Returns diagnostics for dirty files split into new (unseen) and seen counts,
    /// then updates the shown snapshot. Files with no new issues and no seen issues
    /// are omitted.
    pub fn take_new_for_display(
        &mut self,
        dirty_paths: &[&std::path::PathBuf],
    ) -> Vec<DiagnosticsFile> {
        log::info!(
            "[LSP] take_new_for_display: {} dirty_paths, {} diagnostics entries",
            dirty_paths.len(),
            self.diagnostics.len()
        );
        for (url, diags) in &self.diagnostics {
            let fp = url.to_file_path().ok();
            log::info!(
                "[LSP]   diag key: {} (file_path={:?}) -> {} diags",
                url.as_str(),
                fp,
                diags.len()
            );
        }
        for dp in dirty_paths {
            log::info!("[LSP]   dirty_path: {:?}", dp);
        }
        let dirty_urls: Vec<lsp_types::Url> = self
            .diagnostics
            .keys()
            .filter(|url| {
                let fp = url.to_file_path().ok();
                dirty_paths
                    .iter()
                    .any(|dp| fp.as_ref().map(|p| p == *dp).unwrap_or(false))
            })
            .cloned()
            .collect();

        let mut results = Vec::new();
        for url in &dirty_urls {
            let current: Vec<Diagnostic> = self.diagnostics.get(url).cloned().unwrap_or_default();
            log::info!("[LSP]   url={} -> {} diagnostics", url.as_str(), current.len());
            let (new_diags, seen_errors, seen_warnings) = {
                let pre = self
                    .pre_edit_diagnostics
                    .get(url)
                    .map(|v| v.as_slice())
                    .unwrap_or(&[]);
                let mut new_diags = Vec::new();
                let mut seen_errors = 0u32;
                let mut seen_warnings = 0u32;
                for diag in &current {
                    if diag_is_seen(diag, pre) {
                        match diag.severity {
                            Some(DiagnosticSeverity::Error) => seen_errors += 1,
                            Some(DiagnosticSeverity::Warning) => seen_warnings += 1,
                            _ => {}
                        }
                    } else {
                        new_diags.push(diag.clone());
                    }
                }
                (new_diags, seen_errors, seen_warnings)
            };
            if !new_diags.is_empty() || seen_errors > 0 || seen_warnings > 0 {
                results.push(DiagnosticsFile {
                    path: url.to_string(),
                    diagnostics: new_diags,
                    seen_errors,
                    seen_warnings,
                });
            }
        }
        results
    }

    fn write_frame(&mut self, body: &str) {
        use std::io::Write;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        let _ = self.stdin.write_all(header.as_bytes());
        let _ = self.stdin.write_all(body.as_bytes());
        let _ = self.stdin.flush();
    }

    fn write_request(&mut self, method: &str, params: serde_json::Value, id: u64) {
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.write_frame(&req.to_string());
    }

    fn write_notification(&mut self, method: &str, params: serde_json::Value) {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });
        self.write_frame(&notif.to_string());
    }

    fn parse_frame(buf: &mut Vec<u8>) -> Option<serde_json::Value> {
        let needle = b"\r\n\r\n";
        let pos = buf.windows(4).position(|w| w == needle)?;
        let header_str = std::str::from_utf8(&buf[..pos]).ok()?;
        let content_length = header_str.lines().find_map(|l| {
            if let Some(val) = l.strip_prefix("Content-Length:") {
                val.trim().parse::<usize>().ok()
            } else {
                None
            }
        })?;
        let body_start = pos + 4;
        if buf.len() < body_start + content_length {
            return None;
        }
        let body = &buf[body_start..body_start + content_length];
        let value: serde_json::Value = serde_json::from_slice(body).ok()?;
        buf.drain(..body_start + content_length);
        Some(value)
    }

    fn convert_diagnostics(value: Option<&serde_json::Value>) -> Vec<Diagnostic> {
        let Some(diags_val) = value else {
            return vec![];
        };
        let Some(arr) = diags_val.as_array() else {
            return vec![];
        };

        arr.iter()
            .filter_map(|d| {
                let range = d.get("range")?;
                let start = range.get("start")?;
                let end = range.get("end")?;
                let message = d.get("message")?.as_str()?;

                Some(Diagnostic {
                    range: Range {
                        start: Position {
                            line: start.get("line")?.as_u64()? as u32,
                            character: start.get("character")?.as_u64()? as u32,
                        },
                        end: Position {
                            line: end.get("line")?.as_u64()? as u32,
                            character: end.get("character")?.as_u64()? as u32,
                        },
                    },
                    severity: d.get("severity").and_then(|s| match s.as_u64()? {
                        1 => Some(DiagnosticSeverity::Error),
                        2 => Some(DiagnosticSeverity::Warning),
                        3 => Some(DiagnosticSeverity::Information),
                        4 => Some(DiagnosticSeverity::Hint),
                        _ => None,
                    }),
                    message: message.to_string(),
                    code: d.get("code").and_then(|c| c.as_str().map(String::from)),
                })
            })
            .collect()
    }
}

pub fn detect_language(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    match ext {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" | "mjs" => Some("javascript"),
        "py" => Some("python"),
        "go" => Some("go"),
        "c" | "h" => Some("c"),
        "cpp" | "cc" | "cxx" | "hpp" => Some("cpp"),
        _ => None,
    }
}

pub fn default_server(lang_id: &str) -> Option<LspServerConfig> {
    match lang_id {
        "rust" => Some(LspServerConfig {
            language: "rust".into(),
            command: "rust-analyzer".into(),
            args: vec![],
            timeout_ms: 5000,
            silence_ms: 500,
        }),
        "typescript" | "javascript" => Some(LspServerConfig {
            language: lang_id.into(),
            command: "typescript-language-server".into(),
            args: vec!["--stdio".into()],
            timeout_ms: 8000,
            silence_ms: 500,
        }),
        "python" => Some(LspServerConfig {
            language: "python".into(),
            command: "pylsp".into(),
            args: vec![],
            timeout_ms: 5000,
            silence_ms: 500,
        }),
        "go" => Some(LspServerConfig {
            language: "go".into(),
            command: "gopls".into(),
            args: vec![],
            timeout_ms: 5000,
            silence_ms: 500,
        }),
        "c" | "cpp" => Some(LspServerConfig {
            language: lang_id.into(),
            command: "clangd".into(),
            args: vec![],
            timeout_ms: 5000,
            silence_ms: 500,
        }),
        _ => None,
    }
}

fn diag_is_seen(diag: &Diagnostic, shown: &[Diagnostic]) -> bool {
    shown.iter().any(|s| {
        s.range.start.line == diag.range.start.line
            && s.severity == diag.severity
            && s.message == diag.message
    })
}

pub fn binary_exists(cmd: &str) -> bool {
    match Command::new(cmd)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(mut child) => child.wait().is_ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => false,
    }
}

pub fn format_diagnostics(results: &[LspFileResult]) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut infos: Vec<String> = Vec::new();

    for r in results {
        let path = &r.path;
        for d in &r.diagnostics {
            let severity_label = match d.severity {
                Some(DiagnosticSeverity::Error) => "error",
                Some(DiagnosticSeverity::Warning) => "warning",
                Some(DiagnosticSeverity::Information) => "info",
                Some(DiagnosticSeverity::Hint) => "hint",
                None => "note",
            };
            let code_str = d
                .code
                .as_ref()
                .map(|c| format!("[{}] ", c))
                .unwrap_or_default();
            let line = format!(
                "{}:{}:{}: {}{}: {}",
                path,
                d.range.start.line + 1,
                d.range.start.character,
                code_str,
                severity_label,
                d.message
            );
            match d.severity {
                Some(DiagnosticSeverity::Error) => errors.push(line),
                Some(DiagnosticSeverity::Warning) => warnings.push(line),
                _ => infos.push(line),
            }
        }
    }

    if !errors.is_empty() {
        lines.push(format!("## Errors\n{}", errors.join("\n")));
    }
    if !warnings.is_empty() {
        lines.push(format!("## Warnings\n{}", warnings.join("\n")));
    }
    if !infos.is_empty() {
        lines.push(format!("## Other Diagnostics\n{}", infos.join("\n")));
    }

    if lines.is_empty() {
        return "No diagnostics.".to_string();
    }

    format!("## LSP Diagnostics\n\n{}", lines.join("\n\n"))
}
