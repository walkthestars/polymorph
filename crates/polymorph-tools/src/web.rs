//! `tools::web` — serica-backed web client (Epic P, chunk 119).
//!
//! Serica (v0.2.0, `C:/Workspace/.projects/serica`) is the extraction
//! engine: single static binary, REST + MCP, no keys, SSRF-hardened.
//! It is AGPL-3.0-only, so this module drives it over HTTP to a local
//! server (process boundary — serica crates never enter `Cargo.lock`;
//! binaries live host-side, the lsp-servers.md precedent). Transport is
//! the workspace `reqwest` (already carried for genai — zero new heavy
//! deps). No connection pool: HTTP needs none (unlike stdio children);
//! pool-if-measured at P1b, not upfront.
//!
//! Wire contract (source-read off the serica checkout, chunk-116/119
//! design): `GET {base}/api/v1/search?q=&page=&safe=&lang=` +
//! `GET {base}/api/v1/extract?url=`, envelope
//! `{"success":true,"data":{…}}` /
//! `{"success":false,"error":{"code","message"}}`. Statuses (serica's
//! own tests): INVALID_QUERY→400, RATE_LIMITED→429, UNAUTHORIZED→401,
//! CONTENT_TOO_LARGE→413, EXTRACT_TIMEOUT→504, 503 when down.
//!
//! `total_results` is the PAGE-BATCH count (serica struct comment —
//! NOT a corpus total). This client echoes it verbatim; P1b tool
//! descriptions must carry that caveat (docketed in plan-chunk119).

use std::time::Duration;

/// Timeout discipline, mirrored (script_hooks caps at 600 s).
pub const MAX_TIMEOUT_SECS: u64 = 600;
/// Matches serica's own extract budget (30 s wall-clock).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// Serica's default listen address (`SERICA_SERVER__PORT` = 3000).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:3000";
/// Serica rejects `q` outside 1–500 chars (fail fast locally).
pub const MAX_QUERY_CHARS: usize = 500;

/// Which serica server to talk to. Built by [`validate`] (fail-closed
/// at attach — bad config bails naming the field, never at call).
#[derive(Debug, Clone, PartialEq)]
pub struct WebSpec {
    pub base_url: String,
    pub timeout: Duration,
}

/// Validate one `[web]` table into a runnable spec.
pub fn validate(base_url: &str, timeout_secs: u64) -> anyhow::Result<WebSpec> {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        anyhow::bail!("web config is missing `base_url`");
    }
    if timeout_secs == 0 || timeout_secs > MAX_TIMEOUT_SECS {
        anyhow::bail!(
            "web timeout must be 1..={} seconds (got {timeout_secs})",
            MAX_TIMEOUT_SECS
        );
    }
    Ok(WebSpec {
        base_url: base_url.trim_end_matches('/').to_string(),
        timeout: Duration::from_secs(timeout_secs),
    })
}

/// Build a spec from parsed config (P1b's `wire_web` uses this; the
/// `[[hooks]]`-validation precedent — config never trusted raw).
pub fn spec_for(cfg: &polymorph_core::config::WebConfig) -> anyhow::Result<WebSpec> {
    validate(&cfg.base_url, cfg.timeout_secs)
}

/// One search hit (serica `SearchResult` mirror — hand-mirrored, never
/// a crate dep; strict fields so server drift reds loud).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchHit {
    pub url: String,
    pub title: String,
    pub snippet: String,
}

/// One search page (serica `SearchResponse` mirror).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SearchReport {
    pub results: Vec<SearchHit>,
    /// PAGE-BATCH count (`results.len()` pre-cap), not a corpus total.
    pub total_results: usize,
    pub page: u32,
    pub timing_ms: u64,
    pub cached: bool,
}

/// One extraction (serica `ExtractResponse` mirror: single `content`
/// string, markdown-or-text per server config — never two fields).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ExtractReport {
    pub url: String,
    pub title: Option<String>,
    pub author: Option<String>,
    pub date: Option<String>,
    pub sitename: Option<String>,
    pub page_type: Option<String>,
    pub content: Option<String>,
    pub excerpt: Option<String>,
    pub timing_ms: u64,
    pub cached: bool,
}

