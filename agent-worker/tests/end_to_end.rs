use std::io::{BufRead, Write};
use std::path::PathBuf;

#[test]
fn worker_round_trip() {
    let tmp = tempfile::TempDir::new().unwrap();
    let agent_dir = tmp.path().to_path_buf();
    let tree_id = "test-tree-0001";

    // 1. Write a minimal config.toml
    let config_path = agent_dir.join("config.toml");
    std::fs::write(
        &config_path,
        r#"
[provider]
base_url = "http://localhost:8080/v1"
api_key = "test-key"
model = "test-model"

[summary]
base_url = "http://localhost:8080/v1"
api_key = "test-key"
model = "test-model"

[session]
soft_cap_pct = 65
hard_cap_pct = 85
max_tool_calls_per_turn = 25

[logging]
level = "error"
to_stderr = false

[sandbox]
enabled = false
"#,
    )
    .unwrap();

    // 2. Create tree directory and seed data.jsonl with header
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

    // 3. Write meta.json
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
    std::fs::write(tree_dir.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).unwrap();

    // 4. Locate the agent binary
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir.parent().unwrap();
    let profile = std::env::var("PROFILE").unwrap_or_else(|_| "debug".to_string());
    let exe = workspace_root.join("target").join(profile).join("agent");
    assert!(exe.exists(), "agent binary not found at {:?}", exe);

    // 5. Spawn worker subprocess
    let mut child = std::process::Command::new(&exe)
        .arg("worker")
        .arg("--tree-id")
        .arg(tree_id)
        .arg("--config")
        .arg(&config_path)
        .env("AGENT_DIR", &agent_dir)
        .env("AGENT_TEST_STUB", "1")
        .env("RUST_LOG", "error")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    // 6. Send a message
    let mut stdin = child.stdin.take().unwrap();
    writeln!(stdin, r#"{{"method":"message","params":{{"text":"hi"}}}}"#).unwrap();
    // We must drop stdin so the worker sees EOF after processing the message
    drop(stdin);

    // 7. Collect events from stdout (with a timeout)
    let stdout = child.stdout.take().unwrap();
    let reader = std::io::BufReader::new(stdout);
    let mut events: Vec<String> = Vec::new();
    let mut lines = reader.lines();

    // Read events with a 30-second timeout (the agent loop may use mpsc channel
    // buffering, so events may arrive in batches)
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        if std::time::Instant::now() > deadline {
            panic!("Timed out waiting for Done event. Got {} events so far.", events.len());
        }
        match lines.next() {
            Some(Ok(line)) => {
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() {
                    continue;
                }
                // Check if this is Done
                if trimmed.contains(r#""type":"done""#) {
                    events.push(trimmed);
                    break;
                }
                events.push(trimmed);
            }
            Some(Err(e)) => {
                panic!("Error reading worker stdout: {}", e);
            }
            None => {
                panic!("Worker stdout closed before Done event. Got {} events: {:?}", events.len(), events);
            }
        }
    }

    // Drain any remaining stdout
    for line in lines {
        if let Ok(line) = line {
            let trimmed = line.trim().to_string();
            if !trimmed.is_empty() {
                events.push(trimmed);
            }
        }
    }

    // 8. Wait for child to finish
    let status = child.wait().expect("worker process should exit");
    assert!(status.success(), "worker exited with: {}", status);

    // 9. Parse events and assert structure
    let parsed: Vec<serde_json::Value> = events
        .iter()
        .filter_map(|e| serde_json::from_str(e).ok())
        .collect();

    assert!(
        parsed.iter().any(|e| e.get("type").and_then(|t| t.as_str()) == Some("text_chunk")),
        "Expected at least one text_chunk event, got: {:?}",
        parsed
    );
    assert!(
        parsed.iter().any(|e| e.get("type").and_then(|t| t.as_str()) == Some("done")),
        "Expected a done event, got: {:?}",
        parsed
    );

    // 10. Tree data.jsonl should contain the user message and assistant message
    let store = agent_core::store::Store::new(agent_dir);
    let entries = store.read_all_entries(tree_id).unwrap();
    let has_user = entries.iter().any(|e| {
        matches!(e, agent_core::types::Entry::Message { message, .. } if matches!(message.role, agent_core::types::MessageRole::User))
    });
    let has_assistant = entries.iter().any(|e| {
        matches!(e, agent_core::types::Entry::Message { message, .. } if matches!(message.role, agent_core::types::MessageRole::Assistant))
    });
    assert!(has_user, "No User message entry found in store");
    assert!(has_assistant, "No Assistant message entry found in store");
}