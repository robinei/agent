use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use agent_core::types::{AgentInput, ServerEvent};

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let (tree_id, config_path) = parse_argv()?;
    let config = agent_core::config::load_config_from_path(&config_path);
    let log_file = config
        .logging
        .to_file
        .as_ref()
        .and_then(|p| p.to_str());
    agent_core::logging::init_logging(log_file, &config.logging.level);
    agent_core::hooks::run_startup_hooks().ok();

    let store = agent_core::store::Store::default();
    let provider = agent_core::provider::Provider::new(
        config.provider.base_url.clone(),
        config.provider.api_key.clone(),
        config.provider.model.clone(),
    );

    let (input_tx, input_rx) = mpsc::channel::<AgentInput>();
    let (event_tx, event_rx) = mpsc::channel::<ServerEvent>();
    let stop = Arc::new(AtomicBool::new(false));

    let stop_for_stdin = stop.clone();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        let mut buf = String::new();
        loop {
            match agent_core::rpc::read_json_line::<_, agent_core::rpc::WsCommand>(
                &mut reader,
                &mut buf,
            ) {
                Ok(Some(agent_core::rpc::WsCommand::Message { params })) => {
                    let _ = input_tx.send(AgentInput::Message { text: params.text });
                }
                Ok(Some(agent_core::rpc::WsCommand::Stop)) => {
                    stop_for_stdin.store(true, Ordering::Relaxed);
                    let _ = input_tx.send(AgentInput::Stop);
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut writer = stdout.lock();
        for event in event_rx {
            let _ = agent_core::rpc::write_json_line(&mut writer, &event);
        }
    });

    agent_core::agent::run_agent(
        &tree_id, store, provider, config.session, input_rx, event_tx, stop,
    );
    Ok(())
}

fn parse_argv() -> Result<(String, std::path::PathBuf), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let mut tree_id = None;
    let mut config_path = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--tree-id" => {
                tree_id = Some(args.next().ok_or("missing --tree-id value")?);
            }
            "--config" => {
                config_path = Some(std::path::PathBuf::from(
                    args.next().ok_or("missing --config value")?,
                ));
            }
            _ => {}
        }
    }
    Ok((
        tree_id.ok_or("--tree-id is required")?,
        config_path.ok_or("--config is required")?,
    ))
}