/// Classified failure. Status-first (a bare 429 with no body still
/// classifies); envelope code second; unparseable-unknown last.
#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("bad web argument: {0}")]
    BadArg(String),
    #[error("serica rate-limited this request (HTTP 429)")]
    RateLimited,
    #[error("serica refused oversized content (HTTP 413): {0}")]
    TooLarge(String),
    #[error("serica extract timed out: {0}")]
    Timeout(String),
    #[error("serica requires an API key (HTTP 401): {0}")]
    Unauthorized(String),
    #[error("serica error {code}: {message}")]
    Server { code: String, message: String },
    #[error("web transport failed: {0}")]
    Transport(String),
}

#[derive(Debug, serde::Deserialize)]
struct Envelope<T> {
    #[serde(default)]
    success: bool,
    data: Option<T>,
    error: Option<ErrorBody>,
}

#[derive(Debug, serde::Deserialize)]
struct ErrorBody {
    #[serde(default = "unknown_code")]
    code: String,
    #[serde(default)]
    message: String,
}

fn unknown_code() -> String {
    "UNKNOWN".to_string()
}

/// Split an optional `#fragment` off a scheme remainder (P2 chunk 121).
/// Fragments never ride HTTP — the caller fetches without it and echoes
/// it (116-D4: never stripped silently).
fn split_fragment(rest: &str) -> (&str, Option<String>) {
    match rest.split_once('#') {
        Some((base, frag)) if !frag.is_empty() => (base, Some(frag.to_string())),
        Some((base, _)) => (base, None),
        None => (rest, None),
    }
}

/// `gh://owner/repo[/path][@ref][#frag]` → GitHub blob HTML page.
/// Blob pages (not raw: serica refuses `text/plain`) + `HEAD` default
/// ref (both live-proven). Path optional → repo landing page.
pub fn github_url(rest: &str) -> anyhow::Result<(String, Option<String>)> {
    let (base, fragment) = split_fragment(rest);
    let (repo_path, reference) = match base.split_once('@') {
        Some((p, r)) if !r.is_empty() => (p, r.to_string()),
        _ => (base.split('@').next().unwrap_or(base), "HEAD".to_string()),
    };
    let mut parts = repo_path.splitn(3, '/');
    let (owner, repo) = match (parts.next(), parts.next()) {
        (Some(o), Some(r)) if !o.is_empty() && !r.is_empty() => (o, r),
        _ => anyhow::bail!("gh: want `gh://owner/repo[/path][@ref]"),
    };
    let path = parts.next().unwrap_or("");
    let url = if path.is_empty() {
        format!("https://github.com/{owner}/{repo}")
    } else {
        format!("https://github.com/{owner}/{repo}/blob/{reference}/{path}")
    };
    Ok((url, fragment))
}

/// `npm://name[@version][#frag]` → npmjs package page (proven clean
/// extract). Arbitrary file reads have no servable HTML mapping
/// (unpkg serves JSON/tarball serica refuses) → fail-closed naming it.
/// Pinned versions can 403 upstream (bot-wall, live-proven) — the
/// server error surfaces verbatim, never masked.
pub fn npm_url(rest: &str) -> anyhow::Result<(String, Option<String>)> {
    let (base, fragment) = split_fragment(rest);
    // Version `@` is the LAST `@` past position 0 (`@scope/name` alone
    // carries no version; a trailing `@` means latest).
    let (name, version) = match base.rfind('@') {
        Some(i) if i > 0 => {
            let (n, v) = base.split_at(i);
            if v.len() > 1 {
                (n, Some(&v[1..]))
            } else {
                (n, None)
            }
        }
        _ => (base, None),
    };
    // Valid names: `pkg` or `@scope/pkg` — anything more slashed is a
    // file path, which has no servable HTML mapping (unpkg serves
    // JSON/tarball serica refuses) → fail-closed naming it.
    let valid = if let Some(stripped) = name.strip_prefix('@') {
        let mut p = stripped.split('/');
        matches!((p.next(), p.next(), p.next()), (Some(s), Some(n), None) if !s.is_empty() && !n.is_empty())
    } else {
        !name.is_empty() && !name.contains('/')
    };
    if !valid {
        anyhow::bail!("npm: file-level reads (`npm://pkg/file`) have no extractable page — use `npm://name[@version]`");
    }
    let url = match version {
        Some(v) => format!("https://www.npmjs.com/package/{name}/v/{v}"),
        None => format!("https://www.npmjs.com/package/{name}"),
    };
    Ok((url, fragment))
}

