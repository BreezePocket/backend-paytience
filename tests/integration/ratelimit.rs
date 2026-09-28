//! §2.10 per-IP rate limits, with small limits so the tests stay fast.

use std::time::Duration;

use paytience_backend::RateLimits;
use serde_json::{Value, json};

use crate::harness::*;

async fn limited(window: Duration) -> TestServer {
    let mut cfg = test_config();
    cfg.rate_limits = RateLimits {
        window,
        rfq: 2,
        sign: 2,
        faucet: 1,
        get: 3,
        ws: 4,
        exempt_local_ws: false,
    };
    start(cfg).await
}

async fn send(
    server: &TestServer,
    method: reqwest::Method,
    path: &str,
    ip: Option<&str>,
) -> (u16, Value, reqwest::header::HeaderMap) {
    let mut req = server.http.request(method.clone(), server.url(path));
    if let Some(ip) = ip {
        req = req.header("CF-Connecting-IP", ip);
    }
    if method == reqwest::Method::POST {
        req = req.json(&json!({}));
    }
    let r = req.send().await.unwrap();
    let status = r.status().as_u16();
    let headers = r.headers().clone();
    let body = r.json().await.unwrap_or(Value::Null);
    (status, body, headers)
}

const MINUTE: Duration = Duration::from_secs(60);

#[tokio::test]
async fn each_class_has_its_own_limit() {
    let server = limited(MINUTE).await;
    let post = reqwest::Method::POST;
    let get = reqwest::Method::GET;
    let limited = json!({ "error": "rate limited" });
    for (path, allowed, usual) in [("/rfq", 2, 400), ("/sign", 2, 400), ("/faucet", 1, 404)] {
        for _ in 0..allowed {
            assert_eq!(
                send(&server, post.clone(), path, None).await.0,
                usual,
                "{path}"
            );
        }
        let (status, body, headers) = send(&server, post.clone(), path, None).await;
        assert_eq!((status, &body), (429, &limited), "{path}");
        assert_eq!(headers["access-control-allow-origin"], "*");
    }
    // POSTs spent nothing of the GET budget; every GET path shares it.
    for path in ["/health", "/faucet", "/nope"] {
        assert_ne!(
            send(&server, get.clone(), path, None).await.0,
            429,
            "{path}"
        );
    }
    let (status, body, _) = send(&server, get.clone(), "/assets", None).await;
    assert_eq!((status, body), (429, limited));
}

#[tokio::test]
async fn options_pass_and_websocket_upgrades_have_their_own_budget() {
    let server = limited(MINUTE).await;
    for _ in 0..3 {
        send(&server, reqwest::Method::GET, "/health", None).await;
    }
    assert_eq!(
        send(&server, reqwest::Method::GET, "/health", None).await.0,
        429
    );
    for _ in 0..5 {
        assert_eq!(
            send(&server, reqwest::Method::OPTIONS, "/rfq", None)
                .await
                .0,
            204
        );
    }
    // Four GET upgrades from the same IP, whose GET budget is spent: the WS budget is
    // separate.
    let _a = FakeMm::builder(&key_a(), "i1").connect(&server).await;
    let _b = FakeMm::builder(&key_b(), "i1").connect(&server).await;
    server.wait_eligible(2).await;
    // The fifth upgrade in the window is refused before upgrading.
    assert_eq!(upgrade(&server, "/ws/rfq", None).await.err(), Some(429));
    assert_eq!(upgrade(&server, "/ws/sign", None).await.err(), Some(429));
    assert!(
        upgrade(&server, "/ws/sign", Some("203.0.113.5"))
            .await
            .is_ok()
    );
    // POSTs to paths without a class are not counted.
    for _ in 0..5 {
        assert_eq!(
            send(&server, reqwest::Method::POST, "/nope", None).await.0,
            404
        );
    }
}

#[tokio::test]
async fn cf_connecting_ip_takes_precedence_over_the_peer() {
    let server = limited(MINUTE).await;
    let get = reqwest::Method::GET;
    for _ in 0..3 {
        assert_eq!(
            send(&server, get.clone(), "/health", Some("203.0.113.1"))
                .await
                .0,
            200
        );
    }
    assert_eq!(
        send(&server, get.clone(), "/health", Some("203.0.113.1"))
            .await
            .0,
        429
    );
    // Another client behind the tunnel, and the bare peer, have their own budgets.
    assert_eq!(
        send(&server, get.clone(), "/health", Some("203.0.113.2"))
            .await
            .0,
        200
    );
    assert_eq!(
        send(&server, get.clone(), "/health", Some("2001:db8::7"))
            .await
            .0,
        200
    );
    for _ in 0..3 {
        assert_eq!(send(&server, get.clone(), "/health", None).await.0, 200);
    }
    assert_eq!(send(&server, get.clone(), "/health", None).await.0, 429);
    // A header that is not an IP falls back to the (exhausted) peer.
    assert_eq!(
        send(&server, get.clone(), "/health", Some("garbage"))
            .await
            .0,
        429
    );
    assert_eq!(
        send(&server, get.clone(), "/health", Some("203.0.113.3"))
            .await
            .0,
        200
    );
}

#[tokio::test]
async fn a_new_window_resets_the_count() {
    let window = Duration::from_millis(300);
    let server = limited(window).await;
    // Spend the budget until refused (a window boundary mid-burst only adds tries).
    let mut refused = false;
    for _ in 0..10 {
        if send(&server, reqwest::Method::POST, "/faucet", None)
            .await
            .0
            == 429
        {
            refused = true;
            break;
        }
    }
    assert!(refused);
    tokio::time::sleep(window + Duration::from_millis(50)).await;
    assert_eq!(
        send(&server, reqwest::Method::POST, "/faucet", None)
            .await
            .0,
        404
    );
}
