use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;

use agent_core::config::Config;

use crate::spawner::{self, WorkerMsg};
use crate::worker_loop::WsClient;

pub fn accept(
    mut stream: TcpStream,
    path: &str,
    headers: &[(String, Vec<u8>)],
    cfg: Arc<Config>,
) {
    let tree_id = match path
        .strip_prefix("/trees/")
        .and_then(|r| r.strip_suffix("/ws"))
    {
        Some(id) if !id.is_empty() && !id.contains('/') => id.to_string(),
        _ => {
            let _ = write_400(&mut stream, "bad ws path");
            return;
        }
    };

    let key = match get_header(headers, "sec-websocket-key") {
        Some(k) => k,
        None => {
            let _ = write_400(&mut stream, "missing Sec-WebSocket-Key");
            return;
        }
    };

    let accept = tungstenite::handshake::derive_accept_key(key.trim().as_bytes());
    if write!(
        stream,
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {}\r\n\r\n",
        accept
    )
    .is_err()
    {
        return;
    }

    if spawner::worker_get(&tree_id).is_none() {
        if let Err(e) = spawner::spawn_worker(&tree_id, cfg.clone()) {
            let mut ws = tungstenite::WebSocket::from_raw_socket(
                stream,
                tungstenite::protocol::Role::Server,
                None,
            );
            let _ = ws.send(tungstenite::Message::Text(
                serde_json::to_string(&serde_json::json!({
                    "type": "error", "message": e, "fatal": true,
                }))
                .unwrap(),
            ));
            return;
        }
    }

    let entry = match spawner::worker_get(&tree_id) {
        Some(e) => e,
        None => return,
    };

    if stream.set_nonblocking(true).is_err() {
        return;
    }

    let ws = tungstenite::WebSocket::from_raw_socket(
        stream,
        tungstenite::protocol::Role::Server,
        None,
    );

    let ws_client = Box::new(WsClient::new(ws));

    let (msg_tx, notify_write) = {
        let guard = entry.lock().unwrap();
        (
            guard.msg_tx.clone(),
            guard.notify_write.try_clone().ok(),
        )
    };

    match notify_write {
        Some(nw) => {
            if let Err(e) = msg_tx.send(WorkerMsg::NewClient(ws_client)) {
                log::warn!("[ws] failed to deliver NewClient to event loop (worker exited?): {}", e);
            } else {
                log::info!("[ws] client connected for tree {}", tree_id);
                let _ = nix::unistd::write(&nw, b"\x00");
            }
        }
        None => {
            log::warn!("[ws] notify_write clone failed, dropping client");
        }
    }
}

fn get_header(headers: &[(String, Vec<u8>)], name: &str) -> Option<String> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .map(|s| s.to_string())
}

fn write_400(stream: &mut TcpStream, msg: &str) -> std::io::Result<()> {
    let body = format!("{{\"error\":\"{}\"}}", msg);
    write!(
        stream,
        "HTTP/1.1 400 Bad Request\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

#[cfg(test)]
mod tests {
    use tungstenite::handshake::derive_accept_key;

    #[test]
    fn test_derive_accept_key_matches_rfc() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = derive_accept_key(key.as_bytes());
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
