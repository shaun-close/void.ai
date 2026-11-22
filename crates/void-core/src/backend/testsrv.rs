//! A one-shot HTTP server for backend tests: records the request, replays canned bytes.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Serve one connection on 127.0.0.1, writing `chunks` in order with a short pause between them.
/// Returns the base URL (`http://127.0.0.1:<port>`) and a handle that yields the raw request.
pub(crate) async fn serve_once(chunks: Vec<Vec<u8>>) -> (String, JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            if n == 0 {
                break;
            }
            req.extend_from_slice(&buf[..n]);
            if let Some(end) = find(&req, b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&req[..end]).to_lowercase();
                let len = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse::<usize>().ok())
                    .unwrap_or(0);
                if req.len() >= end + 4 + len {
                    break;
                }
            }
        }
        for c in chunks {
            sock.write_all(&c).await.unwrap();
            sock.flush().await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        let _ = sock.shutdown().await;
        String::from_utf8_lossy(&req).into_owned()
    });
    (base, handle)
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// An event-stream response split into `parts` roughly equal chunks after the headers.
pub(crate) fn sse_response(body: &str, parts: usize) -> Vec<Vec<u8>> {
    let mut out = vec![
        b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n".to_vec(),
    ];
    let bytes = body.as_bytes();
    let size = bytes.len().div_ceil(parts.max(1)).max(1);
    out.extend(bytes.chunks(size).map(<[u8]>::to_vec));
    out
}

/// A complete JSON response with the given status.
pub(crate) fn json_response(status: u16, body: &str) -> Vec<Vec<u8>> {
    vec![
        format!(
            "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    ]
}
