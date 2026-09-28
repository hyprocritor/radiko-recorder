use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
};
use radiko_recorder::api::Radiko;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct TestState {
    base: String,
    auth_calls: Arc<AtomicUsize>,
}

#[tokio::test]
async fn authentication_retry_endpoint_fallback_and_cache() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let router = Router::new()
        .route("/apps/js/playerCommon.js", get(|| async { "new RadikoJSPlayer(audio, 'pc_html5', 'bcd151073c03b352e1ef2fd66c32209da9ca0afa', {})" }))
        .route("/v2/api/auth1", get(|State(state): State<TestState>, headers: HeaderMap| async move {
            assert_eq!(headers["x-radiko-app"], "pc_html5");
            assert_eq!(headers["x-radiko-app-version"], "0.0.1");
            assert_eq!(headers["x-radiko-user"], "dummy_user");
            assert_eq!(headers["x-radiko-device"], "pc");
            let n = state.auth_calls.fetch_add(1, Ordering::SeqCst);
            ([("x-radiko-authtoken", if n == 0 { "expired" } else { "fresh" }), ("x-radiko-keyoffset", "9"), ("x-radiko-keylength", "16")], "")
        }))
        .route("/v2/api/auth2", get(|headers: HeaderMap| async move {
            assert_eq!(headers["x-radiko-partialkey"], "YzAzYjM1MmUxZWYyZmQ2Ng==");
            assert_eq!(headers["x-radiko-connection"], "wifi");
            assert!(!headers.contains_key("x-radiko-session"));
            "JP14,神奈川県,kanagawa Japan\r\n"
        }))
        .route("/v3/station/stream/pc_html5/JORF.xml", get(|State(state): State<TestState>| async move {
            format!("<urls><url timefree=\"0\" areafree=\"0\"><playlist_create_url>{0}/unavailable</playlist_create_url></url><url timefree=\"0\" areafree=\"0\"><playlist_create_url>{0}/master</playlist_create_url></url></urls>", state.base)
        }))
        .route("/unavailable", get(|| async { StatusCode::SERVICE_UNAVAILABLE }))
        .route("/master", get(|headers: HeaderMap| async move {
            assert_eq!(headers["x-radiko-areaid"], "JP14");
            if headers["x-radiko-authtoken"] != "fresh" { return StatusCode::FORBIDDEN.into_response(); }
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=52973\n/media?session=synthetic-test-session\n".into_response()
        }))
        .route("/program/v3/weekly/JORF.xml", get(|| async { include_str!("fixtures/schedule.xml") }))
        .with_state(TestState { base: base.clone(), auth_calls: calls.clone() });
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let api = Radiko::with_endpoints(&base, &base).unwrap();
    assert_eq!(api.schedule("JORF").await.unwrap().len(), 2);
    let expected = format!("{base}/media?session=synthetic-test-session");
    assert_eq!(api.live_url("JORF").await.unwrap(), expected);
    assert_eq!(api.live_url("JORF").await.unwrap(), expected);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "re-authenticate once, then reuse the cache"
    );
    api.invalidate_auth().await;
    assert_eq!(api.live_url("JORF").await.unwrap(), expected);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "invalidate cached authentication on reconnect"
    );
    server.abort();
}

#[tokio::test]
async fn out_of_area_is_a_permanent_error() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = Router::new()
        .route(
            "/apps/js/playerCommon.js",
            get(|| async { "unavailable app key: exercise public fallback" }),
        )
        .route(
            "/v2/api/auth1",
            get(|| async {
                (
                    [
                        ("x-radiko-authtoken", "synthetic"),
                        ("x-radiko-keyoffset", "9"),
                        ("x-radiko-keylength", "16"),
                    ],
                    "",
                )
            }),
        )
        .route("/v2/api/auth2", get(|| async { "OUT" }));
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let error = Radiko::with_endpoints(&base, &base)
        .unwrap()
        .live_url("JORF")
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<radiko_recorder::api::PermanentError>()
            .is_some()
    );
    server.abort();
}
