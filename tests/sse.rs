//! The SSE transport: its security checks and a real MCP handshake over HTTP.
//!
//! The client here is hand-rolled on `TcpStream` and speaks **HTTP/1.0**: with no chunked encoding
//! the response body is just bytes until EOF, so reading SSE events needs no chunk parser and the
//! tests stay independent of an HTTP client crate. The server is spawned in-process on
//! `127.0.0.1:0`, so nothing outside the test can connect and no port is hard-coded.

use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

use poc_rust_mcp::{
    config::{Secret, ServerConfig},
    db::Store,
    server::Calculator,
    sse,
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};
use tokio_util::sync::CancellationToken;

const READ_TIMEOUT: Duration = Duration::from_secs(5);

/// A server bound to a free loopback port, cancelled when the test ends.
struct TestServer {
    addr: SocketAddr,
    cancel: CancellationToken,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn start(config: ServerConfig) -> TestServer {
    start_with_keep_alive(config, Duration::from_secs(15)).await
}

async fn start_with_keep_alive(config: ServerConfig, keep_alive: Duration) -> TestServer {
    let mut config = config;
    config.sse_address = "127.0.0.1:0".parse().expect("a free loopback port");
    let settings = sse::Settings::from_config(&config)
        .expect("test settings are valid")
        .with_keep_alive(keep_alive);
    let listener = tokio::net::TcpListener::bind(config.sse_address)
        .await
        .expect("bind a loopback listener");
    let addr = listener.local_addr().expect("the listener has an address");
    let cancel = CancellationToken::new();
    let server_cancel = cancel.clone();
    tokio::spawn(async move {
        let _ = sse::serve(
            listener,
            settings,
            || Calculator::new(Store::disabled()),
            server_cancel,
        )
        .await;
    });
    TestServer { addr, cancel }
}

fn host(server: &TestServer) -> String {
    format!("127.0.0.1:{}", server.addr.port())
}

/// Read a status line and headers from an HTTP response.
async fn read_head(reader: &mut BufReader<TcpStream>) -> (u16, Vec<(String, String)>) {
    let mut status_line = String::new();
    read_line(reader, &mut status_line).await;
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {status_line:?}"));
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        read_line(reader, &mut line).await;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return (status, headers);
        }
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_ascii_lowercase(), value.trim().to_owned()));
        }
    }
}

async fn read_line(reader: &mut BufReader<TcpStream>, line: &mut String) {
    let read = tokio::time::timeout(READ_TIMEOUT, reader.read_line(line))
        .await
        .expect("the server should answer within the deadline");
    let read = read.expect("reading from the server should not fail");
    assert!(read > 0, "the connection closed before the response ended");
}

async fn send_request(addr: SocketAddr, request: String) -> BufReader<TcpStream> {
    let stream = tokio::time::timeout(READ_TIMEOUT, TcpStream::connect(addr))
        .await
        .expect("connect within the deadline")
        .expect("connect to the test server");
    stream
        .set_nodelay(true)
        .expect("set TCP_NODELAY on a loopback stream");
    let mut stream = stream;
    tokio::time::timeout(READ_TIMEOUT, stream.write_all(request.as_bytes()))
        .await
        .expect("write within the deadline")
        .expect("write the request");
    BufReader::new(stream)
}

