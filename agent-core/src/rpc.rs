use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::PathBuf;

use crate::types::{Message, ToolDefinition};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "method", rename_all = "lowercase")]
pub enum WsCommand {
    Message { params: MessageParams },
    Stop,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MessageParams {
    pub text: String,
}

// Carried in PipeOut::Llm — worker asks server to run a chat completion
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct LlmRequest {
    pub id: u64,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
}

// Carried in PipeIn::Llm — server streams completion back to worker
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LlmResponse {
    Chunk { id: u64, data: String },
    Done { id: u64 },
    Error { id: u64, message: String },
}

// Envelope for worker stdout → server
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "ch", content = "msg", rename_all = "snake_case")]
pub enum PipeOut {
    Event(ServerEvent),
    Llm(LlmRequest),
}

/// Configuration sent to the worker over the pipe at startup. Carries only
/// what the worker needs — no API keys or credentials.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct WorkerConfig {
    pub session_soft_cap_pct: u8,
    pub session_hard_cap_pct: u8,
    pub max_tool_calls_per_turn: usize,
    pub logging_level: String,
    pub logging_to_file: Option<PathBuf>,
    pub logging_to_stderr: bool,
}

// Envelope for server stdin → worker
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "ch", content = "msg", rename_all = "snake_case")]
pub enum PipeIn {
    Cmd(WsCommand),
    Llm(LlmResponse),
    Config(WorkerConfig),
}

// Re-export ServerEvent so PipeOut/Event is usable without a second import
pub use crate::types::ServerEvent;

pub fn write_json_line<W: Write, T: Serialize>(w: &mut W, value: &T) -> std::io::Result<()> {
    let s = serde_json::to_string(value).map_err(std::io::Error::other)?;
    w.write_all(s.as_bytes())?;
    w.write_all(b"\n")?;
    w.flush()
}

pub fn read_json_line<R: BufRead, T: serde::de::DeserializeOwned>(
    r: &mut R,
    buf: &mut String,
) -> std::io::Result<Option<T>> {
    buf.clear();
    match r.read_line(buf)? {
        0 => Ok(None),
        _ => Ok(Some(serde_json::from_str(buf.trim_end()).map_err(std::io::Error::other)?)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wscommand_message_roundtrip() {
        let cmd = WsCommand::Message {
            params: MessageParams { text: "hi".into() },
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert_eq!(s, r#"{"method":"message","params":{"text":"hi"}}"#);
        let deserialized: WsCommand = serde_json::from_str(&s).unwrap();
        match deserialized {
            WsCommand::Message { params } => assert_eq!(params.text, "hi"),
            _ => panic!("expected Message variant"),
        }
    }

    #[test]
    fn test_wscommand_stop_no_params() {
        let s = r#"{"method":"stop"}"#;
        let cmd: WsCommand = serde_json::from_str(s).unwrap();
        assert!(matches!(cmd, WsCommand::Stop));
    }

    #[test]
    fn test_llm_request_roundtrip() {
        let req = LlmRequest {
            id: 42,
            messages: vec![],
            tools: vec![],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: LlmRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 42);
        assert!(parsed.messages.is_empty());
        assert!(parsed.tools.is_empty());
    }

    #[test]
    fn test_llm_response_chunk_roundtrip() {
        let resp = LlmResponse::Chunk {
            id: 1,
            data: "data: {\"key\":\"value\"}\n".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"kind":"chunk","id":1,"data":"data: {\"key\":\"value\"}\n"}"#);
        let parsed: LlmResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(&parsed, LlmResponse::Chunk { id, .. } if *id == 1));
    }

    #[test]
    fn test_llm_response_done_roundtrip() {
        let resp = LlmResponse::Done { id: 2 };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"kind":"done","id":2}"#);
        let parsed: LlmResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, LlmResponse::Done { id } if id == 2));
    }

    #[test]
    fn test_llm_response_error_roundtrip() {
        let resp = LlmResponse::Error {
            id: 3,
            message: "API error".into(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"kind":"error","id":3,"message":"API error"}"#);
        let parsed: LlmResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(&parsed, LlmResponse::Error { id, message } if *id == 3 && message == "API error"));
    }

    #[test]
    fn test_pipe_out_event_roundtrip() {
        let ev = ServerEvent::TextChunk { content: "hello".into() };
        let pipe = PipeOut::Event(ev);
        let json = serde_json::to_string(&pipe).unwrap();
        assert_eq!(json, r#"{"ch":"event","msg":{"type":"text_chunk","content":"hello"}}"#);
    }

    #[test]
    fn test_pipe_out_llm_roundtrip() {
        let req = LlmRequest {
            id: 0,
            messages: vec![],
            tools: vec![],
        };
        let pipe = PipeOut::Llm(req);
        let json = serde_json::to_string(&pipe).unwrap();
        assert_eq!(json, r#"{"ch":"llm","msg":{"id":0,"messages":[],"tools":[]}}"#);
    }

    #[test]
    fn test_pipe_in_cmd_roundtrip() {
        let cmd = WsCommand::Message {
            params: MessageParams { text: "go".into() },
        };
        let pipe = PipeIn::Cmd(cmd);
        let json = serde_json::to_string(&pipe).unwrap();
        assert_eq!(json, r#"{"ch":"cmd","msg":{"method":"message","params":{"text":"go"}}}"#);
    }

    #[test]
    fn test_pipe_in_llm_roundtrip() {
        let resp = LlmResponse::Chunk {
            id: 0,
            data: "data: {...}\n".into(),
        };
        let pipe = PipeIn::Llm(resp);
        let json = serde_json::to_string(&pipe).unwrap();
        assert_eq!(json, r#"{"ch":"llm","msg":{"kind":"chunk","id":0,"data":"data: {...}\n"}}"#);
    }

    #[test]
    fn test_pipe_in_llm_done_roundtrip() {
        let pipe = PipeIn::Llm(LlmResponse::Done { id: 0 });
        let json = serde_json::to_string(&pipe).unwrap();
        assert_eq!(json, r#"{"ch":"llm","msg":{"kind":"done","id":0}}"#);
    }

    #[test]
    fn test_pipe_in_config_roundtrip() {
        let cfg = WorkerConfig {
            session_soft_cap_pct: 65,
            session_hard_cap_pct: 85,
            max_tool_calls_per_turn: 25,
            logging_level: "info".into(),
            logging_to_file: Some("/tmp/agent.log".into()),
            logging_to_stderr: true,
        };
        let pipe = PipeIn::Config(cfg);
        let json = serde_json::to_string(&pipe).unwrap();
        assert_eq!(
            json,
            r#"{"ch":"config","msg":{"session_soft_cap_pct":65,"session_hard_cap_pct":85,"max_tool_calls_per_turn":25,"logging_level":"info","logging_to_file":"/tmp/agent.log","logging_to_stderr":true}}"#
        );
        let parsed: PipeIn = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, PipeIn::Config(_)));
    }
}