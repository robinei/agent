use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::config::Config;
use agent_core::store::Store;
use tungstenite::Message;

const STREAM_TOKEN: mio::Token = mio::Token(0);
const WAKER_TOKEN: mio::Token = mio::Token(1);

pub fn accept(
    mut stream: TcpStream,
    path: &str,
    headers: &[(String, Vec<u8>)],
    store: Arc<Store>,
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

    let accept =
        tungstenite::handshake::derive_accept_key(key.trim().as_bytes());
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

    // Set non-blocking BEFORE wrapping in tungstenite. This is what makes
    // the single-threaded read+write session loop work: reads return
    // WouldBlock when no message is ready, so we can fall through to
    // draining outbound events and keepalive instead of deadlocking.
    // This supersedes any `set_read_timeout` inherited from the HTTP
    // layer — those are mutually exclusive modes on POSIX, and
    // non-blocking wins. The leftover timeout is safely ignored.
    if stream.set_nonblocking(true).is_err() {
        return;
    }

    let mut ws = tungstenite::WebSocket::from_raw_socket(
        stream,
        tungstenite::protocol::Role::Server,
        None,
    );

    if crate::lifecycle::worker_get(&tree_id).is_none() {
        if let Err(e) = crate::lifecycle::spawn_worker(&tree_id, store.clone(), cfg.clone()) {
            let _ = ws.send(Message::Text(
                serde_json::to_string(&serde_json::json!({
                    "type": "error", "message": e, "fatal": true,
                }))
                .unwrap(),
            ));
            return;
        }
    }

    let poll = match mio::Poll::new() {
        Ok(p) => p,
        Err(_) => return,
    };
    let waker = match mio::Waker::new(poll.registry(), WAKER_TOKEN) {
        Ok(w) => w,
        Err(_) => return,
    };

    let Some((catch_up, rx)) = crate::lifecycle::worker_subscribe(&tree_id, waker) else {
        return;
    };
    for ev in catch_up {
        if let Ok(s) = serde_json::to_string(&ev) {
            if ws.send(Message::Text(s)).is_err() {
                return;
            }
        }
    }

    run_session(tree_id, ws, rx, poll);
}

fn run_session(
    tree_id: String,
    mut ws: tungstenite::WebSocket<TcpStream>,
    rx: std::sync::mpsc::Receiver<agent_core::types::ServerEvent>,
    mut poll: mio::Poll,
) {
    use std::os::unix::io::AsRawFd;

    let raw_fd = ws.get_ref().as_raw_fd();
    let mut source = mio::unix::SourceFd(&raw_fd);
    if poll.registry()
        .register(&mut source, STREAM_TOKEN, mio::Interest::READABLE)
        .is_err()
    {
        return;
    }

    let mut events = mio::Events::with_capacity(8);
    let mut last_ping = Instant::now();
    let mut last_pong = Instant::now();

    loop {
        // Block until: WS data ready, waker signalled, or next keepalive due.
        let timeout = Duration::from_secs(30)
            .checked_sub(last_ping.elapsed())
            .unwrap_or(Duration::ZERO);
        if poll.poll(&mut events, Some(timeout)).is_err() {
            break;
        }

        // Drain inbound WS messages.
        match ws.read() {
            Ok(Message::Text(s)) => {
                let _ = crate::lifecycle::worker_send_command(&tree_id, &s);
            }
            Ok(Message::Pong(_)) => {
                last_pong = Instant::now();
            }
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => break,
        }

        // Drain outbound events.
        while let Ok(ev) = rx.try_recv() {
            if let Ok(s) = serde_json::to_string(&ev) {
                if ws.send(Message::Text(s)).is_err() {
                    return;
                }
            }
        }

        let now = Instant::now();
        if now.duration_since(last_ping) > Duration::from_secs(30) {
            let _ = ws.send(Message::Ping(Vec::new()));
            last_ping = now;
        }
        if now.duration_since(last_pong) > Duration::from_secs(90) {
            log::warn!("[ws {}] pong timeout", tree_id);
            let _ = ws.send(Message::Close(None));
            break;
        }

        // INTENTIONAL: no sleep here. mio::Poll above blocks until the WS
        // socket is readable or waker.wake() is called by the broadcast path.
        // The timeout drives the keepalive interval. This replaces the former
        // 10ms sleep; removing the sleep is the whole point of this step.
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
    use super::*;

    #[test]
    fn test_derive_accept_key_matches_rfc() {
        let key = "dGhlIHNhbXBsZSBub25jZQ==";
        let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
    }
}
