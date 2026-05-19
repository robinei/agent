use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

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
}