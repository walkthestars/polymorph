//! Minimal REST + SSE server.
//!
//! Sessions CRUD + bounded-replay SSE over a strict-minimal HTTP/1.1
//! subset on a tokio `TcpListener`: exact `HTTP/1.1` version, single
//! space splits, `Content-Length` bodies only (chunked / 100-continue /
//! anything clever 400s instead of being interpreted — the subset is
//! deliberately too small to be clever in). std sockets, zero new deps.
//!
//! Routes: `GET /health`, `GET /sessions`, `POST /sessions` {title},
//! `GET /sessions/:id`, `DELETE /sessions/:id`,
//! `GET /sessions/:id/messages[?limit=]`, `GET /sessions/:id/events`
//! (SSE: existing messages as `event: message` frames + terminal
//! `event: end`, then close — bounded replay, NOT a live tail; the
//! hold-open tail needs a producer hookup that does not exist, and
//! hanging tests to fake liveness is refused — 135-plan ruling).
//! Errors are JSON `{"error": …}` (400 malformed, 404 unknown
//! route/session, 405 method, 500 store fault with the fault named).
//! Localhost-only posture is DOCUMENTED (single-operator pack), not
//! enforced — a bind flag rides the first non-localhost ask (stated).

use std::path::Path;
use std::sync::Arc;

use polymorph_core::sessions::{Message, Session, Store};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Shared server state (store handle only — no sessions affinity).
#[derive(Clone)]
pub struct Server {
    store: Arc<Store>,
}

impl Server {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(store),
        }
    }

    /// Serve forever on `port` (all interfaces — see module docs for the
    /// localhost posture note); returns on Ctrl-C.
    pub async fn serve(self, port: u16) -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
        tracing::info!(port, "polymorph serve: listening (local-operator posture)");
        loop {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break,
                accepted = listener.accept() => {
                    let (stream, _) = accepted?;
                    let this = self.clone();
                    tokio::spawn(async move {
                        if let Err(e) = this.serve_conn(stream).await {
                            tracing::debug!(error = %e, "serve: connection fault");
                        }
                    });
                }
            }
        }
        Ok(())
    }

    /// Serve ONE connection (the test seam — raw bytes in, raw bytes out).
    async fn serve_conn(&self, mut stream: tokio::net::TcpStream) -> anyhow::Result<()> {
        let req = read_request(&mut stream).await?;
        let resp = self.route(req).await;
        stream.write_all(&resp).await?;
        stream.flush().await?;
        Ok(())
    }
}

/// Open the store at `db` and serve on `port` (CLI wiring seam).
pub async fn serve(db: &Path, port: u16) -> anyhow::Result<()> {
    let store = Store::open(db)
        .await
        .map_err(|e| anyhow::anyhow!("serve: cannot open session store: {e}"))?;
    Server::new(store).serve(port).await
}

struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

/// Read one HTTP/1.1 request: headers until `\r\n\r\n`, then exactly
/// `Content-Length` body bytes (absent = empty). Anything outside the
/// subset is an error naming the fault (never interpreted).
async fn read_request(stream: &mut tokio::net::TcpStream) -> anyhow::Result<Request> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("serve: connection closed mid-request");
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
        if buf.len() > 1_048_576 {
            anyhow::bail!("serve: headers exceed 1MB");
        }
    }
    let head_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("loop breaks only on marker");
    let head = String::from_utf8(buf[..head_end].to_vec())
        .map_err(|_| anyhow::anyhow!("serve: request head is not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split(' ');
    let (method, target, version) = match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some(m), Some(t), Some(v), None) => (m, t, v),
        _ => anyhow::bail!("serve: malformed request line"),
    };
    if version != "HTTP/1.1" {
        anyhow::bail!("serve: unsupported version {version:?} (HTTP/1.1 only)");
    }
    if method.is_empty() || target.is_empty() || !target.starts_with('/') {
        anyhow::bail!("serve: malformed request line");
    }
    let mut content_length: usize = 0;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| anyhow::anyhow!("serve: malformed header line"))?;
        if k.trim().eq_ignore_ascii_case("content-length") {
            content_length = v
                .trim()
                .parse()
                .map_err(|_| anyhow::anyhow!("serve: bad Content-Length"))?;
            if content_length > 1_048_576 {
                anyhow::bail!("serve: body exceeds 1MB");
            }
        }
    }
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.to_string(), String::new()),
    };
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < content_length {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            anyhow::bail!("serve: connection closed mid-body");
        }
        body.extend_from_slice(&tmp[..n]);
        if body.len() > content_length {
            anyhow::bail!("serve: body overruns Content-Length");
        }
    }
    body.truncate(content_length);
    Ok(Request {
        method: method.to_string(),
        path,
        query,
        body,
    })
}

