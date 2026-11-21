use crate::{catalog, config::Config, model_policy, models_dev::Database};
use anyhow::{Context, Result, bail};
use axum::{
    Router,
    body::Body,
    extract::{DefaultBodyLimit, FromRequest, Multipart, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use std::{
    io::Read,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct ProxyState {
    pub config: Arc<Config>,
    pub metadata: Arc<RwLock<Database>>,
    pub cancel: CancellationToken,
    client: reqwest::Client,
}

impl ProxyState {
    pub fn new(config: Config, metadata: Database) -> Result<Self> {
        config.validate()?;
        let client = http_client(
            Duration::from_secs(config.upstream.connect_timeout_secs),
            Duration::from_secs(config.upstream.read_timeout_secs),
        )?;
        Ok(Self {
            config: Arc::new(config),
            metadata: Arc::new(RwLock::new(metadata)),
            cancel: CancellationToken::new(),
            client,
        })
    }
}

pub fn http_client(connect_timeout: Duration, read_timeout: Duration) -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .user_agent(concat!("codex-newapi-gateway/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

pub fn router(state: ProxyState) -> Router {
    Router::new().fallback(forward).with_state(state)
}

fn error(status: StatusCode, message: &str) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"error":{"message":message,"type":"gateway_error"}})),
    )
        .into_response()
}

fn extract_key(headers: &HeaderMap, cfg: &Config) -> Result<HeaderValue, &'static str> {
    let configured = HeaderName::from_bytes(cfg.auth.key_header.as_bytes()).unwrap();
    let raw = if headers.contains_key(&configured) {
        let mut values = headers.get_all(&configured).iter();
        let first = values.next().unwrap();
        if values.next().is_some() {
            return Err("Multiple API keys are not accepted");
        }
        first.to_str().map_err(|_| "Invalid API key header")?
    } else if cfg.auth.allow_bearer_fallback {
        let mut values = headers.get_all("authorization").iter();
        let first = values.next().ok_or("New API key required")?;
        if values.next().is_some() {
            return Err("Multiple authorization headers are not accepted");
        }
        let value = first.to_str().map_err(|_| "Invalid authorization header")?;
        let (scheme, key) = value
            .split_once(' ')
            .ok_or("Bearer authentication required")?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return Err("Bearer authentication required");
        }
        key
    } else {
        return Err("New API key required");
    };
    if raw.is_empty() || raw.len() > 4096 || !raw.bytes().all(|v| (33..=126).contains(&v)) {
        return Err("Invalid API key");
    }
    let mut value =
        HeaderValue::from_str(&format!("Bearer {raw}")).map_err(|_| "Invalid API key")?;
    value.set_sensitive(true);
    Ok(value)
}

