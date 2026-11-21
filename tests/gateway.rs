use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::any,
};
use codex_newapi_gateway::{
    catalog::{self, ServiceTier},
    config::Config,
    model_policy, models_dev,
    proxy::{self, ProxyState},
};
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

type CapturedCalls = Arc<Mutex<Vec<(String, HeaderMap, Vec<u8>)>>>;

#[derive(Clone, Default)]
struct Mock {
    calls: CapturedCalls,
    revoked: Arc<AtomicBool>,
    websocket: Arc<AtomicBool>,
    websocket_headers: Arc<Mutex<Vec<HeaderMap>>>,
}

async fn upstream(State(s): State<Mock>, mut r: Request) -> Response {
    if r.method() == axum::http::Method::GET && r.uri().path() == "/v1/responses" {
        s.websocket_headers
            .lock()
            .unwrap()
            .push(r.headers().clone());
        if r.headers()
            .get("authorization")
            .is_none_or(|v| v != "Bearer key-a")
        {
            return StatusCode::UNAUTHORIZED.into_response();
        }
        if !s.websocket.load(Ordering::Relaxed) {
            return StatusCode::NOT_FOUND.into_response();
        }
        let upgrade = hyper::upgrade::on(&mut r);
        tokio::spawn(async move {
            let io = hyper_util::rt::TokioIo::new(upgrade.await.unwrap());
            let (mut reader, mut writer) = tokio::io::split(io);
            tokio::io::copy(&mut reader, &mut writer).await.unwrap();
        });
        return Response::builder()
            .status(101)
            .header("connection", "upgrade")
            .header("upgrade", "websocket")
            .header("sec-websocket-accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
            .body(Body::empty())
            .unwrap();
    }

    let (p, b) = r.into_parts();
    let bytes = to_bytes(b, 1024 * 1024).await.unwrap();
    s.calls
        .lock()
        .unwrap()
        .push((p.uri.to_string(), p.headers.clone(), bytes.to_vec()));
    let auth = p
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !matches!(auth, "Bearer key-a" | "Bearer key-b") || s.revoked.load(Ordering::Relaxed) {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(json!({"error":{"message":"invalid token"}})),
        )
            .into_response();
    }
    if p.uri.path() == "/v1/models" {
        return axum::Json(
            json!({"data":[{"id":if auth=="Bearer key-a"{"model-a"}else{"model-b"}}]}),
        )
        .into_response();
    }
    if p.uri.path() == "/v1/responses"
        && p.headers
            .get("accept")
            .is_some_and(|v| v == "text/event-stream")
    {
        let stream = futures_util::stream::unfold(0, |step| async move {
            match step {
                0 => Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from_static(b"data: first\n\n")),
                    1,
                )),
                1 => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Some((Ok(bytes::Bytes::from_static(b"data: done\n\n")), 2))
                }
                _ => None,
            }
        });
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    if p.uri.path() == "/v1/images/edits" {
        return Response::builder()
            .status(429)
            .header("retry-after", "7")
            .header("set-cookie", "must-not-forward=1")
            .body(Body::from("limited"))
            .unwrap();
    }
    axum::Json(json!({"ok":true})).into_response()
}

async fn serve(r: Router) -> (String, tokio::task::JoinHandle<()>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    let h = tokio::spawn(async move { axum::serve(l, r).await.unwrap() });
    (base, h)
}

struct Fixture {
    base: String,
    state: ProxyState,
    mock: Mock,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state.cancel.cancel();
        for t in &self.tasks {
            t.abort();
        }
    }
}

async fn fixture(mut c: Config) -> Fixture {
    let mock = Mock::default();
    let (up, a) = serve(
        Router::new()
            .fallback(any(upstream))
            .with_state(mock.clone()),
    )
    .await;
    c.upstream.base_url = format!("{up}/v1");
    let state = ProxyState::new(c, Default::default()).unwrap();
    let (base, b) = serve(proxy::router(state.clone())).await;
    Fixture {
        base,
        state,
        mock,
        tasks: vec![a, b],
    }
}