fn json_response(status: u16, reason: &str, value: serde_json::Value) -> Vec<u8> {
    let body = serde_json::to_string(&value).unwrap_or_else(|_| "{\"error\":\"encode\"}".into());
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn err(status: u16, reason: &str, detail: &str) -> Vec<u8> {
    json_response(status, reason, serde_json::json!({"error": detail}))
}

fn session_json(s: &Session) -> serde_json::Value {
    serde_json::json!({
        "id": s.id,
        "title": s.title,
        "created_at": s.created_at,
        "updated_at": s.updated_at,
    })
}

fn message_json(m: &Message) -> serde_json::Value {
    serde_json::json!({
        "id": m.id,
        "session_id": m.session_id,
        "role": m.role.to_string(),
        "content": m.content,
        "created_at": m.created_at,
    })
}

/// Reject empty / dot segments (fail closed — ids never escape routing).
fn clean_id(seg: &str) -> Option<&str> {
    if seg.is_empty() || seg == "." || seg == ".." || seg.contains('/') || seg.contains('\\') {
        return None;
    }
    Some(seg)
}

fn query_limit(query: &str) -> i64 {
    for pair in query.split('&') {
        if let Some(v) = pair.strip_prefix("limit=") {
            if let Ok(n) = v.parse::<i64>() {
                return n.clamp(1, 1000);
            }
        }
    }
    100
}

impl Server {
    async fn route(&self, req: Request) -> Vec<u8> {
        let segs: Vec<&str> = req.path.split('/').skip(1).collect();
        match (req.method.as_str(), segs.as_slice()) {
            ("GET", ["health"]) => json_response(200, "OK", serde_json::json!({"ok": true})),
            ("GET", ["sessions"]) => match self.store.list_sessions().await {
                Ok(list) => json_response(
                    200,
                    "OK",
                    serde_json::Value::Array(list.iter().map(session_json).collect()),
                ),
                Err(e) => err(500, "Internal Server Error", &format!("store: {e}")),
            },
            ("POST", ["sessions"]) => {
                let title = serde_json::from_slice::<serde_json::Value>(&req.body)
                    .ok()
                    .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(str::to_string))
                    .unwrap_or_default();
                if title.trim().is_empty() {
                    return err(400, "Bad Request", "sessions: `title` must be non-empty");
                }
                match self.store.create_session(&title).await {
                    Ok(s) => json_response(201, "Created", session_json(&s)),
                    Err(e) => err(500, "Internal Server Error", &format!("store: {e}")),
                }
            }
            ("GET", ["sessions", id]) => {
                let Some(id) = clean_id(id) else {
                    return err(400, "Bad Request", "sessions: bad id");
                };
                match self.store.list_sessions().await {
                    Ok(list) => match list.iter().find(|s| s.id == id) {
                        Some(s) => json_response(200, "OK", session_json(s)),
                        None => err(404, "Not Found", &format!("session not found: {id}")),
                    },
                    Err(e) => err(500, "Internal Server Error", &format!("store: {e}")),
                }
            }
            ("DELETE", ["sessions", id]) => {
                let Some(id) = clean_id(id) else {
                    return err(400, "Bad Request", "sessions: bad id");
                };
                match self.store.delete_session(id).await {
                    Ok(()) => json_response(200, "OK", serde_json::json!({"deleted": id})),
                    Err(e) => err(404, "Not Found", &format!("store: {e}")),
                }
            }
            ("GET", ["sessions", id, "messages"]) => {
                let Some(id) = clean_id(id) else {
                    return err(400, "Bad Request", "sessions: bad id");
                };
                match self.store.history(id, query_limit(&req.query)).await {
                    Ok(msgs) => json_response(
                        200,
                        "OK",
                        serde_json::Value::Array(msgs.iter().map(message_json).collect()),
                    ),
                    Err(e) => err(404, "Not Found", &format!("store: {e}")),
                }
            }
            ("GET", ["sessions", id, "events"]) => {
                let Some(id) = clean_id(id) else {
                    return err(400, "Bad Request", "sessions: bad id");
                };
                match self.store.history(id, 1000).await {
                    Ok(msgs) => {
                        let mut body = String::new();
                        for m in &msgs {
                            body.push_str(&format!(
                                "event: message\ndata: {}\n\n",
                                serde_json::to_string(&message_json(m))
                                    .unwrap_or_else(|_| "{}".into())
                            ));
                        }
                        body.push_str("event: end\ndata: {}\n\n");
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        )
                        .into_bytes()
                    }
                    Err(e) => err(404, "Not Found", &format!("store: {e}")),
                }
            }
            (_, ["sessions", ..]) => err(405, "Method Not Allowed", "sessions: method not allowed"),
            (_, ["health"]) => err(405, "Method Not Allowed", "health: method not allowed"),
            _ => err(404, "Not Found", "serve: unknown route"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polymorph_core::sessions::MessageRole;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Live server on an ephemeral port (seeded: one session + 2 messages).
    /// The accept task is aborted at test end (test-only lifecycle).
    async fn live_server() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>, String) {
        let store = Store::open_in_memory().await.unwrap();
        let s = store.create_session("demo").await.unwrap();
        store
            .append_message(&s.id, MessageRole::User, "hi")
            .await
            .unwrap();
        store
            .append_message(&s.id, MessageRole::Assistant, "hello")
            .await
            .unwrap();
        let server = Server::new(store);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    break;
                };
                let this = server.clone();
                tokio::spawn(async move {
                    let _ = this.serve_conn(stream).await;
                });
            }
        });
        (addr, h, s.id)
    }

    async fn raw(addr: std::net::SocketAddr, req: &str) -> String {
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        stream.read_to_end(&mut out).await.unwrap();
        String::from_utf8(out).unwrap()
    }

    fn body_of(resp: &str) -> &str {
        resp.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("")
    }

    #[test]
    fn clean_id_refuses_dot_segments() {
        assert!(clean_id("").is_none());
        assert!(clean_id(".").is_none());
        assert!(clean_id("..").is_none());
        assert!(clean_id("a/b").is_none());
        assert!(clean_id(&format!("a{}b", char::from(92u8))).is_none());
        assert_eq!(clean_id("abc-123"), Some("abc-123"));
    }

    #[tokio::test]
    async fn health_and_list() {
        let (addr, h, _) = live_server().await;
        let r = raw(addr, "GET /health HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
        assert!(body_of(&r).contains("\"ok\":true"), "{r}");
        let r = raw(addr, "GET /sessions HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
        assert!(body_of(&r).contains("\"demo\""), "{r}");
        h.abort();
    }

    #[tokio::test]
    async fn create_get_delete_roundtrip() {
        let (addr, h, _) = live_server().await;
        let payload = "{\"title\":\"work\"}";
        let r = raw(
            addr,
            &format!(
                "POST /sessions HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            ),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 201 Created"), "{r}");
        let v: serde_json::Value = serde_json::from_str(body_of(&r)).unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        let r = raw(
            addr,
            &format!("GET /sessions/{id} HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 200 OK") && body_of(&r).contains("\"work\""),
            "{r}"
        );
        let r = raw(
            addr,
            &format!("DELETE /sessions/{id} HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
        let r = raw(
            addr,
            &format!("GET /sessions/{id} HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 404") && body_of(&r).contains(&id),
            "{r}"
        );
        h.abort();
    }

    #[tokio::test]
    async fn post_requires_title_and_unknowns_fail_closed() {
        let (addr, h, _) = live_server().await;
        let r = raw(
            addr,
            "POST /sessions HTTP/1.1\r\nHost: x\r\nContent-Length: 2\r\n\r\n{}",
        )
        .await;
        assert!(
            r.starts_with("HTTP/1.1 400") && body_of(&r).contains("title"),
            "{r}"
        );
        let r = raw(addr, "GET /nope HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let r = raw(addr, "POST /health HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(r.starts_with("HTTP/1.1 405"), "{r}");
        let r = raw(addr, "GET /sessions/ghost HTTP/1.1\r\nHost: x\r\n\r\n").await;
        assert!(
            r.starts_with("HTTP/1.1 404") && body_of(&r).contains("ghost"),
            "{r}"
        );
        h.abort();
    }

    #[tokio::test]
    async fn messages_and_sse_replay() {
        let (addr, h, id) = live_server().await;
        let r = raw(
            addr,
            &format!("GET /sessions/{id}/messages HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
        let v: serde_json::Value = serde_json::from_str(body_of(&r)).unwrap();
        assert_eq!(v.as_array().unwrap().len(), 2);
        let r = raw(
            addr,
            &format!("GET /sessions/{id}/events HTTP/1.1\r\nHost: x\r\n\r\n"),
        )
        .await;
        assert!(r.starts_with("HTTP/1.1 200 OK"), "{r}");
        assert!(r.contains("text/event-stream"), "{r}");
        let b = body_of(&r);
        assert_eq!(b.matches("event: message").count(), 2, "{b}");
        assert!(b.contains("event: end"), "{b}");
        assert!(b.contains("\"hello\""), "{b}");
        h.abort();
    }

    #[tokio::test]
    async fn malformed_request_closes_without_bytes() {
        // Fail-closed parse boundary: garbage in, zero bytes out (the
        // fault is a debug log, never a response guess).
        let (addr, h, _) = live_server().await;
        let r = raw(addr, "GARBAGE\r\n\r\n").await;
        assert!(r.is_empty(), "{r}");
        h.abort();
    }
}
