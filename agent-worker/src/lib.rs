mod thinking;
mod turn;
mod util;

use std::io::{BufRead, BufReader, BufWriter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use log::{info, warn};

use agent_core::rpc::{LlmResponse, PipeIn, WsCommand};
use agent_core::store::Store;
use agent_core::tools;

use crate::turn::{begin_turn, cancel_turn, finish_response, process_chunk};
use crate::util::{parse_tree_id, read_config, resolve_repo_path};

pub(crate) enum AgentState {
    Idle,
    Streaming {
        messages: Vec<agent_core::types::Message>,
        leaf_id: Option<String>,
        response_text: String,
        in_thinking: bool,
        tool_calls_buf: Vec<agent_core::types::ToolCallBuilder>,
        finish_reason: Option<String>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
    },
}

impl AgentState {
    pub(crate) fn new_streaming(
        messages: Vec<agent_core::types::Message>,
        leaf_id: Option<String>,
        tool_call_round: usize,
        tool_calls_this_turn: usize,
        consecutive_failures: usize,
    ) -> Self {
        AgentState::Streaming {
            messages,
            leaf_id,
            response_text: String::new(),
            in_thinking: false,
            tool_calls_buf: vec![],
            finish_reason: None,
            tool_call_round,
            tool_calls_this_turn,
            consecutive_failures,
        }
    }
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_hook(info);
        let _ = std::io::Write::flush(&mut std::io::stderr());
    }));

    let tree_id = parse_tree_id()?;

    let mut reader = BufReader::new(std::io::stdin());
    let mut line = String::new();
    let config = read_config(&mut reader, &mut line)?;

    let log_file = config.logging_to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging_level);
    agent_core::hooks::run_startup_hooks().ok();

    let store = Store::default();
    let session_cfg = agent_core::config::SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };
    let cwd = resolve_repo_path(&store, &tree_id);
    let tools = tools::all_tools(&cwd);
    let stop = Arc::new(AtomicBool::new(false));

    let mut out = BufWriter::new(std::io::stdout());

    let mut state = AgentState::Idle;

    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let msg: PipeIn = match serde_json::from_str(line.trim_end()) {
            Ok(m) => m,
            Err(e) => {
                warn!("Ignoring unparseable PipeIn: {} — line: {}", e, line.trim_end());
                continue;
            }
        };
        match &msg {
            PipeIn::Cmd(WsCommand::Message { params }) => {
                info!("-> Cmd::Message: {}", params.text)
            }
            PipeIn::Cmd(WsCommand::Stop) => info!("-> Cmd::Stop"),
            PipeIn::Llm(LlmResponse::Chunk { data, .. }) => {
                log::debug!(
                    "-> Llm::Chunk: {}",
                    data.chars().take(100).collect::<String>()
                )
            }
            PipeIn::Llm(LlmResponse::Done { .. }) => info!("-> Llm::Done"),
            PipeIn::Llm(LlmResponse::Error { message, .. }) => info!("-> Llm::Error: {}", message),
            PipeIn::Config(_) => info!("-> Config"),
        }
        match msg {
            PipeIn::Cmd(WsCommand::Message { params }) => {
                if matches!(state, AgentState::Idle) {
                    state = begin_turn(
                        params.text, &tree_id, &store, &session_cfg,
                        &tools, &cwd, &stop, &mut out,
                    );
                }
            }
            PipeIn::Cmd(WsCommand::Stop) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    state = cancel_turn(state, &tree_id, &store, &stop, &mut out);
                } else {
                    stop.store(false, Ordering::Relaxed);
                }
            }
            PipeIn::Llm(LlmResponse::Chunk { data, .. }) => {
                if let AgentState::Streaming { .. } = &mut state {
                    process_chunk(&data, &mut state, &mut out);
                }
            }
            PipeIn::Llm(LlmResponse::Done { .. }) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    state = finish_response(
                        state, &tree_id, &store, &session_cfg,
                        &tools, &stop, &mut out,
                    );
                }
            }
            PipeIn::Llm(LlmResponse::Error { message, .. }) => {
                if matches!(state, AgentState::Streaming { .. }) {
                    crate::util::emit_error(&mut out, message, true);
                    state = AgentState::Idle;
                }
            }
            PipeIn::Config(_) => {}
        }
    }
    Ok(())
}