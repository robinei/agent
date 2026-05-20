use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use agent_core::provider::PipeProvider;
use agent_core::rpc::{PipeIn, PipeOut, WorkerConfig};
use agent_core::types::{AgentInput, ServerEvent};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let tree_id = parse_tree_id()?;

    // Read WorkerConfig from the first PipeIn message on stdin.
    // Must happen on the main thread before spawning the stdin-reader thread.
    let config: WorkerConfig = {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buf = String::new();
        match agent_core::rpc::read_json_line::<_, PipeIn>(&mut reader, &mut buf) {
            Ok(Some(PipeIn::Config(cfg))) => cfg,
            Ok(Some(other)) => return Err(format!("Expected Config as first message, got {:?}", other).into()),
            Ok(None) => return Err("stdin closed before Config message".into()),
            Err(e) => return Err(format!("Failed to read initial Config: {}", e).into()),
        }
    };

    let log_file = config.logging_to_file.as_ref().and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging_level);
    agent_core::hooks::run_startup_hooks().ok();

    let store = agent_core::store::Store::default();

    let session_config = agent_core::config::SessionConfig {
        soft_cap_pct: config.session_soft_cap_pct,
        hard_cap_pct: config.session_hard_cap_pct,
        max_tool_calls_per_turn: config.max_tool_calls_per_turn,
    };

    let (input_tx, input_rx) = mpsc::channel::<AgentInput>();
    let (event_tx, event_rx) = mpsc::channel::<ServerEvent>();
    let stop = Arc::new(AtomicBool::new(false));

    // Channel for serialised PipeOut strings written to stdout
    let (out_tx, out_rx) = mpsc::channel::<String>();

    // Channel for per-request LlmResponse senders (registered by PipeProvider)
    let (llm_register_tx, llm_register_rx) = mpsc::channel::<mpsc::Sender<agent_core::rpc::LlmResponse>>();

    // Bridge: wrap ServerEvents in PipeOut::Event, serialise, send to out_tx
    let out_tx_for_bridge = out_tx.clone();
    std::thread::spawn(move || {
        for event in event_rx {
            let pipe_out = PipeOut::Event(event);
            if let Ok(json) = serde_json::to_string(&pipe_out) {
                let _ = out_tx_for_bridge.send(json);
            }
        }
    });

    // Stdin-reader thread: parse PipeIn envelope, dispatch Cmd/Llm
    let stop_for_stdin = stop.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buf = String::new();
        let mut current_llm_tx: Option<mpsc::Sender<agent_core::rpc::LlmResponse>> = None;
        loop {
            // Drain registrations before dispatching so the sender is installed
            // before the first Chunk for that request arrives.
            while let Ok(tx) = llm_register_rx.try_recv() {
                current_llm_tx = Some(tx);
            }
            match agent_core::rpc::read_json_line::<_, PipeIn>(&mut reader, &mut buf) {
                Ok(Some(parsed)) => {
                    match parsed {
                        PipeIn::Cmd(cmd) => match cmd {
                            agent_core::rpc::WsCommand::Message { params } => {
                                let _ = input_tx.send(AgentInput::Message { text: params.text });
                            }
                            agent_core::rpc::WsCommand::Stop => {
                                stop_for_stdin.store(true, Ordering::Relaxed);
                                let _ = input_tx.send(AgentInput::Stop);
                            }
                        },
                        PipeIn::Llm(resp) => {
                            // INVARIANT: PipeProvider registers the sender (via
                            // llm_register_tx) *before* sending the PipeOut::Llm
                            // request. However, if read_line returned the response
                            // before the next try_recv, retry the drain now.
                            if current_llm_tx.is_none() {
                                while let Ok(tx) = llm_register_rx.try_recv() {
                                    current_llm_tx = Some(tx);
                                }
                            }
                            if let Some(tx) = &current_llm_tx {
                                let _ = tx.send(resp);
                            }
                        }
                        PipeIn::Config(_) => {}
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    // Stdout-writer thread: read serialised PipeOut strings, write to stdout
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        for line in out_rx {
            let _ = writeln!(writer, "{}", line);
            let _ = writer.flush();
        }
    });

    let pipe_provider = PipeProvider::new(out_tx, llm_register_tx);

    agent_core::agent::run_agent(
        &tree_id, store, pipe_provider, session_config, input_rx, event_tx, stop,
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