//! Context building and token estimation.
//!
//! These are library functions used by the worker agent loop.

use std::collections::HashMap;

use agent_core::types::*;

// ── Context building ──

/// Build a list of Messages for the LLM by walking the parent chain from leaf_id.
///
/// Walk upward from leaf_id:
/// - `Message` -> include as-is
/// - `SessionEnd` with continuation_brief -> inject as system message, stop
/// - `ModelSet` -> track model
/// - `GoalSet` -> track goal
/// - Skip: `session_start`, `label`, `bash_exec`
pub fn build_context(entries: &[Entry], leaf_id: &str) -> Vec<Message> {
    let map: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id(), e)).collect();

    let mut messages: Vec<Message> = Vec::new();
    let mut current: Option<&str> = Some(leaf_id);
    let mut found_goal = None;
    let mut _found_model = None;

    while let Some(cid) = current {
        let entry = match map.get(cid) {
            Some(e) => e,
            None => break,
        };

        match entry {
            Entry::Message { message, .. } => {
                messages.push(message.clone());
            }
            Entry::SessionEnd {
                continuation_brief, ..
            } => {
                if let Some(brief) = continuation_brief {
                    if !brief.trim().is_empty() {
                        messages.push(Message {
                            role: MessageRole::System,
                            content: MessageContent::Text(format!(
                                "## Previous Session Continuation\n{}",
                                brief
                            )),
                            tool_calls: None,
                            tool_call_id: None,
                            tool_name: None,
                            usage: None,
                            stop_reason: None,
                            is_error: None,
                        });
                    }
                }
                break;
            }
            Entry::GoalSet { goal, .. } => {
                found_goal = Some(goal.clone());
            }
            Entry::ModelSet { model, .. } => {
                _found_model = Some(model.clone());
            }
            _ => {}
        }

        current = entry.parent_id();
    }

    messages.reverse();

    if let Some(goal) = found_goal {
        messages.insert(
            0,
            Message {
                role: MessageRole::System,
                content: MessageContent::Text(format!("## Current Goal\n{}", goal)),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            },
        );
    }

    messages
}

// ── Token estimation ──

/// Estimate tokens for a single string. Uses ~3.5 chars/token average.
pub fn estimate_tokens(content: &str) -> usize {
    (content.len() * 2 + 7) / 7
}

/// Estimate total tokens across all messages in a context.
pub fn estimate_context_tokens(messages: &[Message]) -> usize {
    let mut total = 0;
    for msg in messages {
        match &msg.content {
            MessageContent::Text(s) => total += estimate_tokens(s),
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    match block {
                        ContentBlock::Text { text } => total += estimate_tokens(text),
                        ContentBlock::ToolCall { arguments, .. } => {
                            total += estimate_tokens(&arguments.to_string());
                        }
                    }
                }
            }
        }
        if let Some(calls) = &msg.tool_calls {
            for call in calls {
                total += estimate_tokens(&call.arguments.to_string());
            }
        }
    }
    total
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;

    fn make_message(id: &str, parent: Option<&str>, role: MessageRole, text: &str) -> Entry {
        Entry::Message {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            message: Message {
                role,
                content: MessageContent::Text(text.to_string()),
                tool_calls: None,
                tool_call_id: None,
                tool_name: None,
                usage: None,
                stop_reason: None,
                is_error: None,
            },
        }
    }

    fn make_session_start(id: &str, parent: Option<&str>) -> Entry {
        Entry::SessionStart {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn make_session_end(id: &str, parent: Option<&str>, brief: Option<&str>) -> Entry {
        Entry::SessionEnd {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            summary: None,
            status: SessionStatus::Continuing,
            continuation_brief: brief.map(|s| s.to_string()),
        }
    }

    fn make_goal_set(id: &str, parent: Option<&str>, goal: &str) -> Entry {
        Entry::GoalSet {
            id: id.to_string(),
            parent_id: parent.map(|s| s.to_string()),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            goal: goal.to_string(),
        }
    }

#[test]
    fn test_build_context_empty_tree() {
        let entries = vec![make_session_start("1", None)];
        let messages = build_context(&entries, "1");
        assert!(messages.is_empty(), "SessionStart should be excluded");
    }

    #[test]
    fn test_build_context_single_message() {
        let entries = vec![
            make_session_start("root", None),
            make_message("msg1", Some("root"), MessageRole::User, "hello"),
        ];
        let messages = build_context(&entries, "msg1");
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].role, MessageRole::User);
    }

    #[test]
    fn test_build_context_chain() {
        let entries = vec![
            make_session_start("root", None),
            make_message("msg1", Some("root"), MessageRole::User, "first"),
            make_message("msg2", Some("msg1"), MessageRole::Assistant, "second"),
            make_message("msg3", Some("msg2"), MessageRole::User, "third"),
        ];
        let messages = build_context(&entries, "msg3");
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].role, MessageRole::User);
        assert_eq!(messages[2].role, MessageRole::User);
    }

    #[test]
    fn test_build_context_with_continuation_brief() {
        let entries = vec![
            make_session_start("root", None),
            make_session_end("end1", Some("root"), Some("Accomplished X")),
            make_session_start("ss2", Some("end1")),
            make_message("msg1", Some("ss2"), MessageRole::User, "continue"),
        ];
        let messages = build_context(&entries, "msg1");
        assert!(messages.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("Accomplished X"))
        }));
    }

    #[test]
    fn test_build_context_with_goal() {
        let entries = vec![
            make_session_start("root", None),
            make_goal_set("goal1", Some("root"), "Refactor auth"),
            make_message("msg1", Some("goal1"), MessageRole::User, "start"),
        ];
        let messages = build_context(&entries, "msg1");
        assert!(messages.iter().any(|m| {
            matches!(&m.content, MessageContent::Text(t) if t.contains("Refactor auth"))
        }));
    }


    #[test]
    fn test_estimate_tokens_short() {
        let est = estimate_tokens("hello world");
        assert!(est > 0);
        assert!(est <= 10);
    }

    #[test]
    fn test_estimate_tokens_long() {
        let text = "a".repeat(1000);
        let est = estimate_tokens(&text);
        assert!(est > 200);
        assert!(est <= 400);
    }

    #[test]
    fn test_estimate_context_tokens_empty() {
        assert_eq!(estimate_context_tokens(&[]), 0);
    }

    #[test]
    fn test_estimate_context_tokens_single() {
        let msg = Message {
            role: MessageRole::User,
            content: MessageContent::Text("hello".to_string()),
            tool_calls: None,
            tool_call_id: None,
            tool_name: None,
            usage: None,
            stop_reason: None,
            is_error: None,
        };
        let est = estimate_context_tokens(&[msg]);
        assert_eq!(est, estimate_tokens("hello"));
    }
}