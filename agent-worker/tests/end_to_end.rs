use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::mpsc;

use agent_core::rpc::{LlmResponse, MessageParams, PipeIn, PipeOut, WorkerConfig, WsCommand};

#[test]
fn worker_round_trip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let agent_dir = tmp.path().to_path_buf();
    let tree_id = "test-tree-0001";

    // 1. Create tree directory and seed data.jsonl with header
    let tree_dir = agent_dir.join("trees").join(tree_id);
    std::fs::create_dir_all(&tree_dir).unwrap();

    let header = serde_json::json!({
        "type": "meta",
        "version": 1,
        "id": tree_id,
        "total_tokens": 0,
        "current_model": "test-model"
    });
    let mut header_line = serde_json::to_string(&header).unwrap();
    header_line.push('\n');
    std::fs::write(tree_dir.join("data.jsonl"), header_line.as_bytes()).unwrap();

    // 2. Write meta.json
    let meta = serde_json::json!({
        "id": tree_id,
        "parent_id": null,
        "repo_path": null,
        "title": null,
        "created_at": 0,
        "updated_at": 0,
        "leaf_id": null,
        "sandbox": {
            "writable": [],
            "network": null,
            "hide": [],
            "unhide": []
        }
    });
    std::fs::write(
        tree_dir.join("meta.json"),
        serde_json::to_string_pretty(&meta).unwrap(),
    )
    .unwrap();

    // 3. Locate the agent binary
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let exe = workspace_root.join("target").join(profile).join("agent");
    assert!(exe.exists(), "agent binary not found at {:?}", exe);

    // 4. Spawn worker subprocess (no --config, no AGENT_TEST_STUB — the test
    //    acts as the LLM stub via the PipeIn/PipeOut protocol)
let mut child = std::process::Command::new(&exe)
        .arg("worker")
        .arg("--tree-id")
        .arg(tree_id)
        .env("AGENT_DIR", &agent_dir)
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();

    // 5. Send PipeIn::Config as the first message (the worker expects this)
    let config = PipeIn::Config(WorkerConfig {
        session_soft_cap_pct: 65,
        session_hard_cap_pct: 85,
        max_tool_calls_per_turn: 25,
        logging_level: "error".into(),
        logging_to_file: None,
        logging_to_stderr: false,
    });
    writeln!(stdin, "{}", serde_json::to_string(&config).unwrap()).unwrap();

    // 6. Send user message as PipeIn::Cmd
    let cmd = PipeIn::Cmd(WsCommand::Message {
        params: MessageParams { text: "hi".into() },
    });
    writeln!(stdin, "{}", serde_json::to_string(&cmd).unwrap()).unwrap();

    // 7. Spawn a stdin-writer thread so we can send LlmResponses from the
    //    reader loop without deadlocking.
    let (resp_tx, resp_rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in resp_rx {
            if writeln!(stdin, "{}", line).is_err() {
                break;
            }
            let _ = stdin.flush();
        }
    });

    // 8. Read PipeOut from stdout, proxy LlmRequests, collect events
    let mut reader = std::io::BufReader::new(stdout);
    let mut events: Vec<String> = Vec::new();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);

    loop {
        if std::time::Instant::now() > deadline {
            panic!(
                "Timed out waiting for Done event. Got {} events so far.",
                events.len()
            );
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!(
                "Worker stdout closed before Done event. Got {} events: {:?}",
                events.len(),
                events
            ),
            Err(e) => panic!("Error reading worker stdout: {}", e),
            Ok(_) => {}
        }
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() {
            continue;
        }

        let pipe_out: PipeOut = match serde_json::from_str(&trimmed) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Skipping unparseable PipeOut line: {} ({})", trimmed, e);
                continue;
            }
        };

        match pipe_out {
            PipeOut::Event(event) => {
                let json = serde_json::to_string(&event).unwrap();
                events.push(json);
                if matches!(event, agent_core::types::ServerEvent::Done { .. }) {
                    break;
                }
            }
            PipeOut::Llm(_req) => {
                // Respond with canned SSE chunks as PipeIn::Llm, mimicking the
                // server's handle_llm_request stub mode.
                let chunks = [
                    r#"data: {"choices":[{"delta":{"content":"Hello! I am an AI assistant."},"index":0,"finish_reason":null}]}"#,
                    "",
                    r#"data: {"choices":[{"delta":{},"index":0,"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
                    "",
                ];
                for chunk in &chunks {
                    let resp = PipeIn::Llm(LlmResponse::Chunk {
                        id: 0,
                        data: format!("{}\n", chunk),
                    });
                    resp_tx
                        .send(serde_json::to_string(&resp).unwrap())
                        .unwrap();
                }
                let done = PipeIn::Llm(LlmResponse::Done { id: 0 });
                resp_tx
                    .send(serde_json::to_string(&done).unwrap())
                    .unwrap();
            }
        }
    }

    // Drop so the stdin-writer thread and worker see EOF
    drop(resp_tx);

    // 9. Wait for child to finish
    let status = child.wait().expect("worker process should exit");
    assert!(status.success(), "worker exited with: {}", status);

    // 10. Parse events and assert structure
    let parsed: Vec<serde_json::Value> = events
        .iter()
        .filter_map(|e| serde_json::from_str(e).ok())
        .collect();

    assert!(
        parsed
            .iter()
            .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("text_chunk")),
        "Expected at least one text_chunk event, got: {:?}",
        parsed
    );
    assert!(
        parsed
            .iter()
            .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("done")),
        "Expected a done event, got: {:?}",
        parsed
    );

    // 11. Tree data.jsonl should contain the user message and assistant message
    let store = agent_core::store::Store::new(agent_dir);
    let entries = store.read_all_entries(tree_id).unwrap();
    let has_user = entries.iter().any(|e| {
        matches!(
            e,
            agent_core::types::Entry::Message { message, .. }
                if matches!(message.role, agent_core::types::MessageRole::User)
        )
    });
    let has_assistant = entries.iter().any(|e| {
        matches!(
            e,
            agent_core::types::Entry::Message { message, .. }
                if matches!(message.role, agent_core::types::MessageRole::Assistant)
        )
    });
    assert!(has_user, "No User message entry found in store");
    assert!(has_assistant, "No Assistant message entry found in store");
}