//! Hook system for extensibility.
//!
//! Hooks allow intercepting agent lifecycle events (tool calls, LLM calls,
//! session end, startup) without modifying core code.
//!
//! Hooks are registered at startup via `register_hook()`.

use std::sync::{LazyLock, Mutex};

/// Action a hook can take in response to an event.
#[derive(Clone, Debug)]
pub enum HookAction {
    /// Let the event proceed normally.
    PassThrough,
    /// Block the event with a reason.
    Block { reason: String },
    /// Replace the event payload.
    Replace { payload: serde_json::Value },
    /// Log / observe only.
    Observe,
}

/// A hook that can intercept agent lifecycle events.
pub trait Hook: Send {
    fn name(&self) -> &'static str;

    /// Called before a tool executes. Return Block to prevent execution.
    fn on_tool_call(
        &self,
        _tool: &str,
        _params: &serde_json::Value,
    ) -> Result<HookAction, Box<dyn std::error::Error + Send + Sync>> {
        Ok(HookAction::PassThrough)
    }

    /// Called after context is built but before LLM call.
    fn on_before_llm_call(
        &self,
        _messages: &mut Vec<crate::types::Message>,
    ) -> Result<HookAction, Box<dyn std::error::Error + Send + Sync>> {
        Ok(HookAction::PassThrough)
    }

    /// Called after session_end is generated.
    fn on_session_end(
        &self,
        _summary: &str,
    ) -> Result<HookAction, Box<dyn std::error::Error + Send + Sync>> {
        Ok(HookAction::PassThrough)
    }

    /// Called on server startup.
    fn on_startup(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

// ── Hook registry ──

static HOOKS: LazyLock<Mutex<Vec<Box<dyn Hook>>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// Register a hook. Called at server startup.
pub fn register_hook(hook: Box<dyn Hook>) {
    HOOKS.lock().unwrap().push(hook);
}

/// Run all on_tool_call hooks. Returns error if any hook blocks.
pub fn run_tool_call_hooks(
    tool: &str,
    params: &serde_json::Value,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hooks = HOOKS.lock().unwrap();
    for hook in hooks.iter() {
        match hook.on_tool_call(tool, params)? {
            HookAction::Block { reason } => {
                return Err(format!("Hook '{}' blocked tool call: {}", hook.name(), reason).into());
            }
            HookAction::Replace { .. } => {
                // For simplicity, ignore Replace for now — hooks can modify
                // params directly if needed. Replace would require &mut params.
            }
            HookAction::PassThrough | HookAction::Observe => {}
        }
    }
    Ok(())
}

/// Run all on_before_llm_call hooks. Returns error if any hook blocks.
pub fn run_before_llm_hooks(
    messages: &mut Vec<crate::types::Message>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hooks = HOOKS.lock().unwrap();
    for hook in hooks.iter() {
        match hook.on_before_llm_call(messages)? {
            HookAction::Block { reason } => {
                return Err(
                    format!("Hook '{}' blocked LLM call: {}", hook.name(), reason).into(),
                );
            }
            HookAction::Replace { payload } => {
                if let Ok(new_msgs) =
                    serde_json::from_value::<Vec<crate::types::Message>>(payload)
                {
                    *messages = new_msgs;
                }
            }
            HookAction::PassThrough | HookAction::Observe => {}
        }
    }
    Ok(())
}

/// Run all on_session_end hooks.
pub fn run_session_end_hooks(
    summary: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hooks = HOOKS.lock().unwrap();
    for hook in hooks.iter() {
        hook.on_session_end(summary)?;
    }
    Ok(())
}

/// Run all on_startup hooks.
pub fn run_startup_hooks() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let hooks = HOOKS.lock().unwrap();
    for hook in hooks.iter() {
        hook.on_startup()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BlockRmHook;

    impl Hook for BlockRmHook {
        fn name(&self) -> &'static str {
            "block-rm"
        }

        fn on_tool_call(
            &self,
            tool: &str,
            params: &serde_json::Value,
        ) -> Result<HookAction, Box<dyn std::error::Error + Send + Sync>> {
            if tool == "bash" {
                let cmd = params.get("command").and_then(|v| v.as_str()).unwrap_or("");
                if cmd.contains("rm -rf") {
                    return Ok(HookAction::Block {
                        reason: "rm -rf blocked by policy".into(),
                    });
                }
            }
            Ok(HookAction::PassThrough)
        }
    }

    #[test]
    fn test_register_and_block() {
        // Clear any hooks from previous tests
        let hooks = HOOKS.lock().unwrap();
        assert!(hooks.is_empty());
        drop(hooks);

        register_hook(Box::new(BlockRmHook));

        let result = run_tool_call_hooks("bash", &serde_json::json!({"command": "rm -rf /"}));
        assert!(result.is_err(), "rm -rf should be blocked");

        let ok_result = run_tool_call_hooks("bash", &serde_json::json!({"command": "ls -la"}));
        assert!(ok_result.is_ok(), "ls should not be blocked");
    }

    #[test]
    fn test_session_end_hook() {
        let result = run_session_end_hooks("session completed");
        assert!(result.is_ok());
    }
}