#[tokio::test]
async fn models_are_per_request_and_revocation_is_not_cached() {
    let f = fixture(Default::default()).await;
    let c = reqwest::Client::new();
    for (key, id) in [("key-a", "model-a"), ("key-b", "model-b")] {
        let r = c
            .get(format!("{}/models", f.base))
            .header("X-NewAPI-Key", key)
            .send()
            .await
            .unwrap();
        assert_eq!(r.headers()["cache-control"], "no-store");
        assert_eq!(r.headers()["vary"], "Authorization, X-NewAPI-Key");
        let d: Value = r.json().await.unwrap();
        assert_eq!(d["models"][0]["slug"], id);
        assert_eq!(d["models"].as_array().unwrap().len(), 1);
        assert_eq!(d["models"][0]["service_tiers"][0]["id"], "priority");
        assert_eq!(d["models"][0]["additional_speed_tiers"], json!(["fast"]));
    }
    f.mock.revoked.store(true, Ordering::Relaxed);
    assert_eq!(
        c.get(format!("{}/models", f.base))
            .header("X-NewAPI-Key", "key-a")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(f.mock.calls.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn dedicated_key_wins_and_official_identity_never_reaches_newapi() {
    let f = fixture(Default::default()).await;
    let r = reqwest::Client::new()
        .post(format!("{}/responses?trace=one", f.base))
        .header("X-NewAPI-Key", "key-a")
        .bearer_auth("chatgpt-token")
        .header("cookie", "secret=1")
        .header("chatgpt-account-id", "private-account")
        .header("content-type", "application/json")
        .body(r#"{"model":"unchanged-id","input":"unchanged"}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let calls = f.mock.calls.lock().unwrap();
    let (p, h, b) = &calls[0];
    assert_eq!(p, "/v1/responses?trace=one");
    assert_eq!(h["authorization"], "Bearer key-a");
    for name in ["x-newapi-key", "cookie", "chatgpt-account-id"] {
        assert!(!h.contains_key(name));
    }
    assert_eq!(b, br#"{"model":"unchanged-id","input":"unchanged"}"#);
}

#[tokio::test]
async fn response_service_tier_is_forwarded_without_rewriting_or_defaulting() {
    let f = fixture(Default::default()).await;
    let client = reqwest::Client::new();
    let payloads: [&[u8]; 4] = [
        br#"{"model":"model-a","input":"unchanged"}"#,
        br#"{"model":"model-a","service_tier":"default","input":"unchanged"}"#,
        br#"{"model":"model-a","service_tier":"priority","input":"unchanged"}"#,
        br#"{"model":"model-fast","service_tier":"ultrafast","input":"unchanged"}"#,
    ];
    for payload in payloads {
        let response = client
            .post(format!("{}/responses", f.base))
            .header("X-NewAPI-Key", "key-a")
            .header("content-type", "application/json")
            .body(payload.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    let calls = f.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), payloads.len());
    for ((path, headers, body), payload) in calls.iter().zip(payloads) {
        assert_eq!(path, "/v1/responses");
        assert_eq!(headers["authorization"], "Bearer key-a");
        assert_eq!(body, payload);
    }
}

#[tokio::test]
async fn missing_invalid_dedicated_key_never_falls_back() {
    let f = fixture(Default::default()).await;
    let c = reqwest::Client::new();
    let url = format!("{}/models", f.base);
    assert_eq!(c.get(&url).send().await.unwrap().status(), 401);
    assert!(f.mock.calls.lock().unwrap().is_empty());
    assert_eq!(
        c.get(&url)
            .header("X-NewAPI-Key", "")
            .bearer_auth("key-a")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        c.get(&url)
            .header("X-NewAPI-Key", "wrong-key")
            .bearer_auth("key-a")
            .send()
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(f.mock.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn streaming_and_compatibility_routes_are_preserved() {
    let f = fixture(Default::default()).await;
    let c = reqwest::Client::new();
    let r = c
        .post(format!("{}/responses", f.base))
        .header("X-NewAPI-Key", "key-a")
        .header("accept", "text/event-stream")
        .body("{}")
        .send()
        .await
        .unwrap();
    let mut stream = r.bytes_stream();
    let first = tokio::time::timeout(Duration::from_millis(250), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), b"data: first\n\n");
    assert_eq!(
        stream.next().await.unwrap().unwrap().as_ref(),
        b"data: done\n\n"
    );
    for route in ["responses/compact", "alpha/search", "images/generations"] {
        assert_eq!(
            c.post(format!("{}/{route}", f.base))
                .header("X-NewAPI-Key", "key-a")
                .body("{}")
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
    }
    let r = c
        .post(format!("{}/images/edits", f.base))
        .header("X-NewAPI-Key", "key-a")
        .body(vec![0, 1, 255])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 429);
    assert_eq!(r.headers()["retry-after"], "7");
    assert!(!r.headers().contains_key("set-cookie"));
}

#[tokio::test]
async fn hidden_model_remains_usable_disabled_model_does_not() {
    let mut cfg = Config::default();
    cfg.models.insert(
        "model-a".into(),
        model_policy::ModelOverride {
            visible: Some(false),
            ..Default::default()
        },
    );
    cfg.models.insert(
        "blocked".into(),
        model_policy::ModelOverride {
            enabled: Some(false),
            ..Default::default()
        },
    );
    let f = fixture(cfg).await;
    let c = reqwest::Client::new();
    let d: Value = c
        .get(format!("{}/models", f.base))
        .header("X-NewAPI-Key", "key-a")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(d["models"][0]["visibility"], "hide");
    assert_eq!(d["models"].as_array().unwrap().len(), 1);
    let bytes = zstd::stream::encode_all(br#"{"model":"model-a"}"#.as_slice(), 1).unwrap();
    assert_eq!(
        c.post(format!("{}/responses", f.base))
            .header("X-NewAPI-Key", "key-a")
            .header("content-encoding", "zstd")
            .body(bytes.clone())
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(f.mock.calls.lock().unwrap().last().unwrap().2, bytes);
    let before = f.mock.calls.lock().unwrap().len();
    assert_eq!(
        c.post(format!("{}/responses", f.base))
            .header("X-NewAPI-Key", "key-a")
            .json(&json!({"model":"blocked"}))
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(f.mock.calls.lock().unwrap().len(), before);
}

#[tokio::test]
async fn unknown_routes_browser_requests_and_incomplete_websocket_handshakes_are_rejected() {
    let f = fixture(Default::default()).await;
    let c = reqwest::Client::new();
    assert_eq!(
        c.get(format!("{}/admin", f.base))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        c.get(format!("{}/models", f.base))
            .header("X-NewAPI-Key", "key-a")
            .header("origin", "https://example.org")
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        c.get(format!("{}/responses", f.base))
            .header("X-NewAPI-Key", "key-a")
            .header("upgrade", "websocket")
            .send()
            .await
            .unwrap()
            .status(),
        426
    );
    assert!(f.mock.calls.lock().unwrap().is_empty());
}

#[test]
fn full_original_codex_metadata_shape_is_preserved() {
    let c = Config::default();
    let entries = model_policy::resolve(&["model-a".into()], &c, &Default::default()).unwrap();
    let d = catalog::build_document(&entries, &c.catalog).unwrap();
    let m = &d["models"][0];
    let expected = [
        "slug",
        "display_name",
        "description",
        "default_reasoning_level",
        "supported_reasoning_levels",
        "multi_agent_reasoning_effort",
        "service_tiers",
        "additional_speed_tiers",
        "shell_type",
        "visibility",
        "supported_in_api",
        "priority",
        "base_instructions",
        "model_messages",
        "supports_reasoning_summaries",
        "supports_reasoning_summary_parameter",
        "support_verbosity",
        "default_reasoning_summary",
        "supports_parallel_tool_calls",
        "truncation_policy",
        "context_window",
        "max_context_window",
        "auto_compact_token_limit",
        "effective_context_window_percent",
        "experimental_supported_tools",
        "supports_search_tool",
        "input_modalities",
        "use_responses_lite",
    ];
    assert_eq!(m.as_object().unwrap().len(), expected.len());
    for key in expected {
        assert!(m.get(key).is_some(), "{key}");
    }
    assert_eq!(m["slug"], "model-a");
    assert_eq!(m["use_responses_lite"], false);
    assert_eq!(m["context_window"], 1_000_000);
    assert_eq!(m["auto_compact_token_limit"], 900_000);
    assert_eq!(m["multi_agent_reasoning_effort"], "max");
    assert_eq!(
        m["model_messages"]["instructions_template"],
        m["base_instructions"]
    );
}

#[test]
fn service_tiers_are_configurable_without_hardcoded_model_ids() {
    let ids = [
        "model-primary",
        "model-fast",
        "model-ultrafast",
        "model-image",
        "model-autoreview",
        "disabled-model",
    ]
    .map(str::to_owned);

    // By default, no model gets hardcoded extra tiers.
    let mut config = Config::default();
    config.models.insert(
        "model-ultrafast".into(),
        model_policy::ModelOverride {
            extra_service_tiers: vec![ServiceTier {
                id: "ultrafast".into(),
                name: "Ultrafast".into(),
                description: "ultrafast mode".into(),
            }],
            ..Default::default()
        },
    );
    config.models.insert(
        "model-fast".into(),
        model_policy::ModelOverride {
            visible: Some(false),
            ..Default::default()
        },
    );
    config.models.insert(
        "disabled-model".into(),
        model_policy::ModelOverride {
            enabled: Some(false),
            ..Default::default()
        },
    );
    config.models.insert(
        "not-permitted".into(),
        model_policy::ModelOverride::default(),
    );
    config.validate().unwrap();

    let entries = model_policy::resolve(&ids, &config, &Default::default()).unwrap();
    let document = catalog::build_document(&entries, &config.catalog).unwrap();
    let models = document["models"].as_array().unwrap();
    assert_eq!(models.len(), ids.len() - 1);
    for model in models {
        assert_ne!(model["slug"], "disabled-model");
        assert_ne!(model["slug"], "not-permitted");
        let tiers = model["service_tiers"].as_array().unwrap();
        if model["slug"] == "model-ultrafast" {
            assert_eq!(tiers.len(), 2);
            assert_eq!(
                tiers[1],
                json!({"id": "ultrafast", "name": "Ultrafast", "description": "ultrafast mode"})
            );
        } else {
            assert_eq!(tiers.len(), 1);
        }
        assert_eq!(tiers[0]["id"], "priority");
        assert_eq!(tiers[0]["name"], "Fast");
        assert_eq!(tiers[0]["description"], "fast mode");
        assert_eq!(model["additional_speed_tiers"], json!(["fast"]));
        assert!(model.get("default_service_tier").is_none());
        assert!(model.get("default_speed_tier").is_none());
        assert_eq!(model["default_reasoning_level"], "xhigh");
        assert_eq!(model["multi_agent_reasoning_effort"], "max");
        assert_eq!(
            model["supported_reasoning_levels"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()["effort"],
            "ultra"
        );
    }
    for id in ["model-fast", "model-image", "model-autoreview"] {
        let model = models.iter().find(|model| model["slug"] == id).unwrap();
        assert_eq!(model["visibility"], "hide");
        assert_eq!(model["supported_in_api"], true);
    }

    // Disabling a model with extra tiers removes it completely.
    config.models.insert(
        "model-ultrafast".into(),
        model_policy::ModelOverride {
            enabled: Some(false),
            ..Default::default()
        },
    );
    let disabled_entries = model_policy::resolve(&ids, &config, &Default::default()).unwrap();
    let disabled_document = catalog::build_document(&disabled_entries, &config.catalog).unwrap();
    assert!(
        disabled_document["models"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["slug"] != "model-ultrafast")
    );
}

#[test]
fn hidden_keywords_are_configurable_and_default_to_image_and_autoreview() {
    let cases = [
        ("gpt-image-2", false),
        ("vendor/GPT-IMAGE-3", false),
        ("codex-auto-review", false),
        ("CODEX_AUTOREVIEW", false),
        ("codex_auto_review", false),
        ("vendor/autoreview-v2", false),
        ("model-primary", true),
        ("model-fast", true),
    ];
    let ids: Vec<String> = cases.iter().map(|(id, _)| (*id).to_owned()).collect();
    let config = Config::default();
    let entries = model_policy::resolve(&ids, &config, &Default::default()).unwrap();
    let document = catalog::build_document(&entries, &config.catalog).unwrap();
    let models = document["models"].as_array().unwrap();
    assert_eq!(models.len(), cases.len());
    for ((entry, model), (id, visible)) in entries.iter().zip(models).zip(cases) {
        assert_eq!(entry.visible, visible, "{id}");
        assert!(entry.enabled, "hidden models must remain enabled: {id}");
        assert_eq!(model["slug"], id);
        assert_eq!(model["visibility"], if visible { "list" } else { "hide" });
        assert_eq!(model["supported_in_api"], true);
    }

    // Custom hidden keywords replace defaults cleanly.
    let mut custom = Config::default();
    custom.catalog.hidden_keywords = vec!["internal".into()];
    let custom_ids = ["gpt-image-2", "internal-helper"].map(str::to_owned);
    let custom_entries = model_policy::resolve(&custom_ids, &custom, &Default::default()).unwrap();
    assert_eq!(
        custom_entries.iter().map(|e| e.visible).collect::<Vec<_>>(),
        vec![true, false]
    );
}

#[test]
fn explicit_model_visibility_overrides_keyword_defaults() {
    let mut config = Config::default();
    for id in ["gpt-image-2", "codex-auto-review"] {
        config.models.insert(
            id.into(),
            model_policy::ModelOverride {
                visible: Some(true),
                ..Default::default()
            },
        );
    }
    config.models.insert(
        "model-primary".into(),
        model_policy::ModelOverride {
            visible: Some(false),
            ..Default::default()
        },
    );
    let ids = ["gpt-image-2", "codex-auto-review", "model-primary"].map(str::to_owned);
    let entries = model_policy::resolve(&ids, &config, &Default::default()).unwrap();
    assert_eq!(
        entries
            .iter()
            .map(|entry| entry.visible)
            .collect::<Vec<_>>(),
        vec![true, true, false]
    );
    assert!(entries.iter().all(|entry| entry.enabled));
}

#[test]
fn auto_review_binding_is_configured_and_respects_catalog_and_disabled_models() {
    let ids = ["model-primary", "model-fast", "codex-auto-review"].map(str::to_owned);

    // Default open-source config does not bind any hardcoded model IDs.
    let default_cfg = Config::default();
    let default_entries = model_policy::resolve(&ids, &default_cfg, &Default::default()).unwrap();
    let default_doc = catalog::build_document(&default_entries, &default_cfg.catalog).unwrap();
    for model in default_doc["models"].as_array().unwrap() {
        assert!(model.get("auto_review_model_override").is_none());
    }

    // Configured preference list picks the first enabled model (hidden remains eligible).
    let mut config = Config::default();
    config.catalog.auto_review_models = vec!["codex-auto-review".into(), "model-fast".into()];
    config.models.insert(
        "model-fast".into(),
        model_policy::ModelOverride {
            visible: Some(false),
            ..Default::default()
        },
    );
    for (subset, expected) in [
        (
            vec!["model-primary", "model-fast", "codex-auto-review"],
            Some("codex-auto-review"),
        ),
        (vec!["model-primary", "model-fast"], Some("model-fast")),
        (
            vec!["model-primary", "codex-auto-review"],
            Some("codex-auto-review"),
        ),
        (vec!["model-primary"], None),
    ] {
        let subset: Vec<String> = subset.into_iter().map(str::to_owned).collect();
        let entries = model_policy::resolve(&subset, &config, &Default::default()).unwrap();
        let document = catalog::build_document(&entries, &config.catalog).unwrap();
        let models = document["models"].as_array().unwrap();
        assert_eq!(models.len(), subset.len());
        for model in models {
            match expected {
                Some(reviewer) => assert_eq!(model["auto_review_model_override"], reviewer),
                None => assert!(model.get("auto_review_model_override").is_none()),
            }
        }
        if let Some(reviewer) = expected {
            let review_model = models
                .iter()
                .find(|model| model["slug"] == reviewer)
                .unwrap();
            assert_eq!(review_model["visibility"], "hide");
            assert_eq!(review_model["supported_in_api"], true);
        }
    }

    // Disabled models are skipped.
    for (disabled, expected) in [
        (vec!["codex-auto-review"], Some("model-fast")),
        (vec!["codex-auto-review", "model-fast"], None),
    ] {
        let mut cfg = Config::default();
        cfg.catalog.auto_review_models = vec!["codex-auto-review".into(), "model-fast".into()];
        for id in &disabled {
            cfg.models.insert(
                (*id).into(),
                model_policy::ModelOverride {
                    enabled: Some(false),
                    ..Default::default()
                },
            );
        }
        let entries = model_policy::resolve(&ids, &cfg, &Default::default()).unwrap();
        let document = catalog::build_document(&entries, &cfg.catalog).unwrap();
        let models = document["models"].as_array().unwrap();
        assert_eq!(models.len(), ids.len() - disabled.len());
        for model in models {
            match expected {
                Some(reviewer) => assert_eq!(model["auto_review_model_override"], reviewer),
                None => assert!(model.get("auto_review_model_override").is_none()),
            }
        }
    }
}

#[test]
fn catalog_options_control_default_model_description_and_ultra_reasoning() {
    let mut cfg = Config::default();
    cfg.catalog.default_model = Some("model-b".into());
    cfg.catalog.append_context_to_description = false;
    cfg.catalog.append_ultra_reasoning = false;
    cfg.models.insert(
        "model-b".into(),
        model_policy::ModelOverride {
            description: Some("Primary model".into()),
            ..Default::default()
        },
    );
    cfg.validate().unwrap();

    let ids = ["model-a".into(), "model-b".into()];
    let entries = model_policy::resolve(&ids, &cfg, &Default::default()).unwrap();
    let doc = catalog::build_document(&entries, &cfg.catalog).unwrap();
    let models = doc["models"].as_array().unwrap();
    assert_eq!(models[0]["slug"], "model-b");
    assert_eq!(models[0]["priority"], 0);
    assert_eq!(models[0]["description"], "Primary model");
    assert_eq!(models[1]["description"], "");
    assert!(
        models[0]["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .iter()
            .all(|lvl| lvl["effort"] != "ultra")
    );
}

#[test]
fn model_metadata_precedence_and_sparse_overrides() {
    let db = models_dev::parse(&json!({
        "openai": {
            "models": {
                "model-a": {
                    "limit": {"context": 256000},
                    "reasoning_options": [{"type": "effort", "values": ["low", "high"]}],
                    "modalities": {"input": ["text", "image"]}
                }
            }
        }
    }))
    .unwrap();
    let mut c = Config::default();
    c.defaults.compaction = model_policy::Compaction::Percent { percent: 80.0 };
    c.models.insert(
        "model-a".into(),
        model_policy::ModelOverride {
            context_window: Some(128000),
            ..Default::default()
        },
    );
    let entries = model_policy::resolve(&["model-a".into()], &c, &db).unwrap();
    let d = catalog::build_document(&entries, &c.catalog).unwrap();
    let m = &d["models"][0];
    assert_eq!(m["context_window"], 128000);
    assert_eq!(m["auto_compact_token_limit"], 102400);
    assert_eq!(m["input_modalities"], json!(["text", "image"]));
    assert_eq!(m["description"], "128k");
    assert_eq!(
        m["supported_reasoning_levels"]
            .as_array()
            .unwrap()
            .last()
            .unwrap()["effort"],
        "ultra"
    );
}

#[test]
fn strict_configuration_and_no_url_credentials() {
    assert!(toml_edit::de::from_str::<Config>("unexpected = 1").is_err());
    assert!(codex_newapi_gateway::config::normalize_url("https://secret@example.com/v1").is_err());
    assert!(
        codex_newapi_gateway::config::normalize_url("https://example.com/v1?key=secret").is_err()
    );
    let c: Config =
        toml_edit::de::from_str(include_str!("../configs/gateway.example.toml")).unwrap();
    c.validate().unwrap();
    assert!(!c.auth.allow_bearer_fallback);
    assert!(!Config::default().auth.allow_bearer_fallback);
    assert!(c.catalog.auto_review_models.is_empty());
}

#[test]
fn source_based_reasoning_override_is_valid_before_metadata_arrives() {
    let mut config = Config::default();
    config.models.insert(
        "model-a".into(),
        model_policy::ModelOverride {
            source: Some("openai/model-a".into()),
            default_reasoning: Some("low".into()),
            ..Default::default()
        },
    );
    config.validate().unwrap();
    assert!(toml_edit::de::from_str::<Config>("[defaults]\nunknown = true").is_err());
}

#[tokio::test]
async fn websocket_tunnel_preserves_authentication_and_bidirectional_bytes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let f = fixture(Default::default()).await;
    f.mock.websocket.store(true, Ordering::Relaxed);
    let r = reqwest::Client::new()
        .get(format!("{}/responses", f.base))
        .header("X-NewAPI-Key", "key-a")
        .bearer_auth("not-the-newapi-key")
        .header("cookie", "private-session=1")
        .header("chatgpt-account-id", "private-account")
        .header("connection", "upgrade")
        .header("upgrade", "websocket")
        .header("sec-websocket-version", "13")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 101);
    assert_eq!(
        r.headers()["sec-websocket-accept"],
        "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
    );
    {
        let captured = f.mock.websocket_headers.lock().unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["authorization"], "Bearer key-a");
        for name in ["x-newapi-key", "cookie", "chatgpt-account-id"] {
            assert!(!captured[0].contains_key(name));
        }
    }
    let mut stream = r.upgrade().await.unwrap();
    let payload = br#"{"type":"response.create","model":"model-a","service_tier":"priority","input":"unchanged"}"#;
    stream.write_all(payload).await.unwrap();
    let mut reply = vec![0; payload.len()];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut reply))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply, payload);
}

#[tokio::test]
async fn strict_header_mode_rejects_bearer_only_without_contacting_upstream() {
    let f = fixture(Default::default()).await;
    let c = reqwest::Client::new();
    for (method, path) in [
        (reqwest::Method::GET, "models"),
        (reqwest::Method::GET, "responses"),
        (reqwest::Method::POST, "responses"),
        (reqwest::Method::POST, "images/generations"),
    ] {
        let response = c
            .request(method, format!("{}/{path}", f.base))
            .bearer_auth("key-a")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 401, "{path}");
    }
    assert!(f.mock.calls.lock().unwrap().is_empty());
    assert!(f.mock.websocket_headers.lock().unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_dedicated_headers_are_rejected_before_upstream() {
    let f = fixture(Default::default()).await;
    let mut headers = HeaderMap::new();
    headers.append("x-newapi-key", "key-a".parse().unwrap());
    headers.append("x-newapi-key", "key-b".parse().unwrap());
    let response = reqwest::Client::new()
        .get(format!("{}/models", f.base))
        .headers(headers)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 401);
    assert!(f.mock.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn bearer_compatibility_requires_explicit_opt_in() {
    let mut config = Config::default();
    config.auth.allow_bearer_fallback = true;
    let f = fixture(config).await;
    let response = reqwest::Client::new()
        .get(format!("{}/models", f.base))
        .bearer_auth("key-a")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let calls = f.mock.calls.lock().unwrap();
    assert_eq!(calls[0].1["authorization"], "Bearer key-a");
}

#[tokio::test]
async fn standalone_image_routes_replace_account_auth_and_preserve_payload() {
    let f = fixture(Default::default()).await;
    let c = reqwest::Client::new();
    let payload = br#"{"model":"gpt-image-2","prompt":"unchanged"}"#;
    for (route, status) in [("images/generations", 200), ("images/edits", 429)] {
        let response = c
            .post(format!("{}/{route}", f.base))
            .header("X-NewAPI-Key", "key-a")
            .bearer_auth("chatgpt-token")
            .header("chatgpt-account-id", "private-account")
            .header("cookie", "private-session=1")
            .header("content-type", "application/json")
            .body(payload.to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), status);
    }
    let calls = f.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for (_, headers, body) in calls.iter() {
        assert_eq!(headers["authorization"], "Bearer key-a");
        for name in ["x-newapi-key", "chatgpt-account-id", "cookie"] {
            assert!(!headers.contains_key(name));
        }
        assert_eq!(body, payload);
    }
}

#[tokio::test]
async fn custom_key_header_is_consumed_reflected_in_vary_and_not_forwarded() {
    let mut config = Config::default();
    config.auth.key_header = "OpenAI-Project".to_string();
    let f = fixture(config).await;
    let client = reqwest::Client::new();
    let response = client
        .get(format!("{}/models", f.base))
        .header("OpenAI-Project", "key-a")
        .bearer_auth("chatgpt-token")
        .header("chatgpt-account-id", "private-account")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["vary"], "Authorization, OpenAI-Project");
    let catalog: Value = response.json().await.unwrap();
    assert_eq!(catalog["models"][0]["slug"], "model-a");
    assert_eq!(catalog["models"][0]["use_responses_lite"], false);
    let response = client
        .post(format!("{}/responses", f.base))
        .header("OpenAI-Project", "key-a")
        .bearer_auth("chatgpt-token")
        .header("chatgpt-account-id", "private-account")
        .json(&json!({"model":"model-a","input":"unchanged"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let calls = f.mock.calls.lock().unwrap();
    assert_eq!(calls.len(), 2);
    for (_, headers, _) in calls.iter() {
        assert_eq!(headers["authorization"], "Bearer key-a");
        for name in ["openai-project", "x-newapi-key", "chatgpt-account-id"] {
            assert!(!headers.contains_key(name));
        }
    }
}