/// `arxiv://id[#frag]` → ar5iv HTML (proven clean extract). The id
/// passes through VERBATIM (no version normalization invented —
/// server errors surface as-is).
pub fn arxiv_url(rest: &str) -> anyhow::Result<(String, Option<String>)> {
    let (base, fragment) = split_fragment(rest);
    if base.trim().is_empty() || base.contains('/') {
        anyhow::bail!("arxiv: want `arxiv://id` (e.g. `arxiv://1706.03762`)");
    }
    Ok((format!("https://ar5iv.org/html/{base}"), fragment))
}

/// Wire one `[web]` table (the `wire_renderdoc` shape): validate +
/// enroll, fail-soft (warn — a bad table never kills the run).
/// Returns enrolled or not. Absent table needs no call — reads fall
/// back to localhost defaults (fail-soft at call time).
pub async fn wire_web(registry: &crate::Registry, cfg: &polymorph_core::config::WebConfig) -> bool {
    match spec_for(cfg) {
        Ok(spec) => {
            registry.set_web_spec(spec).await;
            true
        }
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "web skipped");
            false
        }
    }
}

/// Search serica: `q` required (1–500 chars), `page` ≥ 1, `safe` 0–2
/// (closed documented ranges — fail fast naming the arg). `lang` is
/// passthrough (open set owned by the server — no client allowlist to
/// drift out of sync).
pub async fn search(
    spec: &WebSpec,
    q: &str,
    page: u32,
    safe: u8,
    lang: Option<&str>,
) -> Result<SearchReport, WebError> {
    let q = q.trim();
    if q.is_empty() {
        return Err(WebError::BadArg("search: `q` must be non-empty".into()));
    }
    if q.chars().count() > MAX_QUERY_CHARS {
        return Err(WebError::BadArg(format!(
            "search: `q` exceeds {MAX_QUERY_CHARS} chars"
        )));
    }
    if page == 0 {
        return Err(WebError::BadArg("search: `page` starts at 1".into()));
    }
    if safe > 2 {
        return Err(WebError::BadArg(format!(
            "search: `safe` is 0, 1, or 2 (got {safe})"
        )));
    }
    let client = client_for(spec)?;
    let mut params: Vec<(&str, String)> = vec![
        ("q", q.to_string()),
        ("page", page.to_string()),
        ("safe", safe.to_string()),
    ];
    if let Some(lang) = lang {
        params.push(("lang", lang.to_string()));
    }
    let resp = client
        .get(format!("{}/api/v1/search", spec.base_url))
        .query(&params)
        .send()
        .await
        .map_err(|e| send_err(e, "search", spec))?;
    read_envelope(resp, "search").await
}

/// Extract a URL through serica (https-only enforced server-side;
/// empty URL refused here naming the arg).
pub async fn extract(spec: &WebSpec, url: &str) -> Result<ExtractReport, WebError> {
    let url = url.trim();
    if url.is_empty() {
        return Err(WebError::BadArg("extract: `url` must be non-empty".into()));
    }
    let client = client_for(spec)?;
    let resp = client
        .get(format!("{}/api/v1/extract", spec.base_url))
        .query(&[("url", url)])
        .send()
        .await
        .map_err(|e| send_err(e, "extract", spec))?;
    read_envelope(resp, "extract").await
}

fn client_for(spec: &WebSpec) -> Result<reqwest::Client, WebError> {
    reqwest::Client::builder()
        .timeout(spec.timeout)
        .build()
        .map_err(|e| WebError::Transport(format!("web client build failed: {e}")))
}

