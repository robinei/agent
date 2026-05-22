use agent_core::store::Store;
use agent_core::types::*;

use crate::provider;

/// Auto-generate a title for a tree by asking the LLM.
pub fn auto_title(
    store: &Store,
    provider: &dyn provider::Provider,
    tree_id: &str,
) -> Result<String, String> {
    let entries = store.read_all_entries(tree_id).map_err(|e| e.to_string())?;
    let meta = store.get_tree(tree_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "Tree not found".to_string())?;

    let leaf_id = meta.leaf_id.as_deref().ok_or_else(|| "No entries".to_string())?;
    let mut messages = agent_worker::agent::build_context(&entries, leaf_id);

    let system = Message {
        role: MessageRole::System,
        content: MessageContent::Text(
            "Generate a concise title (6 words or fewer) for this coding conversation. \
             Return ONLY the title text, no quotes, no punctuation, no explanation."
                .to_string(),
        ),
        tool_calls: None,
        tool_call_id: None,
        tool_name: None,
        usage: None,
        stop_reason: None,
        is_error: None,
    };
    messages.insert(0, system);

    let response = provider.chat(&messages, &[])
        .map_err(|e| format!("LLM call failed: {}", e))?;

    let title = response.text.trim().trim_matches('"').to_string();
    if title.is_empty() {
        return Err("Generated empty title".to_string());
    }

    let mut meta = meta;
    meta.title = Some(title.clone());
    store.save_tree_meta(&meta).map_err(|e| e.to_string())?;

    Ok(title)
}