// Preserve WS frames byte-for-byte: New API owns protocol parsing, ACLs and billing.
async fn forward_websocket(
    state: &ProxyState,
    parts: axum::http::request::Parts,
    auth: HeaderValue,
    incoming_upgrade: hyper::upgrade::OnUpgrade,
) -> Response {
    if !parts
        .headers
        .get("upgrade")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
    {
        return error(
            StatusCode::UPGRADE_REQUIRED,
            "WebSocket upgrade headers required",
        );
    }
    let url = format!(
        "{}/responses{}",
        state.config.upstream.base_url,
        parts
            .uri
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let mut request = state
        .client
        .get(url)
        .version(reqwest::Version::HTTP_11)
        .header("authorization", auth)
        .header("connection", "upgrade")
        .header("upgrade", "websocket");
    for name in [
        "sec-websocket-key",
        "sec-websocket-version",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
        "openai-beta",
        "originator",
        "x-forwarded-for",
        "x-real-ip",
    ] {
        if let Some(value) = parts.headers.get(name) {
            request = request.header(name, value);
        }
    }
    let response = tokio::select! {
        _ = state.cancel.cancelled() => return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway stopping"),
        r = request.send() => match r {
            Ok(r) => r,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "New API WebSocket connection failed"),
        }
    };
    let status = response.status();
    if matches!(
        status,
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::UPGRADE_REQUIRED
    ) {
        return error(
            StatusCode::UPGRADE_REQUIRED,
            "New API does not support this WebSocket route; use HTTP streaming",
        );
    }
    if status != StatusCode::SWITCHING_PROTOCOLS {
        if status.is_redirection() {
            return error(StatusCode::BAD_GATEWAY, "Upstream redirect refused");
        }
        if status.is_success() {
            return error(
                StatusCode::BAD_GATEWAY,
                "New API did not upgrade the WebSocket connection",
            );
        }
        let mut builder = Response::builder().status(status);
        for name in ["content-type", "retry-after", "x-request-id"] {
            if let Some(value) = response.headers().get(name) {
                builder = builder.header(name, value);
            }
        }
        return builder
            .body(Body::from_stream(response.bytes_stream()))
            .unwrap();
    }
    let mut builder = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("connection", "upgrade")
        .header("upgrade", "websocket");
    for name in [
        "sec-websocket-accept",
        "sec-websocket-protocol",
        "sec-websocket-extensions",
        "x-request-id",
    ] {
        if let Some(value) = response.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    let mut upstream = match response.upgrade().await {
        Ok(s) => s,
        Err(_) => return error(StatusCode::BAD_GATEWAY, "New API WebSocket upgrade failed"),
    };
    let cancel = state.cancel.clone();
    tokio::spawn(async move {
        let started = Instant::now();
        let down = tokio::select! {
            _ = cancel.cancelled() => return,
            r = incoming_upgrade => r,
        };
        let down = match down {
            Ok(s) => s,
            Err(_) => {
                eprintln!("ws downstream_upgrade_failed");
                return;
            }
        };
        let mut down = hyper_util::rt::TokioIo::new(down);
        tokio::select! {
            _ = cancel.cancelled() => {},
            result = tokio::io::copy_bidirectional(&mut down, &mut upstream) => {
                match result {
                    Ok((sent, received)) => eprintln!(
                        "ws closed sent_bytes={} received_bytes={} elapsed_ms={}",
                        sent,
                        received,
                        started.elapsed().as_millis()
                    ),
                    Err(_) => eprintln!(
                        "ws disconnected elapsed_ms={}",
                        started.elapsed().as_millis()
                    ),
                }
            }
        }
    });
    builder.body(Body::empty()).unwrap()
}

async fn forward(State(state): State<ProxyState>, request: Request) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let allowed = matches!(
        (method.as_str(), path.as_str()),
        ("GET", "/models")
            | ("GET", "/responses")
            | ("POST", "/responses")
            | ("POST", "/responses/compact")
            | ("POST", "/alpha/search")
            | ("POST", "/images/generations")
            | ("POST", "/images/edits")
    );
    if method == Method::GET && path == "/healthz" {
        return axum::Json(serde_json::json!({
            "ok": true,
            "service": "codex-newapi-gateway",
            "version": env!("CARGO_PKG_VERSION"),
            "metadata_records": state.metadata.read().unwrap().records.len(),
        }))
        .into_response();
    }
    if !allowed {
        return error(StatusCode::NOT_FOUND, "Unsupported API route");
    }
    let started = Instant::now();
    let response = forward_inner(&state, request).await;
    eprintln!(
        "request method={} path={} status={} headers_ms={}",
        method,
        path,
        response.status().as_u16(),
        started.elapsed().as_millis()
    );
    response
}

