use std::io::{BufRead, BufReader, BufWriter, Write};

use agent_core::rpc::{LlmRequest, PipeOut, WorkerConfig};
use agent_core::store::Store;
use agent_core::types::{NotificationLevel, *};
use log::{error, warn};
use serde::Deserialize;

#[derive(Deserialize)]
struct ConfigEnvelope {
    ch: String,
    msg: serde_json::Value,
}

pub fn read_config(
    reader: &mut BufReader<std::io::Stdin>,
    buf: &mut String,
) -> Result<WorkerConfig, String> {
    let n = reader.read_line(buf).map_err(|e| format!("read stdin: {}", e))?;
    if n == 0 {
        return Err("stdin closed before Config".into());
    }
    let env: ConfigEnvelope = serde_json::from_str(buf.trim_end())
        .map_err(|e| format!("parse config envelope: {}", e))?;
    if env.ch != "config" {
        return Err(format!("expected Config, got ch={}", env.ch));
    }
    let cfg: WorkerConfig = serde_json::from_value(env.msg)
        .map_err(|e| format!("parse WorkerConfig: {}", e))?;
    Ok(cfg)
}

pub fn parse_tree_id() -> Result<String, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut tree_id = None;
    while let Some(arg) = args.next() {
        if arg.as_str() == "--tree-id" {
            tree_id = Some(args.next().ok_or("missing --tree-id value")?);
        }
    }
    tree_id.ok_or_else(|| "--tree-id is required".into())
}

pub fn resolve_repo_path(store: &Store, tree_id: &str) -> std::path::PathBuf {
    match store.get_tree(tree_id) {
        Ok(Some(meta)) => {
            if let Some(repo_path) = &meta.repo_path {
                if repo_path.exists() {
                    return repo_path.clone();
                }
                warn!(
                    "Repo path {:?} for tree {} does not exist, using cwd",
                    repo_path, tree_id
                );
            }
        }
        Ok(None) => warn!("Tree {} not found, using cwd", tree_id),
        Err(e) => warn!("Failed to get tree {}: {}, using cwd", tree_id, e),
    }
    std::env::current_dir().unwrap_or_else(|_| store.base_dir().clone())
}

pub fn emit_event(out: &mut BufWriter<std::io::Stdout>, event: ServerEvent) {
    let ev_debug = match &event {
        ServerEvent::TextChunk { content } => format!("TextChunk(len={})", content.len()),
        ServerEvent::ThinkingChunk { content } => format!("ThinkingChunk(len={})", content.len()),
        ServerEvent::ToolStart { tool, .. } => format!("ToolStart({})", tool),
        ServerEvent::ToolResult { tool, exit, .. } => format!("ToolResult({}, exit={})", tool, exit),
        ServerEvent::Entry(e) => format!("Entry({})", e.id()),
        ServerEvent::CapWarning { level, pct } => format!("CapWarning({},{}%)", level, pct),
        ServerEvent::Notification { level, message } => format!("Notification({:?}, {})", level, message),
        ServerEvent::Done { status } => format!("Done({})", status),
        ServerEvent::FileChanged { path, kind } => format!("FileChanged({},{})", kind, path),
        ServerEvent::MetaUpdate { .. } => "MetaUpdate".into(),
    };
    log::debug!("emit_event: {}", ev_debug);
    let msg = PipeOut::Event(event);
    if let Ok(json) = serde_json::to_string(&msg) {
        writeln!(out, "{}", json).ok();
        out.flush().ok();
    }
}

pub fn emit_notification(out: &mut BufWriter<std::io::Stdout>, level: NotificationLevel, message: String) {
    emit_event(out, ServerEvent::Notification { level, message });
}

pub fn send_llm_request(
    out: &mut BufWriter<std::io::Stdout>,
    messages: Vec<Message>,
    tools: Vec<ToolDefinition>,
) {
    let n_msg = messages.len();
    let n_tools = tools.len();
    log::info!("send_llm_request: {} messages, {} tools", n_msg, n_tools);
    let req = PipeOut::Llm(LlmRequest { id: 0, messages, tools });
    if let Ok(json) = serde_json::to_string(&req) {
        log::debug!(
            "send_llm_request pipe out: {}",
            json.chars().take(200).collect::<String>()
        );
        writeln!(out, "{}", json).ok();
        out.flush().ok();
    }
}

pub fn write_message_entry(
    store: &Store,
    tree_id: &str,
    out: &mut BufWriter<std::io::Stdout>,
    entry_id: &str,
    parent_id: Option<&str>,
    message: &Message,
) {
    let entry = Entry::Message {
        id: entry_id.to_string(),
        parent_id: parent_id.map(|s| s.to_string()),
        timestamp: chrono::Utc::now().to_rfc3339(),
        message: message.clone(),
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        error!("Failed to append message entry for tree {}: {}", tree_id, e);
    }
    emit_event(out, ServerEvent::Entry(entry));
}

pub fn write_session_end(
    store: &Store,
    tree_id: &str,
    out: &mut BufWriter<std::io::Stdout>,
    status: SessionStatus,
    continuation_brief: Option<String>,
) {
    let entry = Entry::SessionEnd {
        id: agent_core::util::generate_entry_id(),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        summary: None,
        status,
        continuation_brief,
    };
    if let Err(e) = store.append_entry(tree_id, &entry) {
        error!("Failed to write session_end for tree {}: {}", tree_id, e);
    }
    emit_event(out, ServerEvent::Entry(entry));
}