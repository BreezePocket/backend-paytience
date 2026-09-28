//! §2 HTTP conventions: CORS, body limit, untyped errors, 404 fallback.

use serde_json::json;

use crate::harness::*;

fn assert_cors(r: &reqwest::Response) {
    let h = r.headers();
    assert_eq!(h["access-control-allow-origin"], "*");
    assert_eq!(
        h["access-control-allow-headers"],
        "content-type, authorization"
    );
    assert_eq!(h["access-control-allow-methods"], "GET, POST, OPTIONS");
}

#[tokio::test]
async fn options_is_204_with_cors() {
    let server = start(test_config()).await;
    for path in ["/rfq", "/sign", "/health", "/anything"] {
        let r = server
            .http
            .request(reqwest::Method::OPTIONS, server.url(path))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 204, "{path}");
        assert_cors(&r);
    }
}

#[tokio::test]
async fn every_response_carries_cors() {
    let server = start(test_config()).await;
    let r = server.http.get(server.url("/health")).send().await.unwrap();
    assert_cors(&r);
    let r = server.http.get(server.url("/nope")).send().await.unwrap();
    assert_cors(&r);
    let r = server
        .http
        .post(server.url("/rfq"))
        .body("{")
        .send()
        .await
        .unwrap();
    assert_cors(&r);
}

#[tokio::test]
async fn unknown_paths_and_methods_are_404() {
    let server = start(test_config()).await;
    assert_eq!(
        server.get("/nope").await,
        (404, json!({ "error": "not found" }))
    );
    assert_eq!(
        server.post("/health", &json!({})).await,
        (404, json!({ "error": "not found" }))
    );
    let (status, body) = server.get("/rfq").await;
    assert_eq!((status, body), (404, json!({ "error": "not found" })));
}

#[tokio::test]
async fn bad_bodies_are_untyped_errors() {
    let server = start(test_config()).await;
    let r = server
        .http
        .post(server.url("/rfq"))
        .header("content-type", "application/json")
        .body("{nope")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap(),
        json!({ "error": "body is not valid JSON" })
    );

    let big = format!("{{\"pad\":\"{}\"}}", "x".repeat(70 * 1024));
    let r = server
        .http
        .post(server.url("/sign"))
        .header("content-type", "application/json")
        .body(big)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap(),
        json!({ "error": "body too large" })
    );

    // Just under the limit is accepted (and then fails validation as a typed message).
    let ok = format!(
        "{{\"request_id\":\"r\",\"pad\":\"{}\"}}",
        "x".repeat(60 * 1024)
    );
    let r = server
        .http
        .post(server.url("/sign"))
        .body(ok)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    assert_eq!(
        r.json::<serde_json::Value>().await.unwrap()["type"],
        "sign_rejection"
    );
}
