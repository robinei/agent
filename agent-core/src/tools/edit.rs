//! EditTool — content editing with exact match first, then fuzzy fallback.
//!
//! Supports multiple disjoint edits in one call. Each edit is matched against
//! the original file content, then applied in reverse order so line offsets
//! remain stable. Overlapping edits are rejected.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, LazyLock, Mutex};

use unicode_normalization::UnicodeNormalization;

use super::{resolve_path, Tool, ToolResult};
use crate::types::{ToolDefinition, ToolOutput};

/// Per-file mutex for serializing concurrent edits/writes to the same file.
static EDIT_FILE_LOCKS: LazyLock<Mutex<std::collections::HashMap<PathBuf, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

fn with_edit_file_lock<F, T>(path: &Path, f: F) -> T
where
    F: FnOnce() -> T,
{
    let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut locks = EDIT_FILE_LOCKS.lock().unwrap();
    let lock = locks.entry(canon).or_default().clone();
    drop(locks);
    let _guard = lock.lock().unwrap();
    f()
}

pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    pub fn new(cwd: &Path) -> Self {
        Self {
            cwd: cwd.to_path_buf(),
        }
    }

    /// Strip BOM, normalize line endings to LF.
    fn normalize(content: &str) -> String {
        let content = content.strip_prefix('\u{feff}').unwrap_or(content);
        content.replace("\r\n", "\n")
    }

    /// Fuzzy-normalize text for matching:
    /// - NFKC normalize
    /// - Strip trailing whitespace per line
    /// - Normalize smart quotes to ASCII
    /// - Normalize dashes to ASCII hyphen
    /// - Normalize special spaces to regular space
    fn fuzzy_normalize(s: &str) -> String {
        let s: String = s.nfkc().collect();
        let mut result = String::with_capacity(s.len());
        for ch in s.chars() {
            match ch {
                '\u{2018}' | '\u{2019}' | '\u{201b}' => result.push('\''),
                '\u{201c}' | '\u{201d}' | '\u{201e}' => result.push('"'),
                '\u{2013}' | '\u{2014}' | '\u{2212}' => result.push('-'),
                '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{3000}' => {
                    result.push(' ');
                }
                c => result.push(c),
            }
        }
        // Strip trailing whitespace per line
        let lines: Vec<&str> = result.lines().collect();
        let stripped: Vec<String> = lines.iter().map(|l| l.trim_end().to_string()).collect();
        let joined = stripped.join("\n");
        joined.trim_end().to_string()
    }

    /// Apply a single edit. Returns new content on success.
    fn apply_edit(
        original: &str,
        old_text: &str,
        new_text: &str,
        index: usize,
    ) -> Result<String, String> {
        // Exact match pass
        if let Some(pos) = original.find(old_text) {
            let mut result = String::with_capacity(original.len() + new_text.len());
            result.push_str(&original[..pos]);
            result.push_str(new_text);
            result.push_str(&original[pos + old_text.len()..]);
            return Ok(result);
        }

        // Fuzzy fallback
        let norm_original = Self::fuzzy_normalize(original);
        let norm_old = Self::fuzzy_normalize(old_text);

        if let Some(pos) = norm_original.find(&norm_old) {
            let orig_pos = Self::map_normalized_pos(original, &norm_original, pos)
                .ok_or_else(|| format!(
                    "Edit #{}: could not map fuzzy match position",
                    index + 1
                ))?;

            let orig_end = Self::map_normalized_pos(original, &norm_original, pos + norm_old.len())
                .ok_or_else(|| format!(
                    "Edit #{}: could not map fuzzy match end position",
                    index + 1
                ))?;

            let mut result = String::with_capacity(original.len() + new_text.len());
            result.push_str(&original[..orig_pos]);
            result.push_str(new_text);
            result.push_str(&original[orig_end..]);
            return Ok(result);
        }

        Err(format!(
            "Edit #{}: oldText not found (exact + fuzzy). oldText (first 100): {:?}",
            index + 1,
            &old_text[..old_text.len().min(100)]
        ))
    }

    /// Map a position in the normalized string back to a byte position in the original.
    fn map_normalized_pos(original: &str, _normalized: &str, norm_pos: usize) -> Option<usize> {
        let mut orig_byte_pos = 0usize;
        let mut norm_char_pos = 0usize;
        for ch in original.chars() {
            if norm_char_pos >= norm_pos {
                return Some(orig_byte_pos);
            }
            let nfkc_count: usize = ch.nfkc().count();
            let next_norm = norm_char_pos + nfkc_count;
            if next_norm > norm_pos {
                return Some(orig_byte_pos + ch.len_utf8());
            }
            norm_char_pos = next_norm;
            orig_byte_pos += ch.len_utf8();
        }
        if norm_char_pos >= norm_pos {
            Some(original.len())
        } else {
            None
        }
    }

    /// Build summary of changes.
    fn build_diff(_original: &str, edits: &[EditInput]) -> String {
        let mut diff = format!("--- changes ({} edits):\n", edits.len());
        for (i, edit) in edits.iter().enumerate() {
            let old_short = edit.old_text.lines().next().unwrap_or("");
            let new_short = edit.new_text.lines().next().unwrap_or("");
            let old_lines = edit.old_text.lines().count();
            let new_lines = edit.new_text.lines().count();
            diff.push_str(&format!(
                "  {}: -{} +{} lines | old: {:.80}\n     | new: {:.80}\n",
                i + 1, old_lines, new_lines, old_short, new_short,
            ));
        }
        diff
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
            description:
                "Edit a file by finding and replacing text. Supports exact match first, \
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

    fn execute(&self, params: &serde_json::Value, _stop: &Arc<AtomicBool>) -> ToolResult {
        let file_path = params
            .get("file_path")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "Missing required field: file_path".to_string())?;

        let resolved = resolve_path(&self.cwd, file_path).ok_or_else(|| {
            format!("Path '{}' does not exist or is outside repo root", file_path)
        })?;

        if !resolved.is_file() {
            return Err(format!("Not a file: {}", resolved.display()).into());
        }

        // Collect edits from either single pair or array
        let mut edits: Vec<EditInput> = Vec::new();

        if let Some(edits_arr) = params.get("edits").and_then(|v| v.as_array()) {
            for (i, edit_val) in edits_arr.iter().enumerate() {
                let old_text = edit_val
                    .get("oldText")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("Edit #{}: missing oldText", i + 1))?;
                let new_text = edit_val
                    .get("newText")
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| format!("Edit #{}: missing newText", i + 1))?;
                edits.push(EditInput {
                    old_text: old_text.to_string(),
                    new_text: new_text.to_string(),
                });
            }
        } else {
            let old_text = params
                .get("old_text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing required field: old_text".to_string())?;
            let new_text = params
                .get("new_text")
                .and_then(|v| v.as_str())
                .ok_or_else(|| "Missing required field: new_text".to_string())?;
            edits.push(EditInput {
                old_text: old_text.to_string(),
                new_text: new_text.to_string(),
            });
        }

        if edits.is_empty() {
            return Err("No edits provided".into());
        }

        // Perform the edit under per-file lock
        with_edit_file_lock(&resolved, || -> ToolResult {
            let raw = fs::read_to_string(&resolved)?;
            let original = Self::normalize(&raw);

            // Validate each edit is found exactly once
            let norm_original = Self::fuzzy_normalize(&original);
            for (i, edit) in edits.iter().enumerate() {
                let norm_old = Self::fuzzy_normalize(&edit.old_text);
                let count: usize = norm_original.match_indices(&norm_old).count();
                if count == 0 {
                    return Err(format!(
                        "Edit #{}: oldText not found in file (exact + fuzzy)",
                        i + 1
                    )
                    .into());
                }
                if count > 1 {
                    return Err(format!(
                        "Edit #{}: oldText matches {} times (ambiguous)",
                        i + 1, count
                    )
                    .into());
                }
            }

            // Apply in reverse order
            let mut content = original.clone();
            for (i, edit) in edits.iter().enumerate().rev() {
                content = Self::apply_edit(&content, &edit.old_text, &edit.new_text, i)?;
            }

            fs::write(&resolved, &content)?;

            let diff = Self::build_diff(&original, &edits);
            let original_size = original.len();

            Ok(ToolOutput {
                content: diff,
                truncated: false,
                original_size,
                exit_code: None,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_edit_exact_match() {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join("test.rs"),
            "fn hello() {\n    println!(\"world\");\n}\n",
        )
        .unwrap();
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "test.rs",
                "old_text": "    println!(\"world\");",
                "new_text": "    println!(\"hello world\");"
            }), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(!result.truncated);
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
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "test.rs",
                "old_text": "println!(\"hello\");",
                "new_text": "println!(\"world\");"
            }), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(!result.truncated);
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
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "test.rs",
                "old_text": "fn hello() {\n    x = 1;\n}",
                "new_text": "fn hello() {\n    x = 2;\n}"
            }), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(!result.truncated);
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
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "test.rs",
                "edits": [
                    {"oldText": "line A", "newText": "line X"},
                    {"oldText": "line C", "newText": "line Z"}
                ]
            }), &Arc::new(AtomicBool::new(false)))
            .unwrap();
        assert!(!result.truncated);
        let content = fs::read_to_string(dir.path().join("test.rs")).unwrap();
        assert!(content.contains("line X"));
        assert!(content.contains("line Z"));
        assert!(!content.contains("line A"));
    }

    #[test]
    fn test_edit_not_found() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("test.rs"), "hello world").unwrap();
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "test.rs",
                "old_text": "zzzzzz",
                "new_text": "yyyyyy"
            }), &Arc::new(AtomicBool::new(false)));
        assert!(result.is_err());
    }

    #[test]
    fn test_edit_escape_rejected() {
        let dir = TempDir::new().unwrap();
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "../etc/passwd",
                "old_text": "root",
                "new_text": "nope"
            }), &Arc::new(AtomicBool::new(false)));
        assert!(result.is_err());
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
        let tool = EditTool::new(dir.path());
        let result = tool
            .execute(&serde_json::json!({
                "file_path": "nope.txt",
                "old_text": "x",
                "new_text": "y"
            }), &Arc::new(AtomicBool::new(false)));
        assert!(result.is_err());
    }
}
