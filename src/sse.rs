//! The SSE transport, with the request validation rmcp's own transports do not do.
//!
//! rmcp's server transports perform no `Host`, `Origin` or bearer-token validation. That is
//! unusable for anything listening beyond loopback, and it is why this module exists. The MCP
//! framing is unchanged — the endpoint event, the
//! `POST /message?sessionId=…` contract, `Sse`/`KeepAlive` — but every request passes through
//! [`authorize`] first, and sessions are bounded and cleaned up.
//!
//! Validation order is deliberate: authority, then `Host` allowlist, then `Origin`, then token. A
//! browser under DNS rebinding sends a foreign `Host`; an allowlisted `Host` is the first thing
//! that can distinguish it, and only after the request is plausibly for this server does spending
//! a token comparison make sense.
//!
//! The transport is a plain `Sink`/`Stream` pair, so rmcp's server loop drives it exactly as it
//! drove its own.

use std::{
    collections::HashMap,
    io,
    marker::PhantomData,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Query, State, rejection::JsonRejection},
    http::{
        HeaderMap, Request, StatusCode,
        header::{AUTHORIZATION, HOST, ORIGIN, RETRY_AFTER, WWW_AUTHENTICATE},
    },
    middleware::{self, Next},
    response::{
        IntoResponse, Response,
        sse::{Event, KeepAlive, Sse},
    },
    routing::{get, post},
};
use futures::{Sink, SinkExt, Stream, StreamExt};
use http::uri::{Authority, Uri};
use rmcp::{
    RoleServer, Service,
    model::ClientJsonRpcMessage,
    service::{RxJsonRpcMessage, ServiceExt, TxJsonRpcMessage},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::{CancellationToken, PollSender};

use crate::config::{
    HostPattern, OriginPattern, ServerConfig, normalize_host, parse_host_pattern,
    parse_origin_pattern,
};

/// Maximum size of one uploaded JSON-RPC message.
pub const MESSAGE_BODY_LIMIT: usize = 256 * 1024;

/// Messages buffered in each direction of one session.
pub const SESSION_QUEUE: usize = 32;

/// How often an idle stream is poked so a dead connection is noticed.
const DEFAULT_KEEP_ALIVE: Duration = Duration::from_secs(15);

/// Why the configured SSE settings cannot be served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SseConfigError {
    /// A non-loopback listener without an `MCP_SSE_TOKEN`.
    NonLoopbackWithoutToken,
    /// A non-loopback listener without an explicit `MCP_SSE_ALLOWED_HOSTS`.
    NonLoopbackWithoutHosts,
    /// An allowlist entry that is not `host` or `host:port`.
    InvalidHost(String),
    /// An allowlist entry that is not `scheme://host[:port]`.
    InvalidOrigin(String),
    /// `MCP_SSE_MAX_SESSIONS` is zero.
    InvalidMaxSessions,
}

impl std::fmt::Display for SseConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonLoopbackWithoutToken => write!(
                f,
                "refusing to serve SSE beyond loopback without MCP_SSE_TOKEN"
            ),
            Self::NonLoopbackWithoutHosts => write!(
                f,
                "refusing to serve SSE beyond loopback without MCP_SSE_ALLOWED_HOSTS"
            ),
            Self::InvalidHost(host) => write!(f, "invalid SSE allowed host {host:?}"),
            Self::InvalidOrigin(origin) => write!(f, "invalid SSE allowed origin {origin:?}"),
            Self::InvalidMaxSessions => write!(f, "MCP_SSE_MAX_SESSIONS must be at least 1"),
        }
    }
}

impl std::error::Error for SseConfigError {}

/// Validated transport settings, derived from [`ServerConfig`].
#[derive(Debug)]
pub struct Settings {
    configured_addr: SocketAddr,
    allowed_hosts: Option<Vec<HostPattern>>,
    allowed_origins: Option<Vec<OriginPattern>>,
    token: Option<String>,
    max_sessions: usize,
    keep_alive: Duration,
}

impl Settings {
    /// Validate the SSE-related part of a resolved configuration.
    ///
    /// # Errors
    ///
    /// Returns [`SseConfigError`] for a malformed allowlist entry, a zero session cap, or a
    /// non-loopback address without the token and host list that make it safe to expose.
    pub fn from_config(config: &ServerConfig) -> Result<Self, SseConfigError> {
        if config.sse_max_sessions == 0 {
            return Err(SseConfigError::InvalidMaxSessions);
        }
        let allowed_hosts = parse_hosts(&config.sse_allowed_hosts)?;
        let allowed_origins = parse_origins(&config.sse_allowed_origins)?;
        let settings = Self {
            configured_addr: config.sse_address,
            allowed_hosts,
            allowed_origins,
            token: config
                .sse_token
                .as_ref()
                .map(|token| token.expose().to_owned()),
            max_sessions: config.sse_max_sessions,
            keep_alive: DEFAULT_KEEP_ALIVE,
        };
        settings.check_non_loopback(settings.configured_addr)?;
        Ok(settings)
    }

