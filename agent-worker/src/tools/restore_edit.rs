//! RestoreEditTool — undo or redo previously recorded edits.

use super::util::{apply_edit, count_fuzzy_matches};
use super::{resolve_path, EditRecord, Tool, ToolContext, ToolOutput};
use agent_core::types::ToolDefinition;
use log::warn;

pub struct RestoreEditTool;

/// Restore a file to its pre-edit state (or delete it if it didn't exist).
async fn restore_pre(resolved: &std::path::Path, record: &EditRecord) -> Result<(), std::io::Error> {
    match &record.pre_snapshot {
        Some(content) => tokio::fs::write(resolved, content).await,
        None => {
            let _ = tokio::fs::remove_file(resolved).await;
            Ok(())
        }
    }
}

fn restore_pre_msg(record: &EditRecord, id: u64) -> String {
    match &record.pre_snapshot {
        Some(_) => format!("Restored file to pre-edit snapshot (edit {})", id),
        None => format!("Deleted file (it did not exist before edit {})", id),
    }
}

#[async_trait::async_trait]
impl Tool for RestoreEditTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "restore_edit".to_string(),
            description: "Restore a file to a state relative to a previously recorded edit. \
                 Use this instead of git checkout when an edit needs to be undone. \
                 Modes:\n\
                 - revert_patch (default): surgically undo the edit by reversing the \
                   string replacements; falls back to pre_snapshot if the patched text \
                   is no longer present\n\
                 - apply_patch: re-apply the edit (useful after a revert)\n\
                 - pre_snapshot: restore the full file to its state before the edit\n\
                 - post_snapshot: restore the full file to its state immediately after \
                   the edit"
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "integer",
                        "description": "The edit ID returned by a previous edit tool call."
                    },
                    "mode": {
                        "type": "string",
                        "enum": ["revert_patch", "apply_patch", "pre_snapshot", "post_snapshot"],
                        "description": "Which state to restore. Defaults to revert_patch."
                    }
                },
                "required": ["id"]
            }),
        }
    }

    async fn execute(&self, params: &serde_json::Value, ctx: &mut ToolContext) -> ToolOutput {
        let id = match params.get("id").and_then(|v| v.as_u64()) {
            Some(i) => i,
            None => {
                return ToolOutput::Done(Err("Missing or invalid required field: id".to_string()))
            }
        };

        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("revert_patch");

        let record = match ctx.edit_store.get(id) {
            Some(r) => r.clone(),
            None => return ToolOutput::Done(Err(format!("No edit with id {} found", id))),
        };

        let resolved = match resolve_path(&ctx.cwd, &record.file_path.to_string_lossy()).await {
            Ok(p) => p,
            Err(e) => return ToolOutput::Done(Err(e)),
        };

        match mode {
            "pre_snapshot" => {
                if record.reverted {
                    return ToolOutput::Done(Err(format!("Edit {} has already been reverted", id)));
                }
                if let Err(e) = restore_pre(&resolved, &record).await {
                    return ToolOutput::Done(Err(e.to_string()));
                }
                if let Some(r) = ctx.edit_store.get_mut(id) {
                    r.reverted = true;
                }
                ToolOutput::Done(Ok(restore_pre_msg(&record, id)))
            }

            "post_snapshot" => {
                let content = match &record.post_snapshot {
                    Some(c) => c.clone(),
                    None => {
                        let base = record.pre_snapshot.as_deref().unwrap_or("");
                        let mut c = base.to_string();
                        for (old, new) in &record.edits {
                            match apply_edit(&c, old, new, 0) {
                                Ok(r) => c = r,
                                Err(e) => return ToolOutput::Done(Err(e)),
                            }
                        }
                        c
                    }
                };
                if let Err(e) = tokio::fs::write(&resolved, &content).await {
                    return ToolOutput::Done(Err(e.to_string()));
                }
                ToolOutput::Done(Ok(format!(
                    "Restored file to post-edit snapshot (edit {})",
                    id
                )))
            }

            "apply_patch" => {
                if record.edits.is_empty() {
                    let content = match &record.post_snapshot {
                        Some(c) => c.clone(),
                        None => {
                            let base = record.pre_snapshot.as_deref().unwrap_or("");
                            let mut c = base.to_string();
                            for (old, new) in &record.edits {
                                match apply_edit(&c, old, new, 0) {
                                    Ok(r) => c = r,
                                    Err(e) => return ToolOutput::Done(Err(e)),
                                }
                            }
                            c
                        }
                    };
                    if let Err(e) = tokio::fs::write(&resolved, &content).await {
                        return ToolOutput::Done(Err(e.to_string()));
                    }
                    if let Some(r) = ctx.edit_store.get_mut(id) {
                        r.reverted = false;
                    }
                    return ToolOutput::Done(Ok(format!("Re-applied (write) for edit {}", id)));
                }

                let current = match tokio::fs::read_to_string(&resolved).await {
                    Ok(c) => c,
                    Err(e) => return ToolOutput::Done(Err(e.to_string())),
                };
                let mut content = current.clone();
                for (i, (old, new)) in record.edits.iter().enumerate() {
                    let count = count_fuzzy_matches(&content, old);
                    if count == 0 {
                        return ToolOutput::Done(Err(format!(
                            "apply_patch: oldText not found for sub-edit {} (edit {})",
                            i, id
                        )));
                    }
                    if count > 1 {
                        return ToolOutput::Done(Err(format!(
                            "apply_patch: oldText matches {} times (ambiguous) for sub-edit {} (edit {})",
                            count, i, id
                        )));
                    }
                    match apply_edit(&content, old, new, i) {
                        Ok(r) => content = r,
                        Err(e) => return ToolOutput::Done(Err(e)),
                    }
                }
                if let Err(e) = tokio::fs::write(&resolved, &content).await {
                    return ToolOutput::Done(Err(e.to_string()));
                }
                if let Some(r) = ctx.edit_store.get_mut(id) {
                    r.reverted = false;
                }
                ToolOutput::Done(Ok(format!("Re-applied patch for edit {}", id)))
            }

            "revert_patch" => {
                if record.reverted {
                    return ToolOutput::Done(Err(format!("Edit {} has already been reverted", id)));
                }

                if record.edits.is_empty() {
                    if let Err(e) = restore_pre(&resolved, &record).await {
                        return ToolOutput::Done(Err(e.to_string()));
                    }
                    if let Some(r) = ctx.edit_store.get_mut(id) {
                        r.reverted = true;
                    }
                    return ToolOutput::Done(Ok(restore_pre_msg(&record, id)));
                }

                let current = match tokio::fs::read_to_string(&resolved).await {
                    Ok(c) => c,
                    Err(e) => return ToolOutput::Done(Err(e.to_string())),
                };
                let mut content = current.clone();
                let mut failed = false;
                for (i, (old, new)) in record.edits.iter().rev().enumerate() {
                    let count = count_fuzzy_matches(&content, new);
                    if count != 1 {
                        warn!(
                            "Patch revert sub-edit {}: new_text matches {} times (expected 1)",
                            i, count
                        );
                        failed = true;
                        break;
                    }
                    match apply_edit(&content, new, old, i) {
                        Ok(r) => content = r,
                        Err(e) => {
                            warn!("Patch revert sub-edit {} failed: {}", i, e);
                            failed = true;
                            break;
                        }
                    }
                }
                if failed {
                    if let Err(e) = restore_pre(&resolved, &record).await {
                        return ToolOutput::Done(Err(e.to_string()));
                    }
                    if let Some(r) = ctx.edit_store.get_mut(id) {
                        r.reverted = true;
                    }
                    ToolOutput::Done(Ok(format!(
                        "Patch revert failed (text not found); restored full pre-edit \
                         snapshot for edit {}. Other changes to this file since edit {} \
                         were also reverted.",
                        id, id
                    )))
                } else {
                    if let Err(e) = tokio::fs::write(&resolved, &content).await {
                        return ToolOutput::Done(Err(e.to_string()));
                    }
                    if let Some(r) = ctx.edit_store.get_mut(id) {
                        r.reverted = true;
                    }
                    ToolOutput::Done(Ok(format!("Reverted patch for edit {}", id)))
                }
            }

            _ => ToolOutput::Done(Err(format!("Unknown mode: {}", mode))),
        }
    }
}