async fn forward_inner(state: &ProxyState, mut request: Request) -> Response {
    let incoming_upgrade =
        if request.method() == Method::GET && request.uri().path() == "/responses" {
            Some(hyper::upgrade::on(&mut request))
        } else {
            None
        };
    let (parts, mut body) = request.into_parts();
    if state.config.server.reject_browser_origin && parts.headers.contains_key("origin") {
        return error(StatusCode::FORBIDDEN, "Browser origins are not accepted");
    }
    let auth = match extract_key(&parts.headers, &state.config) {
        Ok(k) => k,
        Err(e) => return error(StatusCode::UNAUTHORIZED, e),
    };
    let has_disabled_models = state
        .config
        .models
        .values()
        .any(|p| p.enabled == Some(false));
    if parts.method == Method::GET && parts.uri.path() == "/responses" {
        if has_disabled_models {
            return error(
                StatusCode::UPGRADE_REQUIRED,
                "Use HTTP streaming to enforce gateway model policies",
            );
        }
        return forward_websocket(state, parts, auth, incoming_upgrade.unwrap()).await;
    }
    let is_models = parts.method == Method::GET && parts.uri.path() == "/models";
    if !is_models && has_disabled_models {
        let inspection_limit = state.config.server.max_inspection_body_bytes;
        let bytes = match axum::body::to_bytes(body, inspection_limit).await {
            Ok(v) => v,
            Err(_) => {
                return error(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request exceeds policy inspection limit",
                );
            }
        };
        let inspected = match parts.headers.get("content-encoding") {
            None => bytes.clone(),
            Some(value) if value.as_bytes().eq_ignore_ascii_case(b"zstd") => {
                let compressed = bytes.clone();
                match tokio::task::spawn_blocking(move || {
                    inspect_zstd(&compressed, inspection_limit)
                })
                .await
                {
                    Ok(Ok(v)) => v,
                    Ok(Err(InspectionError::TooLarge)) => {
                        return error(
                            StatusCode::PAYLOAD_TOO_LARGE,
                            "Decompressed request exceeds policy inspection limit",
                        );
                    }
                    _ => return error(StatusCode::BAD_REQUEST, "Invalid compressed request"),
                }
            }
            _ => {
                return error(
                    StatusCode::UNSUPPORTED_MEDIA_TYPE,
                    "Unsupported request encoding for model policy",
                );
            }
        };
        let model = match requested_model(&parts.headers, inspected, inspection_limit).await {
            Ok(v) => v,
            Err(_) => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "An unambiguous model field is required",
                );
            }
        };
        if state
            .config
            .models
            .get(&model)
            .is_some_and(|p| p.enabled == Some(false))
        {
            return error(StatusCode::FORBIDDEN, "Model disabled by gateway policy");
        }
        body = Body::from(bytes);
    }
    let target = format!(
        "{}{}{}",
        state.config.upstream.base_url,
        parts.uri.path(),
        parts
            .uri
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let mut outgoing = state
        .client
        .request(parts.method, &target)
        .header("authorization", auth);
    for name in [
        "content-type",
        "content-encoding",
        "content-length",
        "accept",
        "x-forwarded-for",
        "x-real-ip",
    ] {
        if let Some(value) = parts.headers.get(name) {
            outgoing = outgoing.header(name, value);
        }
    }
    outgoing = outgoing.body(reqwest::Body::wrap_stream(body.into_data_stream()));
    let response = tokio::select! {
        _ = state.cancel.cancelled() => return error(StatusCode::SERVICE_UNAVAILABLE, "Gateway stopping"),
        result = outgoing.send() => match result {
            Ok(v) => v,
            Err(_) => return error(StatusCode::BAD_GATEWAY, "New API connection failed"),
        }
    };
    if response.status().is_redirection() {
        return error(StatusCode::BAD_GATEWAY, "Upstream redirect refused");
    }
    if is_models && response.status().is_success() {
        let max_models_bytes = state.config.server.max_models_response_bytes;
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(v) => v,
                Err(_) => {
                    return error(StatusCode::BAD_GATEWAY, "Failed to read New API model list");
                }
            };
            if bytes.len() + chunk.len() > max_models_bytes {
                return error(StatusCode::BAD_GATEWAY, "New API model list too large");
            }
            bytes.extend_from_slice(&chunk);
        }
        let document = (|| -> anyhow::Result<_> {
            let doc: serde_json::Value = serde_json::from_slice(&bytes)?;
            let ids = catalog::parse_ids(&doc)?;
            let entries =
                model_policy::resolve(&ids, &state.config, &state.metadata.read().unwrap())?;
            catalog::build_document(&entries, &state.config.catalog)
        })();
        let vary_header = format!("Authorization, {}", state.config.auth.key_header);
        return match document {
            Ok(doc) => (
                [
                    ("cache-control", "no-store".to_owned()),
                    ("vary", vary_header),
                ],
                axum::Json(doc),
            )
                .into_response(),
            Err(_) => error(
                StatusCode::BAD_GATEWAY,
                "Invalid New API model list or model metadata",
            ),
        };
    }
    let mut builder = Response::builder().status(response.status());
    for name in [
        "content-type",
        "content-encoding",
        "cache-control",
        "retry-after",
        "x-request-id",
    ] {
        if let Some(value) = response.headers().get(name) {
            builder = builder.header(name, value);
        }
    }
    if is_models {
        builder = builder.header("cache-control", "no-store");
    }
    let stream = response
        .bytes_stream()
        .take_until(state.cancel.clone().cancelled_owned());
    builder.body(Body::from_stream(stream)).unwrap()
}

enum InspectionError {
    TooLarge,
    Invalid,
}

fn inspect_zstd(
    compressed: &[u8],
    limit: usize,
) -> std::result::Result<bytes::Bytes, InspectionError> {
    let decoder = zstd::stream::read::Decoder::new(std::io::Cursor::new(compressed))
        .map_err(|_| InspectionError::Invalid)?;
    let mut decoded = Vec::new();
    decoder
        .take(limit as u64 + 1)
        .read_to_end(&mut decoded)
        .map_err(|_| InspectionError::Invalid)?;
    if decoded.len() > limit {
        return Err(InspectionError::TooLarge);
    }
    Ok(decoded.into())
}

async fn requested_model(headers: &HeaderMap, bytes: bytes::Bytes, limit: usize) -> Result<String> {
    let content_type = headers
        .get("content-type")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("application/json");
    if content_type.starts_with("multipart/form-data") {
        let mut request = Request::new(Body::from(bytes));
        *request.headers_mut() = headers.clone();
        DefaultBodyLimit::max(limit).apply(&mut request);
        let mut multipart = Multipart::from_request(request, &()).await?;
        let mut model = None;
        while let Some(field) = multipart.next_field().await? {
            if field.name() == Some("model") {
                if model.is_some() {
                    bail!("duplicate model");
                }
                model = Some(field.text().await?);
            }
        }
        return model.context("missing model");
    }
    // Deserialize the field directly so duplicate JSON model keys are rejected.
    #[derive(serde::Deserialize)]
    struct Model {
        model: String,
    }
    Ok(serde_json::from_slice::<Model>(&bytes)?.model)
}