    /// Override the keep-alive interval. Tests use a short one so a dropped stream is noticed fast.
    #[doc(hidden)]
    #[must_use]
    pub fn with_keep_alive(mut self, interval: Duration) -> Self {
        self.keep_alive = interval;
        self
    }

    /// Whether every accepted request must carry a bearer token.
    #[must_use]
    pub const fn requires_token(&self) -> bool {
        self.token.is_some()
    }

    fn check_non_loopback(&self, actual: SocketAddr) -> Result<(), SseConfigError> {
        if actual.ip().is_loopback() {
            return Ok(());
        }
        if self.token.is_none() {
            return Err(SseConfigError::NonLoopbackWithoutToken);
        }
        if self.allowed_hosts.as_ref().is_none_or(Vec::is_empty) {
            return Err(SseConfigError::NonLoopbackWithoutHosts);
        }
        Ok(())
    }

    /// Resolve against the address actually bound.
    ///
    /// The listener is authoritative: `127.0.0.1:0` and a configured `127.0.0.1:8000` are the same
    /// policy, but the port in the default `Host` entries has to be the real one.
    fn resolve(&self, actual: SocketAddr) -> Result<ResolvedSettings, SseConfigError> {
        self.check_non_loopback(actual)?;
        let allowed_hosts = self.allowed_hosts.clone().unwrap_or_else(|| {
            let port = actual.port();
            ["127.0.0.1", "localhost", "[::1]"]
                .into_iter()
                .filter_map(|host| parse_host_pattern(&format!("{host}:{port}")))
                .collect()
        });
        Ok(ResolvedSettings {
            allowed_hosts,
            allowed_origins: self.allowed_origins.clone(),
            token: self.token.clone(),
            max_sessions: self.max_sessions,
            keep_alive: self.keep_alive,
        })
    }
}

