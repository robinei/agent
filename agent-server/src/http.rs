use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use agent_core::config::Config;

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

pub fn handle_connection(mut stream: TcpStream, cfg: Arc<Config>) {
    // Slowloris guard: a misbehaving client could open a TCP connection and
    // dribble bytes forever, pinning a thread. 30s read timeout closes the
    // connection if no progress is made between reads. Required, not optional.
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let header_end;
    loop {
        if buf.len() > MAX_HEADER_BYTES {
            write_status(&mut stream, 431, "Request Header Fields Too Large");
            return;
        }
        let mut tmp = [0u8; 1024];
        let n = match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        buf.extend_from_slice(&tmp[..n]);
        let mut hs = [httparse::EMPTY_HEADER; 32];
        let mut req = httparse::Request::new(&mut hs);
        match req.parse(&buf) {
            Ok(httparse::Status::Complete(n)) => {
                header_end = n;
                break;
            }
            Ok(httparse::Status::Partial) => continue,
            Err(_) => {
                write_status(&mut stream, 400, "Bad Request");
                return;
            }
        }
    }

    // Re-parse to get owned views of method/path/headers. The borrow
    // checker prevents us from holding the previous parser across the
    // loop boundary, so we extract everything into owned data here.
    let mut hs = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut hs);
    let _ = req.parse(&buf);

    let method = req.method.unwrap_or("").to_string();
    let path = req.path.unwrap_or("").to_string();
    let headers: Vec<(String, Vec<u8>)> = hs
        .iter()
        .filter(|h| !h.name.is_empty())
        .map(|h| (h.name.to_string(), h.value.to_vec()))
        .collect();

    let is_ws = method == "GET"
        && header_contains(&headers, "upgrade", b"websocket")
        && header_contains(&headers, "connection", b"upgrade");
    log::debug!("[http] {} {}{}", method, path, if is_ws { " (ws)" } else { "" });
    if is_ws {
        crate::ws::accept(stream, &path, &headers, cfg);
        return;
    }

    let content_length: usize = header_get(&headers, "content-length")
        .and_then(|v| std::str::from_utf8(&v).ok()?.parse().ok())
        .unwrap_or(0);

    let has_transfer_encoding = header_contains(&headers, "transfer-encoding", b"chunked");
    if has_transfer_encoding && content_length > 0 {
        write_status(&mut stream, 400, "Bad Request");
        return;
    }
    if has_transfer_encoding {
        write_status(&mut stream, 411, "Length Required");
        return;
    }
    if content_length > MAX_BODY_BYTES {
        write_status(&mut stream, 413, "Payload Too Large");
        return;
    }
    let need = header_end + content_length;
    while buf.len() < need {
        let mut tmp = [0u8; 4096];
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return,
        }
    }
    let body = &buf[header_end..need.min(buf.len())];

    let (status, body_bytes, content_type) =
        crate::routes::dispatch(&method, &path, body, &cfg);
    write_response(&mut stream, status, &body_bytes, content_type);
}

fn header_get(headers: &[(String, Vec<u8>)], name: &str) -> Option<Vec<u8>> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

fn header_contains(headers: &[(String, Vec<u8>)], name: &str, needle: &[u8]) -> bool {
    header_get(headers, name)
        .map(|v| {
            v.split(|&b| b == b',')
                .any(|t| t.trim_ascii().eq_ignore_ascii_case(needle))
        })
        .unwrap_or(false)
}

fn write_status(w: &mut TcpStream, code: u16, reason: &str) {
    let _ = write!(
        w,
        "HTTP/1.1 {} {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        code, reason
    );
}

fn write_response(w: &mut TcpStream, status: u16, body: &[u8], content_type: &str) {
    let _ = write!(
        w,
        "HTTP/1.1 {} OK\r\n\
         Content-Type: {}\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\r\n",
        status,
        content_type,
        body.len()
    );
    let _ = w.write_all(body);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_header_contains_case_insensitive() {
        let h = vec![("Upgrade".into(), b"websocket".to_vec())];
        assert!(header_contains(&h, "upgrade", b"websocket"));
        assert!(header_contains(&h, "UPGRADE", b"websocket"));
        assert!(!header_contains(&h, "upgrade", b"http2"));
    }

    #[test]
    fn test_header_contains_comma_separated() {
        let h = vec![("Connection".into(), b"keep-alive, upgrade".to_vec())];
        assert!(header_contains(&h, "connection", b"upgrade"));
        assert!(header_contains(&h, "connection", b"keep-alive"));
        assert!(!header_contains(&h, "connection", b"close"));
    }

    #[test]
    fn test_header_get_nonexistent() {
        let h: Vec<(String, Vec<u8>)> = vec![];
        assert!(header_get(&h, "x-missing").is_none());
    }
}