/// Open the event stream, returning the status and the reader positioned after the headers.
async fn get_sse(
    addr: SocketAddr,
    host: &str,
    origin: Option<&str>,
    token: Option<&str>,
) -> (u16, BufReader<TcpStream>) {
    let mut request = format!("GET /sse HTTP/1.0\r\nHost: {host}\r\n");
    if let Some(origin) = origin {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    let mut reader = send_request(addr, request).await;
    let (status, _) = read_head(&mut reader).await;
    (status, reader)
}

#[allow(clippy::too_many_arguments)]
async fn post(
    addr: SocketAddr,
    path: &str,
    host: &str,
    origin: Option<&str>,
    token: Option<&str>,
    body: &str,
) -> u16 {
    let mut request = format!(
        "POST {path} HTTP/1.0\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        request.push_str(&format!("Origin: {origin}\r\n"));
    }
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    let mut reader = send_request(addr, request).await;
    let (status, _) = read_head(&mut reader).await;
    status
}

/// One SSE event: its `event:` field and concatenated `data:` lines. Keep-alive comments are
/// skipped, and a frame without data is not an event.
async fn next_event(reader: &mut BufReader<TcpStream>) -> (Option<String>, Option<String>) {
    let mut event = None;
    let mut data = String::new();
    let mut have_data = false;
    loop {
        let mut line = String::new();
        read_line(reader, &mut line).await;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if have_data {
                return (event, Some(data));
            }
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("data:") {
            if have_data {
                data.push('\n');
            }
            data.push_str(value.trim_start());
            have_data = true;
        }
    }
}

async fn response_for(reader: &mut BufReader<TcpStream>, id: u64) -> Value {
    loop {
        let (event, data) = next_event(reader).await;
        if event.as_deref() != Some("message") {
            continue;
        }
        let Some(data) = data else { continue };
        let value: Value = serde_json::from_str(&data)
            .unwrap_or_else(|error| panic!("event data is not JSON: {error}: {data}"));
        if value.get("id") == Some(&json!(id)) {
            return value;
        }
    }
}

#[test]
fn non_loopback_binds_require_a_token_and_explicit_hosts() {
    let mut config = ServerConfig {
        sse_address: "0.0.0.0:8000".parse().expect("a valid address"),
        ..ServerConfig::default()
    };
    assert!(
        sse::Settings::from_config(&config).is_err(),
        "no token, no hosts"
    );

    config.sse_token = Some(Secret::new("t"));
    assert!(
        sse::Settings::from_config(&config).is_err(),
        "hosts still missing"
    );

    config.sse_allowed_hosts = vec!["mcp.example".to_owned()];
    sse::Settings::from_config(&config).expect("token + hosts is enough");

    sse::Settings::from_config(&ServerConfig::default()).expect("loopback needs neither");
}

#[test]
fn settings_reject_malformed_allowlist_entries() {
    let mut config = ServerConfig {
        sse_allowed_hosts: vec!["*".to_owned()],
        ..ServerConfig::default()
    };
    assert!(sse::Settings::from_config(&config).is_err());
    config.sse_allowed_hosts = vec!["mcp.example".to_owned()];
    config.sse_allowed_origins = vec!["https://mcp.example/path".to_owned()];
    assert!(sse::Settings::from_config(&config).is_err());
    config.sse_allowed_origins = vec!["https://mcp.example".to_owned()];
    sse::Settings::from_config(&config).expect("valid entries");
}

#[tokio::test]
async fn foreign_host_and_origin_are_rejected() {
    let server = start(ServerConfig::default()).await;
    let good_host = host(&server);

    let (status, _) = get_sse(server.addr, "evil.example", None, None).await;
    assert_eq!(status, 403, "a foreign Host must be refused");
    let status = post(
        server.addr,
        "/message?sessionId=whatever",
        "evil.example",
        None,
        None,
        "{}",
    )
    .await;
    assert_eq!(status, 403, "POST must validate Host too");

    let (status, _) = get_sse(server.addr, &good_host, Some("https://evil.example"), None).await;
    assert_eq!(status, 403, "a foreign Origin must be refused");
    let (status, _) = get_sse(server.addr, &good_host, Some("null"), None).await;
    assert_eq!(status, 403, "an opaque Origin must be refused");

    let (status, _) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 200, "a request without Origin is a local client");
    let (status, _) = get_sse(
        server.addr,
        &good_host,
        Some(&format!("http://{good_host}")),
        None,
    )
    .await;
    assert_eq!(status, 200, "a same-origin browser request must pass");
}

#[tokio::test]
async fn token_is_required_on_both_routes() {
    let config = ServerConfig {
        sse_token: Some(Secret::new("t0ken")),
        ..ServerConfig::default()
    };
    let server = start(config).await;
    let good_host = host(&server);

    let (status, _) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 401, "GET without a token");
    let (status, _) = get_sse(server.addr, &good_host, None, Some("wrong")).await;
    assert_eq!(status, 401, "GET with the wrong token");

    // A well-formed notification, so the only thing under test is the token: a body that fails to
    // deserialize would be rejected with 400 before the session is ever looked up.
    let notification =
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    let status = post(
        server.addr,
        "/message?sessionId=whatever",
        &good_host,
        None,
        None,
        &notification,
    )
    .await;
    assert_eq!(status, 401, "POST without a token");

    let status = post(
        server.addr,
        "/message?sessionId=whatever",
        &good_host,
        None,
        Some("t0ken"),
        &notification,
    )
    .await;
    assert_eq!(
        status, 404,
        "the token is accepted, so the failure is the unknown session"
    );

    let (status, _) = get_sse(server.addr, &good_host, None, Some("t0ken")).await;
    assert_eq!(status, 200, "GET with the token");
}