fn parse_hosts(values: &[String]) -> Result<Option<Vec<HostPattern>>, SseConfigError> {
    if values.is_empty() {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| {
            parse_host_pattern(value).ok_or_else(|| SseConfigError::InvalidHost(value.clone()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn parse_origins(values: &[String]) -> Result<Option<Vec<OriginPattern>>, SseConfigError> {
    if values.is_empty() {
        return Ok(None);
    }
    values
        .iter()
        .map(|value| {
            parse_origin_pattern(value).ok_or_else(|| SseConfigError::InvalidOrigin(value.clone()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

#[derive(Debug)]
struct ResolvedSettings {
    allowed_hosts: Vec<HostPattern>,
    /// `None` means "same host as the request"; see [`origin_allowed`].
    allowed_origins: Option<Vec<OriginPattern>>,
    token: Option<String>,
    max_sessions: usize,
    keep_alive: Duration,
}

/// Serve `router`'s MCP endpoints until `cancel` fires.
///
/// # Errors
///
/// Returns the listener's I/O error, or a configuration error wrapped as one when the settings do
/// not permit serving on the address that was actually bound.
pub async fn serve<F, S>(
    listener: tokio::net::TcpListener,
    settings: Settings,
    service: F,
    cancel: CancellationToken,
) -> io::Result<()>
where
    F: Fn() -> S + Send + Sync + 'static,
    S: Service<RoleServer> + Send + 'static,
{
    let actual = listener.local_addr()?;
    let resolved = settings.resolve(actual).map_err(io::Error::other)?;
    let app = App {
        sessions: Arc::new(Mutex::new(HashMap::new())),
        settings: Arc::new(resolved),
        service: Arc::new(service),
        _service: PhantomData,
    };

    let router = Router::new()
        .route("/sse", get(sse_handler::<F, S>))
        .route("/message", post(post_event_handler::<F, S>))
        .layer(DefaultBodyLimit::max(MESSAGE_BODY_LIMIT))
        .layer(middleware::from_fn_with_state(
            app.clone(),
            authorize::<F, S>,
        ))
        .with_state(app.clone());

    // Cancel every session before waiting for the server to drain: an open SSE response would
    // otherwise keep graceful shutdown waiting forever.
    let sessions = app.sessions.clone();
    let shutdown = cancel.clone();
    tokio::spawn(async move {
        shutdown.cancelled().await;
        let tokens: Vec<CancellationToken> = sessions
            .lock()
            .expect("sessions mutex poisoned")
            .values()
            .map(|session| session.cancel.clone())
            .collect();
        for token in tokens {
            token.cancel();
        }
    });

    axum::serve(listener, router)
        .with_graceful_shutdown(async move { cancel.cancelled().await })
        .await
}

struct App<F, S> {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    settings: Arc<ResolvedSettings>,
    service: Arc<F>,
    _service: PhantomData<fn() -> S>,
}

impl<F, S> Clone for App<F, S> {
    fn clone(&self) -> Self {
        Self {
            sessions: self.sessions.clone(),
            settings: self.settings.clone(),
            service: self.service.clone(),
            _service: PhantomData,
        }
    }
}

struct Session {
    tx: mpsc::Sender<ClientJsonRpcMessage>,
    cancel: CancellationToken,
}

/// Frees one session on drop: cancels its token and removes it from the registry.
///
/// Two independent guards exist — one owned by the response stream, one by the transport — so
/// whichever side ends first releases the slot instead of waiting for the other. Both may run;
/// `HashMap::remove` and `CancellationToken::cancel` are idempotent.
struct SessionCleanup {
    sessions: Arc<Mutex<HashMap<String, Session>>>,
    id: String,
    cancel: CancellationToken,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .remove(&self.id);
    }
}

/// The rmcp-facing half of one session: a bounded sink to the browser and a stream from it.
struct SseTransport {
    stream: ReceiverStream<RxJsonRpcMessage<RoleServer>>,
    sink: PollSender<TxJsonRpcMessage<RoleServer>>,
    _cleanup: SessionCleanup,
}

impl Sink<TxJsonRpcMessage<RoleServer>> for SseTransport {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_ready_unpin(cx).map_err(io::Error::other)
    }

    fn start_send(
        mut self: Pin<&mut Self>,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> Result<(), Self::Error> {
        self.sink.start_send_unpin(item).map_err(io::Error::other)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_flush_unpin(cx).map_err(io::Error::other)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.sink.poll_close_unpin(cx).map_err(io::Error::other)
    }
}

impl Stream for SseTransport {
    type Item = RxJsonRpcMessage<RoleServer>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.stream.poll_next_unpin(cx)
    }
}

/// Reject a request that is not for an allowlisted host, does not come from an allowed origin, or
/// does not carry the configured token.
///
/// The `Err` is a whole `Response` because that is what axum's `from_fn` middleware contract asks
/// for; the box the lint suggests would only be unboxed again by the framework.
#[allow(clippy::result_large_err)]
async fn authorize<F, S>(
    State(app): State<App<F, S>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    let Some((host, port)) = request_authority(&request) else {
        return Err(forbidden("missing or invalid Host"));
    };
    if !host_allowed(&app.settings.allowed_hosts, &host, port) {
        return Err(forbidden("host not allowed"));
    }

    if let Some(origin) = request.headers().get(ORIGIN) {
        let Ok(origin) = origin.to_str() else {
            return Err(forbidden("invalid Origin"));
        };
        if origin == "null"
            || !origin_allowed(app.settings.allowed_origins.as_deref(), origin, &host, port)
        {
            return Err(forbidden("origin not allowed"));
        }
    }

    if let Some(expected) = &app.settings.token
        && !bearer_allowed(expected, request.headers())
    {
        return Err(Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(WWW_AUTHENTICATE, "Bearer")
            .body(Body::empty())
            .expect("a static response is valid"));
    }

    Ok(next.run(request).await)
}

fn forbidden(reason: &'static str) -> Response {
    (StatusCode::FORBIDDEN, reason).into_response()
}

/// Open a session, register it, and stream its events. The `Err` is a whole `Response` for the same
/// reason as [`authorize`].
#[allow(clippy::result_large_err)]
async fn sse_handler<F, S>(
    State(app): State<App<F, S>>,
) -> Result<Sse<impl Stream<Item = Result<Event, io::Error>>>, Response>
where
    F: Fn() -> S + Send + Sync + 'static,
    S: Service<RoleServer> + Send + 'static,
{
    let id = match session_id() {
        Ok(id) => id,
        Err(()) => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "cannot generate a session",
            )
                .into_response());
        }
    };
    let cancel = CancellationToken::new();
    let (from_client_tx, from_client_rx) = mpsc::channel(SESSION_QUEUE);
    let (to_client_tx, to_client_rx) = mpsc::channel(SESSION_QUEUE);

    {
        let mut sessions = app.sessions.lock().expect("sessions mutex poisoned");
        if sessions.len() >= app.settings.max_sessions {
            return Err(Response::builder()
                .status(StatusCode::SERVICE_UNAVAILABLE)
                .header(RETRY_AFTER, "5")
                .body(Body::empty())
                .expect("a static response is valid"));
        }
        sessions.insert(
            id.clone(),
            Session {
                tx: from_client_tx,
                cancel: cancel.clone(),
            },
        );
    }

    // One guard per side, not a shared handle: dropping the HTTP response frees the session
    // without waiting for the transport task, and a service that ends on its own frees it without
    // waiting for the client. Removal is idempotent, so a disconnect and a service end may both
    // run it.
    let transport_cleanup = SessionCleanup {
        sessions: app.sessions.clone(),
        id: id.clone(),
        cancel: cancel.clone(),
    };
    let stream_cleanup = SessionCleanup {
        sessions: app.sessions.clone(),
        id: id.clone(),
        cancel: cancel.clone(),
    };

    let transport = SseTransport {
        stream: ReceiverStream::new(from_client_rx),
        sink: PollSender::new(to_client_tx),
        _cleanup: transport_cleanup,
    };
    let service = (app.service)();
    let service_cancel = cancel.clone();
    tokio::spawn(async move {
        match service.serve_with_ct(transport, service_cancel).await {
            Ok(running) => {
                if let Err(error) = running.waiting().await {
                    tracing::debug!(%error, "SSE session ended with an error");
                }
            }
            Err(error) => tracing::debug!(%error, "could not start the SSE session"),
        }
    });

    let endpoint = futures::stream::once(async move {
        Ok(Event::default()
            .event("endpoint")
            .data(format!("/message?sessionId={id}")))
    });
    let messages = ReceiverStream::new(to_client_rx).map(|message| {
        serde_json::to_string(&message)
            .map(|json| Event::default().event("message").data(json))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    });
    let mut inner = Box::pin(endpoint.chain(messages));
    // The stream owns its own guard, so dropping the HTTP response (a disconnect) frees the
    // session at once rather than waiting for the transport task to notice the closed sink.
    let stream = futures::stream::poll_fn(move |cx| {
        let _keep_alive = &stream_cleanup;
        inner.as_mut().poll_next(cx)
    });

    Ok(Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(app.settings.keep_alive)
            .text("keep-alive"),
    ))
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PostEventQuery {
    session_id: String,
}

async fn post_event_handler<F, S>(
    State(app): State<App<F, S>>,
    Query(PostEventQuery { session_id }): Query<PostEventQuery>,
    payload: Result<Json<ClientJsonRpcMessage>, JsonRejection>,
) -> Response {
    let message = match payload {
        Ok(Json(message)) => message,
        Err(rejection) => {
            let status = match rejection.status() {
                StatusCode::PAYLOAD_TOO_LARGE => StatusCode::PAYLOAD_TOO_LARGE,
                StatusCode::UNSUPPORTED_MEDIA_TYPE => StatusCode::UNSUPPORTED_MEDIA_TYPE,
                _ => StatusCode::BAD_REQUEST,
            };
            return (status, "invalid JSON-RPC message").into_response();
        }
    };

    let sender = {
        let sessions = app.sessions.lock().expect("sessions mutex poisoned");
        sessions.get(&session_id).map(|session| session.tx.clone())
    };
    let Some(sender) = sender else {
        return (StatusCode::NOT_FOUND, "unknown session").into_response();
    };

    match try_send_status(&sender, message) {
        Ok(status) => status.into_response(),
        Err(StatusCode::TOO_MANY_REQUESTS) => Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .header(RETRY_AFTER, "1")
            .body(Body::empty())
            .expect("a static response is valid"),
        Err(status) => status.into_response(),
    }
}

/// Queue one client message without awaiting, mapping the queue state onto HTTP statuses.
fn try_send_status(
    sender: &mpsc::Sender<ClientJsonRpcMessage>,
    message: ClientJsonRpcMessage,
) -> Result<StatusCode, StatusCode> {
    match sender.try_send(message) {
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(mpsc::error::TrySendError::Full(_)) => Err(StatusCode::TOO_MANY_REQUESTS),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(StatusCode::GONE),
    }
}

/// 32 random bytes, hex-encoded: the session capability, so it must not be guessable.
fn session_id() -> Result<String, ()> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|_| ())?;
    let mut id = String::with_capacity(64);
    for byte in bytes {
        use std::fmt::Write;
        let _ = write!(id, "{byte:02x}");
    }
    Ok(id)
}

/// The authority this request claims to be for: `Host` first, then the URI's own authority.
fn request_authority(request: &Request<Body>) -> Option<(String, Option<u16>)> {
    if let Some(value) = request.headers().get(HOST) {
        let value = value.to_str().ok()?;
        let authority: Authority = value.parse().ok()?;
        return normalize_authority(&authority);
    }
    request.uri().authority().and_then(normalize_authority)
}

fn normalize_authority(authority: &Authority) -> Option<(String, Option<u16>)> {
    if authority.as_str().contains('@') || authority.port_u16() == Some(0) {
        return None;
    }
    Some((normalize_host(authority.host())?, authority.port_u16()))
}

/// An allowlisted entry without a port matches any port on that host; one with a port is pinned.
///
/// A request authority with no explicit port names the scheme's default. This transport is plain
/// HTTP, so that is 80 — without this a loopback bind to :80 would refuse `Host: localhost`, which
/// names exactly the port it is listening on.
fn host_allowed(patterns: &[HostPattern], host: &str, port: Option<u16>) -> bool {
    let effective_port = port.unwrap_or(80);
    patterns.iter().any(|pattern| {
        pattern.host == host && pattern.port.is_none_or(|pinned| pinned == effective_port)
    })
}

/// Parse an `Origin` into `(scheme, host, effective_port)`.
fn parse_origin(value: &str) -> Option<(String, String, u16)> {
    let uri: Uri = value.parse().ok()?;
    let scheme = uri.scheme_str()?.to_ascii_lowercase();
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" => 443,
        _ => return None,
    };
    let authority = uri.authority()?;
    if authority.as_str().contains('@') || authority.port_u16() == Some(0) {
        return None;
    }
    if let Some(path_and_query) = uri.path_and_query() {
        let path = path_and_query.path();
        if (!path.is_empty() && path != "/") || path_and_query.query().is_some() {
            return None;
        }
    }
    Some((
        scheme,
        normalize_host(authority.host())?,
        authority.port_u16().unwrap_or(default_port),
    ))
}

/// Decide whether a browser `Origin` is acceptable for a request whose authority is `host[:port]`.
///
/// With an explicit allowlist, scheme, host and (when pinned) port must match. Without one, the
/// only requirement is same-host — and the port only when the request authority carried one, since
/// a reverse proxy may forward `Host: mcp.example` for several local ports.
fn origin_allowed(
    patterns: Option<&[OriginPattern]>,
    origin: &str,
    host: &str,
    port: Option<u16>,
) -> bool {
    let Some((scheme, origin_host, origin_port)) = parse_origin(origin) else {
        return false;
    };
    match patterns {
        Some(patterns) => patterns.iter().any(|pattern| {
            pattern.scheme == scheme
                && pattern.host == origin_host
                && pattern.port.is_none_or(|pinned| pinned == origin_port)
        }),
        None => origin_host == host && port.is_none_or(|request_port| request_port == origin_port),
    }
}

/// Compare a bearer token without an early exit on the first differing byte.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    let mut difference = a.len() ^ b.len();
    for index in 0..a.len().max(b.len()) {
        difference |=
            usize::from(a.get(index).copied().unwrap_or(0) ^ b.get(index).copied().unwrap_or(0));
    }
    difference == 0
}

/// `Authorization: Bearer <token>`, header-only by design.
fn bearer_allowed(expected: &str, headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("bearer") && constant_time_eq(expected.as_bytes(), token.as_bytes())
}

/// Access to the pieces worth testing directly, without opening a socket.
#[doc(hidden)]
pub mod testing {
    use super::*;

    /// See the private [`super::try_send_status`].
    pub fn try_send_status(
        sender: &mpsc::Sender<ClientJsonRpcMessage>,
        message: ClientJsonRpcMessage,
    ) -> Result<StatusCode, StatusCode> {
        super::try_send_status(sender, message)
    }

    /// See the private [`super::normalize_authority`].
    pub fn normalize_authority(authority: &Authority) -> Option<(String, Option<u16>)> {
        super::normalize_authority(authority)
    }

    /// See the private [`super::constant_time_eq`].
    pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
        super::constant_time_eq(a, b)
    }

    /// See the private [`super::parse_origin`].
    pub fn parse_origin(value: &str) -> Option<(String, String, u16)> {
        super::parse_origin(value)
    }

    /// See the private [`super::host_allowed`]. Patterns are given as their textual form so a test
    /// does not need `config::parse_host_pattern`.
    pub fn host_allowed(values: &[String], host: &str, port: Option<u16>) -> bool {
        let patterns: Vec<HostPattern> = values
            .iter()
            .filter_map(|value| parse_host_pattern(value))
            .collect();
        super::host_allowed(&patterns, host, port)
    }
}
