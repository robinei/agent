use std::io::{BufWriter, Write};
use std::net::TcpStream;
use std::os::fd::AsRawFd;
use std::os::fd::RawFd;
use std::process::ChildStdin;
use std::time::Instant;

use agent_core::rpc::{PipeIn, WsCommand};

#[derive(Debug)]
pub struct WsClient {
    ws: tungstenite::WebSocket<TcpStream>,
    last_ping: Instant,
    last_pong: Instant,
}

impl WsClient {
    pub fn new(ws: tungstenite::WebSocket<TcpStream>) -> Self {
        Self {
            ws,
            last_ping: Instant::now(),
            last_pong: Instant::now(),
        }
    }

    pub fn fd(&self) -> RawFd {
        self.ws.get_ref().as_raw_fd()
    }

    pub fn on_readable(&mut self, stdin: &mut BufWriter<ChildStdin>) -> bool {
        match self.ws.read() {
            Ok(tungstenite::Message::Text(s)) => {
                if let Ok(cmd) = serde_json::from_str::<WsCommand>(&s) {
                    let pipe_in = PipeIn::Cmd(cmd);
                    if let Ok(json) = serde_json::to_string(&pipe_in) {
                        let _ = writeln!(stdin, "{}", json);
                        let _ = stdin.flush();
                    }
                }
                true
            }
            Ok(tungstenite::Message::Pong(_)) => {
                self.last_pong = Instant::now();
                true
            }
            Ok(tungstenite::Message::Close(_)) => false,
            Ok(_) => true,
            Err(tungstenite::Error::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }

    pub fn write_raw(&mut self, json: &str) -> bool {
        self.ws
            .send(tungstenite::Message::Text(json.to_string()))
            .is_ok()
    }

    pub fn tick(&mut self, _stdin: &mut BufWriter<ChildStdin>) -> bool {
        let now = Instant::now();
        if now.duration_since(self.last_ping) > std::time::Duration::from_secs(30) {
            let _ = self.ws.send(tungstenite::Message::Ping(Vec::new()));
            self.last_ping = now;
        }
        if now.duration_since(self.last_pong) > std::time::Duration::from_secs(90) {
            let _ = self.ws.send(tungstenite::Message::Close(None));
            false
        } else {
            true
        }
    }
}