fn send_err(e: reqwest::Error, op: &'static str, spec: &WebSpec) -> WebError {
    if e.is_timeout() {
        WebError::Timeout(format!("{op}: client gave up after {:?}", spec.timeout))
    } else if e.is_connect() {
        WebError::Transport(format!(
            "{op}: cannot reach serica at {} ({e})",
            spec.base_url
        ))
    } else {
        WebError::Transport(format!("{op}: {e}"))
    }
}

async fn read_envelope<T>(resp: reqwest::Response, op: &'static str) -> Result<T, WebError>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let status = resp.status().as_u16();
    let body = resp
        .text()
        .await
        .map_err(|e| WebError::Transport(format!("{op}: unreadable body: {e}")))?;
    match serde_json::from_str::<Envelope<T>>(&body) {
        Ok(env) if env.success => env
            .data
            .ok_or_else(|| WebError::Transport(format!("{op}: success envelope carried no data"))),
        Ok(env) => {
            let err = env.error.unwrap_or(ErrorBody {
                code: unknown_code(),
                message: String::new(),
            });
            Err(classify(status, &err.code, &err.message))
        }
        Err(_) => Err(classify_bare(status, op)),
    }
}

fn classify(status: u16, code: &str, message: &str) -> WebError {
    match status {
        429 => WebError::RateLimited,
        413 => WebError::TooLarge(message.to_string()),
        504 => WebError::Timeout(format!("server EXTRACT_TIMEOUT: {message}")),
        401 => WebError::Unauthorized(message.to_string()),
        _ => WebError::Server {
            code: code.to_string(),
            message: message.to_string(),
        },
    }
}

