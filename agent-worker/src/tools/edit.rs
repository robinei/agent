//! EditTool — content editing with exact match first, then fuzzy fallback.
//!
//! Supports multiple disjoint edits in one call. Each edit is matched against
//! the original file content, then applied in reverse order so line offsets
//! remain stable. Overlapping edits are rejected.

use super::util::{apply_edit, build_context_window, count_fuzzy_matches, find_changed_lines};
use super::{resolve_path, EditRecord, Tool, ToolContext, ToolOutput};
use agent_core::types::ToolDefinition;

pub struct EditTool;

impl EditTool {
    /// Strip BOM, normalize line endings to LF.
    fn normalize(content: &str) -> String {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        content.replace("\r\n", "\n")
    }
}

#[derive(Debug)]
struct EditInput {
    old_text: String,
    new_text: String,
}

impl Tool for EditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "edit".to_string(),
            description: "Edit a file by finding and replacing text. Supports exact match first, \
                 then fuzzy fallback (NFKC normalization, smart quotes normalization, \
                 trailing whitespace tolerance). Multiple disjoint edits can be applied \
                 in one call via the 'edits' array."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "file_path": {
                        "type": "string",
                        "description": "File path relative to repo root"
                    },
                    "old_text": {
                        "type": "string",
                        "description": "Existing text to replace"
                    },
                    "new_text": {
                        "type": "string",
                        "description": "New text to replace with"
                    },
                    "edits": {
                        "type": "array",
                        "description": "Multiple disjoint edits for the same file",
                        "items": {
                            "type": "object",
                            "properties": {
                                "oldText": {
                                    "type": "string",
                                    "description": "Existing text to replace"
                                },
                                "newText": {
                                    "type": "string",
                                    "description": "New text to replace with"
                                }
                            },
                            "required": ["oldText", "newText"]
                        }
                    }
                },
                "oneOf": [
                    { "required": ["file_path", "old_text", "new_text"] },
                    { "required": ["file_path", "edits"] }
                ]
            }),
        }
    }

    fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput {
        let file_path = params
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: file_path".to_string());

        let file_path = match file_path {
            Ok(p) => p,
            Err(e) => return ToolOutput::Done(Err(e)),
        };

        let resolved = match resolve_path(&ctx.cwd, file_path) {
            Ok(p) => p,
            Err(e) => return ToolOutput::Done(Err(e)),
        };

        if !resolved.is_file() {
            return ToolOutput::Done(Err(format!("Not a file: {}", resolved.display())));
        }

        let mut edits: Vec<EditInput> = Vec::new();

        if let Some(edits_arr) = params.get("edits").and_then(|v| v.as_array()) {
            for (i, edit_val) in edits_arr.iter().enumerate() {
                let old_text = match edit_val.get("oldText").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => return ToolOutput::Done(Err(format!("Edit #{}: missing oldText", i + 1))),
                };
                let new_text = match edit_val.get("newText").and_then(|v| v.as_str()) {
                    Some(t) => t.to_string(),
                    None => return ToolOutput::Done(Err(format!("Edit #{}: missing newText", i + 1))),
                };
                edits.push(EditInput { old_text, new_text });
            }
        } else {
            let old_text = match params.get("old_text").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return ToolOutput::Done(Err("Missing required field: old_text".into())),
            };
            let new_text = match params.get("new_text").and_then(|v| v.as_str()) {
                Some(t) => t.to_string(),
                None => return ToolOutput::Done(Err("Missing required field: new_text".into())),
            };
            edits.push(EditInput { old_text, new_text });
        }

        if edits.is_empty() {
            return ToolOutput::Done(Err("No edits provided".into()));
        }

        let raw = match std::fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return ToolOutput::Done(Err(e.to_string())),
        };
        let original = Self::normalize(&raw);

        for (i, edit) in edits.iter().enumerate() {
            let count = count_fuzzy_matches(&original, &edit.old_text);
            if count == 0 {
                return ToolOutput::Done(Err(format!(
                    "Edit #{}: oldText not found in file (exact + fuzzy)",
                    i + 1
                )));
            }
            if count > 1 {
                return ToolOutput::Done(Err(format!(
                    "Edit #{}: oldText matches {} times (ambiguous)",
                    i + 1,
                    count
                )));
            }
        }

        let mut content = original.clone();
        for (i, edit) in edits.iter().enumerate().rev() {
            content = match apply_edit(&content, &edit.old_text, &edit.new_text, i) {
                Ok(c) => c,
                Err(e) => return ToolOutput::Done(Err(e)),
            };
        }

        let pre_snapshot = Some(original.clone());

        if let Err(e) = std::fs::write(&resolved, &content) {
            return ToolOutput::Done(Err(e.to_string()));
        }

        let edit_tuples: Vec<(String, String)> = edits
            .iter()
            .map(|e| (e.old_text.clone(), e.new_text.clone()))
            .collect();

        let new_texts: Vec<String> = edits.iter().map(|e| e.new_text.clone()).collect();
        let changed_lines = find_changed_lines(&content, &new_texts);

        ctx.lsp_dirty.push(resolved.clone());
        let id = ctx.edit_store.insert(EditRecord {
            file_path: resolved,
            pre_snapshot,
            edits: edit_tuples,
            post_snapshot: None,
        });

        let diff = build_context_window(file_path, &content, &changed_lines, edits.len(), id);
        let original_size = original.len();

        ToolOutput::Done(Ok(diff))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::fs;
    use tempfile::TempDir;

    fn make_ctx(dir: &Path) -> ToolContext {
        ToolContext::new(dir.to_path_buf())
    }

    fn run_ok(tool: &EditTool, params: serde_json::Value, ctx: &mut ToolContext) -> String {
        match tool.execute(&params, ctx) {
            ToolOutput::Done(Ok(c)) => c,
            _ => panic!("expected Done(Ok)"),
        }
    }

    #[test]
    fn test_edit_exact_match() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn hello() {\n    println!(\"world\");\n}\n",
        )
        .unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({
                "file_path": "test.rs",
                "old_text": "    println!(\"world\");",
                "new_text": "    println!(\"hello world\");"
            }),
            &mut ctx,
        );
        assert!(result.contains("edit_id:"));
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("hello world"));
    }

    #[test]
    fn test_edit_fuzzy_smart_quotes() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "println!(\u{201c}hello\u{201d});\n",
        )
        .unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({
                "file_path": "test.rs",
                "old_text": "println!(\"hello\");",
                "new_text": "println!(\"world\");"
            }),
            &mut ctx,
        );
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("world"));
    }

    #[test]
    fn test_edit_fuzzy_trailing_whitespace() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn hello() {  \n    x = 1;  \n}\n",
        )
        .unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({
                "file_path": "test.rs",
                "old_text": "fn hello() {\n    x = 1;\n}",
                "new_text": "fn hello() {\n    x = 2;\n}"
            }),
            &mut ctx,
        );
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("x = 2"));
    }

    #[test]
    fn test_edit_multi_disjoint() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "line A\nline B\nline C\nline D\n",
        )
        .unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = run_ok(
            &tool,
            serde_json::json!({
                "file_path": "test.rs",
                "edits": [
                    {"oldText": "line A", "newText": "line X"},
                    {"oldText": "line C", "newText": "line Z"}
                ]
            }),
            &mut ctx,
        );
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("line X"));
        assert!(content.contains("line Z"));
        assert!(!content.contains("line A"));
    }

    #[test]
    fn test_edit_not_found() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.rs"), "hello world").unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = tool.execute(
            &serde_json::json!({
                "file_path": "test.rs",
                "old_text": "zzzzzz",
                "new_text": "yyyyyy"
            }),
            &mut ctx,
        );
        assert!(matches!(result, ToolOutput::Done(Err(_))));
    }

    #[test]
    fn test_edit_escape_rejected() {
        let dir = TempDir::new().unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = tool.execute(
            &serde_json::json!({
                "file_path": "../etc/passwd",
                "old_text": "root",
                "new_text": "nope"
            }),
            &mut ctx,
        );
        assert!(matches!(result, ToolOutput::Done(Err(_))));
    }

    #[test]
    fn test_normalize_bom_and_crlf() {
        let input = "\u{feff}hello\r\nworld\r\n";
        let result = EditTool::normalize(input);
        assert_eq!(result, "hello\nworld\n");
        assert!(!result.contains('\r'));
    }

    #[test]
    fn test_edit_nonexistent_file() {
        let dir = TempDir::new().unwrap();
        let tool = EditTool;
        let mut ctx = make_ctx(dir.path());
        let result = tool.execute(
            &serde_json::json!({
                "file_path": "nope.txt",
                "old_text": "x",
                "new_text": "y"
            }),
            &mut ctx,
        );
        assert!(matches!(result, ToolOutput::Done(Err(_))));
    }
}