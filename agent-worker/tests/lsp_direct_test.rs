use std::time::{Duration, Instant};

#[test]
fn test_lsp_client_diagnostics() {
    let tmp = tempfile::TempDir::new().unwrap();
    let dir = tmp.path();

    // Create a C file with a deliberate type error
    let file_path = dir.join("test_buggy.c");
    std::fs::write(
        &file_path,
        "#include <stdio.h>\n\nint main() {\n    int x = \"hello\";\n    return 0;\n}\n",
    )
    .unwrap();
    eprintln!("file_path: {:?}", file_path);

    // Check if clangd is available
    if !agent_worker::lsp_client::binary_exists("clangd") {
        eprintln!("clangd not found, skipping test");
        return;
    }

    let root_uri = format!("file://{}", dir.display());
    eprintln!("root_uri: {}", root_uri);

    let mut client = agent_worker::lsp_client::LspClient::spawn(
        "c",
        "clangd",
        &[],
        &root_uri,
        10000,
    )
    .expect("failed to spawn LSP client");

    // Let the client settle and consume initial messages
    std::thread::sleep(Duration::from_millis(500));
    client.read_available();

    eprintln!("=== Opening file with didOpen ===");
    client.notify_saved(&file_path);

    // Poll for up to 10 seconds
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut found_non_empty = false;
    while Instant::now() < deadline {
        client.read_available();
        for (url, diags) in &client.diagnostics {
            if !diags.is_empty() {
                found_non_empty = true;
                eprintln!("=== GOT DIAGNOSTICS for {} ===", url.as_str());
                for d in diags {
                    eprintln!("  {:?} ({}:{}): {}",
                        d.severity,
                        d.range.start.line, d.range.start.character,
                        d.message);
                }
            }
        }
        if found_non_empty {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    if !found_non_empty {
        eprintln!("FINAL state after timeout:");
        for (url, diags) in &client.diagnostics {
            eprintln!("  Url: {} has {} diagnostics", url.as_str(), diags.len());
        }
    }

    assert!(found_non_empty, "Expected at least one non-empty diagnostic from clangd");
}