#[tokio::test]
async fn proxy_style_origin_matches_a_host_without_a_port() {
    let config = ServerConfig {
        sse_allowed_hosts: vec!["mcp.example".to_owned()],
        ..ServerConfig::default()
    };
    let server = start(config).await;

    let (status, _) = get_sse(
        server.addr,
        "mcp.example",
        Some("https://mcp.example"),
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "a TLS proxy forwards the public Host and a matching Origin"
    );

    let (status, _) = get_sse(
        server.addr,
        "mcp.example",
        Some("https://mcp.example:8443"),
        None,
    )
    .await;
    assert_eq!(
        status, 200,
        "without a port in Host, the origin port is not compared"
    );

    let (status, _) = get_sse(
        server.addr,
        "mcp.example:8443",
        Some("https://mcp.example"),
        None,
    )
    .await;
    assert_eq!(
        status, 403,
        "a pinned Host port makes the origin port significant"
    );
}

#[tokio::test]
async fn initialize_and_a_tool_call_work_over_the_event_stream() {
    let server = start(ServerConfig::default()).await;
    let good_host = host(&server);

    let (status, mut events) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 200);

    let (event, data) = next_event(&mut events).await;
    assert_eq!(event.as_deref(), Some("endpoint"));
    let endpoint = data.expect("the endpoint event carries data");
    let session_id = endpoint
        .strip_prefix("/message?sessionId=")
        .unwrap_or_else(|| panic!("unexpected endpoint {endpoint:?}"));
    let post_path = format!("/message?sessionId={session_id}");

    let initialize = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "sse-test", "version": "0.0.1" }
        }
    });
    assert_eq!(
        post(
            server.addr,
            &post_path,
            &good_host,
            None,
            None,
            &initialize.to_string()
        )
        .await,
        202
    );
    let response = response_for(&mut events, 1).await;
    assert_eq!(
        response["result"]["serverInfo"]["name"],
        json!("poc-rust-mcp"),
        "{response}"
    );

    let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
    assert_eq!(
        post(
            server.addr,
            &post_path,
            &good_host,
            None,
            None,
            &initialized.to_string()
        )
        .await,
        202
    );

    let call = json!({
        "jsonrpc": "2.0",
        "id": 2,
        "method": "tools/call",
        "params": { "name": "add", "arguments": { "a": 2, "b": 3 } }
    });
    assert_eq!(
        post(
            server.addr,
            &post_path,
            &good_host,
            None,
            None,
            &call.to_string()
        )
        .await,
        202
    );
    let response = response_for(&mut events, 2).await;
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("no text content in {response}"));
    assert_eq!(text, "2 + 3 = 5");
}

#[tokio::test]
async fn sessions_are_capped() {
    let config = ServerConfig {
        sse_max_sessions: 1,
        ..ServerConfig::default()
    };
    let server = start(config).await;
    let good_host = host(&server);

    let (status, _first) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 200);
    let (status, _second) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 503, "the second session exceeds the cap");
}

#[tokio::test]
async fn a_disconnected_stream_releases_its_session() {
    let server = start_with_keep_alive(ServerConfig::default(), Duration::from_millis(50)).await;
    let good_host = host(&server);

    let (status, mut events) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 200);
    let (_, data) = next_event(&mut events).await;
    let endpoint = data.expect("the endpoint event carries data");
    let session_id = endpoint
        .strip_prefix("/message?sessionId=")
        .unwrap_or_else(|| panic!("unexpected endpoint {endpoint:?}"))
        .to_owned();
    drop(events);

    let post_path = format!("/message?sessionId={session_id}");
    let body = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = post(server.addr, &post_path, &good_host, None, None, &body).await;
        if status == 404 {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a dropped SSE stream must release its session, still {status}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn post_rejections_are_explicit() {
    let server = start(ServerConfig::default()).await;
    let good_host = host(&server);

    // A real session is needed to get past the session lookup.
    let (status, mut events) = get_sse(server.addr, &good_host, None, None).await;
    assert_eq!(status, 200);
    let (_, data) = next_event(&mut events).await;
    let endpoint = data.expect("endpoint data");
    let session_id = endpoint
        .strip_prefix("/message?sessionId=")
        .unwrap_or_else(|| panic!("unexpected endpoint {endpoint:?}"));
    let post_path = format!("/message?sessionId={session_id}");

    let malformed = post(server.addr, &post_path, &good_host, None, None, "{not json").await;
    assert_eq!(malformed, 400, "malformed JSON is a client error");

    let oversized = format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":1,\"padding\":\"{}\"}}",
        "x".repeat(300 * 1024)
    );
    let status = post(server.addr, &post_path, &good_host, None, None, &oversized).await;
    assert_eq!(status, 413, "an oversized body is refused before parsing");
}
