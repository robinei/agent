use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use agent_core::provider::SyncPipeProvider;
use agent_core::rpc::{PipeIn, WsCommand};
use agent_core::types::{AgentInput, ServerEvent};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let tree_id = parse_tree_id()?;

    let stdin = Arc::new(Mutex::new(BufReader::new(std::io::stdin())));
    let stdout = Arc::new(Mutex::new(std::io::stdout()));

    // Read the initial WorkerConfig (first line on stdin).
    let config = {
        let mut r = stdin.lock().unwrap();
        let mut buf = String::new();
        match agent_core::rpc::read_json_line::<_, PipeIn>(&mut *r, &mut buf) {
            Ok(Some(PipeIn::Config(cfg))) => cfg,
            Ok(Some(other)) => {
                return Err(format!("expected Config, got {:?}", other).into());
            }
            Ok(None) => return Err("stdin closed before Config".into()),
            Err(e) => return Err(format!("read Config: {}", e).into()),
        }
    };

    let log_file = config.logging_to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging_level);
    agent_core::hooks::run_startup_hooks().ok();

    let store = agent_core::store::Store::default();
    let session_cfg = agent_core::config::SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };
    let stop = Arc::new(AtomicBool::new(false));

    let provider = SyncPipeProvider::new(
        stdin.clone(),
        stdout.clone(),
        stop.clone(),
    );

    // next_input: read PipeIn::Cmd messages from stdin; skip everything else.
    let next_input = {
        let stdin = stdin.clone();
        let stop = stop.clone();
        move || -> Option<AgentInput> {
            loop {
                let mut line = String::new();
                let n = stdin.lock().unwrap().read_line(&mut line).ok()?;
                if n == 0 {
                    return None;
                }
                match serde_json::from_str(line.trim_end()) {
                    Ok(PipeIn::Cmd(WsCommand::Message { params })) => {
                        return Some(AgentInput::Message { text: params.text });
                    }
                    Ok(PipeIn::Cmd(WsCommand::Stop)) => {
                        stop.store(true, Ordering::Relaxed);
                        return Some(AgentInput::Stop);
                    }
                    _ => {} // skip Llm responses, Config, unknown
                }
            }
        }
    };

    // emit: serialize event as PipeOut::Event and write directly to stdout.
    let emit = {
        let stdout = stdout.clone();
        move |event: ServerEvent| {
            let pipe_out = agent_core::rpc::PipeOut::Event(event);
            if let Ok(json) = serde_json::to_string(&pipe_out) {
                let mut w = stdout.lock().unwrap();
                writeln!(w, "{}", json).ok();
                w.flush().ok();
            }
        }
    };

    agent_core::agent::run_agent(
        &tree_id, store, provider, session_cfg, next_input, emit, stop,
    );
    Ok(())
}

fn parse_tree_id() -> Result<String, Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut tree_id = None;
    while let Some(arg) = args.next() {
        if arg.as_str() == "--tree-id" {
            tree_id = Some(args.next().ok_or("missing --tree-id value")?);
        }
    }
    tree_id.ok_or_else(|| "--tree-id is required".into())
}
