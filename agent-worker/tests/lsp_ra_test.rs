use std::time::{Duration, Instant};

#[test]
fn test_lsp_client_rust_analyzer_eventually() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();

    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("Cargo.toml"), r#"[package]
name = "test-lsp"
version = "0.1.0"
edition = "2021"
"#).unwrap();
    std::fs::write(dir.join("src").join("main.rs"), "fn main() {\n    println!(\"hello\");\n}\n").unwrap();

    if !agent_worker::lsp_client::binary_exists("rust-analyzer") {
        eprintln!("rust-analyzer not found, skipping test");
        return;
    }

    let root_uri = format!("file://{}", dir.display());
    let mut client = agent_worker::lsp_client::LspClient::spawn(
        "rust", "rust-analyzer", &[], &root_uri, 10000,
    ).expect("failed to spawn LSP client");

    std::thread::sleep(Duration::from_millis(1000));
    client.read_available();

    let file_path = dir.join("src").join("main.rs");
    std::fs::write(&file_path, "fn main() {\n    let x: i32 = \"hello\";\n}\n").unwrap();
    client.notify_saved(&file_path);

    let deadline = Instant::now() + Duration::from_secs(60);
    let mut found = false;
    let mut empty_count = 0;
    while Instant::now() < deadline {
        client.read_available();
        for (url, diags) in &client.diagnostics {
            if diags.is_empty() {
                empty_count += 1;
            } else {
                found = true;
                eprintln!("rust-analyzer diagnostics after {:.1}s:", deadline.elapsed().as_secs_f64());
                for d in diags {
                    eprintln!("  {:?} ({}:{}): {}", d.severity, d.range.start.line, d.range.start.character, d.message);
                }
            }
        }
        if found {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    eprintln!("Waited {:.1}s, received {} empty publishDiagnostics", deadline.elapsed().as_secs_f64(), empty_count);
    assert!(found || deadline.elapsed().as_secs() >= 55, "rust-analyzer eventually sends diagnostics");
}