fn classify_bare(status: u16, op: &'static str) -> WebError {
    match status {
        429 => WebError::RateLimited,
        413 => WebError::TooLarge(format!("{op}: HTTP 413 with unparseable body")),
        504 => WebError::Timeout(format!("{op}: HTTP 504 with unparseable body")),
        401 => WebError::Unauthorized(format!("{op}: HTTP 401 with unparseable body")),
        _ => WebError::Transport(format!("{op}: HTTP {status} with unparseable body")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> WebSpec {
        // Unroutable by construction: validation tests must fail BEFORE
        // any I/O (a test that hangs here is a validation-order bug).
        validate("http://127.0.0.1:9", 5).unwrap()
    }

    #[test]
    fn validate_ok_strips_trailing_slash() {
        let s = validate("http://127.0.0.1:3000/", 30).unwrap();
        assert_eq!(s.base_url, "http://127.0.0.1:3000");
        assert_eq!(s.timeout, Duration::from_secs(30));
        assert_eq!(DEFAULT_BASE_URL, "http://127.0.0.1:3000");
        assert_eq!(DEFAULT_TIMEOUT_SECS, 30);
    }

    #[test]
    fn validate_refuses_empty_base_url_and_bad_timeout() {
        assert!(validate("", 30).is_err());
        assert!(validate("   ", 30).is_err());
        assert!(validate("http://127.0.0.1:3000", 0).is_err());
        assert!(validate("http://127.0.0.1:3000", MAX_TIMEOUT_SECS + 1).is_err());
    }

    #[test]
    fn spec_for_config_uses_table_values() {
        let cfg = polymorph_core::config::WebConfig {
            base_url: "http://127.0.0.1:4000".into(),
            timeout_secs: 10,
        };
        let s = spec_for(&cfg).unwrap();
        assert_eq!(s.base_url, "http://127.0.0.1:4000");
        assert_eq!(s.timeout, Duration::from_secs(10));
        let s = spec_for(&polymorph_core::config::WebConfig::default()).unwrap();
        assert_eq!(s.base_url, DEFAULT_BASE_URL);
    }

    #[tokio::test]
    async fn search_refuses_bad_args_before_io() {
        let s = spec();
        assert!(matches!(
            search(&s, "  ", 1, 1, None).await.unwrap_err(),
            WebError::BadArg(_)
        ));
        assert!(matches!(
            search(&s, &"q".repeat(MAX_QUERY_CHARS + 1), 1, 1, None)
                .await
                .unwrap_err(),
            WebError::BadArg(_)
        ));
        assert!(matches!(
            search(&s, "rust", 0, 1, None).await.unwrap_err(),
            WebError::BadArg(_)
        ));
        assert!(matches!(
            search(&s, "rust", 1, 3, None).await.unwrap_err(),
            WebError::BadArg(_)
        ));
    }

    #[tokio::test]
    async fn extract_refuses_empty_url_before_io() {
        assert!(matches!(
            extract(&spec(), "  ").await.unwrap_err(),
            WebError::BadArg(_)
        ));
    }

    #[test]
    fn envelope_ok_search_parses() {
        let env: Envelope<SearchReport> = serde_json::from_str(
            r#"{"success":true,"data":{"results":[{"url":"https://example.com","title":"T","snippet":"S"}],"total_results":1,"page":1,"timing_ms":7,"cached":false}}"#,
        )
        .unwrap();
        assert!(env.success);
        let d = env.data.unwrap();
        assert_eq!(d.results.len(), 1);
        assert_eq!(d.total_results, 1);
        assert!(!d.cached);
    }

    #[test]
    fn envelope_ok_extract_parses_nulls() {
        let env: Envelope<ExtractReport> = serde_json::from_str(
            r##"{"success":true,"data":{"url":"https://example.com","title":"T","author":null,"date":null,"sitename":null,"page_type":null,"content":"# Hi","excerpt":null,"timing_ms":9,"cached":true}}"##,
        )
        .unwrap();
        let d = env.data.unwrap();
        assert_eq!(d.content.as_deref(), Some("# Hi"));
        assert!(d.author.is_none());
        assert!(d.cached);
    }

    #[test]
    fn envelope_error_names_code() {
        let env: Envelope<SearchReport> = serde_json::from_str(
            r#"{"success":false,"error":{"code":"INVALID_QUERY","message":"query is empty"}}"#,
        )
        .unwrap();
        assert!(!env.success);
        assert_eq!(env.error.unwrap().code, "INVALID_QUERY");
    }

    #[tokio::test]
    async fn wiremock_search_cycle_green() {
        use wiremock::matchers::{method, path, query_param};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .and(query_param("q", "rust"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"success":true,"data":{"results":[{"url":"https://example.com/rust","title":"Rust","snippet":"systems"}],"total_results":1,"page":2,"timing_ms":11,"cached":true}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        let out = search(&s, "rust", 2, 1, Some("en")).await.unwrap();
        assert_eq!(out.results.len(), 1);
        assert_eq!(out.results[0].url, "https://example.com/rust");
        assert_eq!(out.total_results, 1);
        assert_eq!(out.page, 2);
        assert!(out.cached);
    }

    #[tokio::test]
    async fn wiremock_extract_cycle_green() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/extract"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"success":true,"data":{"url":"https://example.com","title":"T","author":null,"date":null,"sitename":"Example","page_type":"article","content":"body","excerpt":"ex","timing_ms":5,"cached":false}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        let out = extract(&s, "https://example.com").await.unwrap();
        assert_eq!(out.content.as_deref(), Some("body"));
        assert_eq!(out.sitename.as_deref(), Some("Example"));
        assert!(!out.cached);
    }

    #[tokio::test]
    async fn wiremock_429_envelope_maps_rate_limited() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(429).set_body_string(
                r#"{"success":false,"error":{"code":"RATE_LIMITED","message":"slow down"}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        assert!(matches!(
            search(&s, "rust", 1, 1, None).await.unwrap_err(),
            WebError::RateLimited
        ));
    }

    #[tokio::test]
    async fn wiremock_bare_429_without_body_still_classifies() {
        // Status-first proof: no envelope at all, still RateLimited —
        // never a Transport misroute (the chunk-7 429 lesson: match the
        // status, never sniff the body).
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(429))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        assert!(matches!(
            search(&s, "rust", 1, 1, None).await.unwrap_err(),
            WebError::RateLimited
        ));
    }

    #[tokio::test]
    async fn wiremock_413_maps_too_large() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/extract"))
            .respond_with(ResponseTemplate::new(413).set_body_string(
                r#"{"success":false,"error":{"code":"CONTENT_TOO_LARGE","message":"raw HTML exceeds limits"}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        let err = extract(&s, "https://example.com/big").await.unwrap_err();
        assert!(matches!(err, WebError::TooLarge(_)), "{err}");
        assert!(format!("{err}").contains("exceeds"), "{err}");
    }

    #[tokio::test]
    async fn wiremock_504_maps_timeout() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/extract"))
            .respond_with(ResponseTemplate::new(504).set_body_string(
                r#"{"success":false,"error":{"code":"EXTRACT_TIMEOUT","message":"extract exceeded budget"}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        assert!(matches!(
            extract(&s, "https://example.com/slow").await.unwrap_err(),
            WebError::Timeout(_)
        ));
    }

    #[tokio::test]
    async fn wiremock_400_names_invalid_query() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                r#"{"success":false,"error":{"code":"INVALID_QUERY","message":"query is empty"}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        // Client-side q-refusal means this path needs a valid q that the
        // server still rejects (passthrough proof — client never masks).
        let err = search(&s, "rust", 1, 1, None).await.unwrap_err();
        assert!(
            matches!(&err, WebError::Server { code, .. } if code == "INVALID_QUERY"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn wiremock_503_names_service_unavailable() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(503).set_body_string(
                r#"{"success":false,"error":{"code":"SERVICE_UNAVAILABLE","message":"draining"}}"#,
            ))
            .mount(&server)
            .await;
        let s = validate(&server.uri(), 10).unwrap();
        let err = search(&s, "rust", 1, 1, None).await.unwrap_err();
        assert!(
            matches!(&err, WebError::Server { code, .. } if code == "SERVICE_UNAVAILABLE"),
            "{err}"
        );
    }

    #[test]
    fn github_forms_map_to_blob_or_repo() {
        assert_eq!(
            github_url("owner/repo/path/f.rs").unwrap(),
            (
                "https://github.com/owner/repo/blob/HEAD/path/f.rs".into(),
                None
            )
        );
        assert_eq!(
            github_url("owner/repo/path/f.rs@main#L10").unwrap(),
            (
                "https://github.com/owner/repo/blob/main/path/f.rs".into(),
                Some("L10".into())
            )
        );
        assert_eq!(
            github_url("owner/repo").unwrap(),
            ("https://github.com/owner/repo".into(), None)
        );
        assert!(github_url("owner").is_err());
        assert!(github_url("/repo/x").is_err());
        assert!(github_url("").is_err());
    }

    #[test]
    fn npm_forms_map_to_package_pages() {
        assert_eq!(
            npm_url("react").unwrap(),
            ("https://www.npmjs.com/package/react".into(), None)
        );
        assert_eq!(
            npm_url("react@18.3.1").unwrap(),
            ("https://www.npmjs.com/package/react/v/18.3.1".into(), None)
        );
        assert_eq!(
            npm_url("@scope/pkg@1.0#readme").unwrap(),
            (
                "https://www.npmjs.com/package/@scope/pkg/v/1.0".into(),
                Some("readme".into())
            )
        );
        assert_eq!(
            npm_url("@scope/pkg").unwrap(),
            ("https://www.npmjs.com/package/@scope/pkg".into(), None)
        );
        assert!(npm_url("react/dist/bundle.js").is_err());
        assert!(npm_url("").is_err());
    }

    #[test]
    fn arxiv_ids_pass_through_verbatim() {
        assert_eq!(
            arxiv_url("1706.03762").unwrap(),
            ("https://ar5iv.org/html/1706.03762".into(), None)
        );
        assert_eq!(
            arxiv_url("1706.03762v7#S3").unwrap(),
            (
                "https://ar5iv.org/html/1706.03762v7".into(),
                Some("S3".into())
            )
        );
        assert!(arxiv_url("").is_err());
        assert!(arxiv_url("a/b").is_err());
    }
}
