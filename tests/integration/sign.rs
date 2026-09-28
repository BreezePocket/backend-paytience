//! §2.7 co-sign routing and broadcast through `POST /sign`.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use paytience_backend::protocol::Channel;
use serde_json::{Value, json};

use crate::harness::*;

const COSIGNED: &str = "Y29zaWduZWQ="; // stand-in for the desk's co-signed tx

/// A desk that co-signs everything, answering from instance `tag`.
async fn signing_mm(
    server: &TestServer,
    key: &MmKey,
    instance: &str,
    yield_amount: &'static str,
) -> FakeMm {
    let k = key.clone();
    let tag = instance.to_string();
    FakeMm::builder(key, instance)
        .on_rfq(move |req| Action::Reply(quote(req, &k, yield_amount, &tag)))
        .on_sign(|req| Action::Reply(sign_ok(req, COSIGNED, None)))
        .connect(server)
        .await
}

#[tokio::test]
async fn routes_to_the_winning_instance() {
    let server = start(test_config()).await;
    let a = key_a();
    // i1 authenticates first and wins; i2 is the most recently authenticated.
    let i1 = signing_mm(&server, &a, "i1", "900").await;
    let i2 = signing_mm(&server, &a, "i2", "100").await;
    server.wait_eligible(2).await;

    let (tx, nonce) = tx_case("open_position_sol");
    let (status, quote) = server.rfq("web-q", &nonce).await;
    assert_eq!((status, quote["quote_id"].as_str().unwrap()), (200, "i1"));

    let (status, body) = server.sign("web-sign-1", &tx).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({ "type": "sign_response", "request_id": "web-sign-1", "tx_base64": COSIGNED })
    );
    let got = i1.received(Channel::Sign, "sign_request", None);
    assert_eq!(got.len(), 1);
    assert!(got[0]["request_id"].as_str().unwrap().starts_with("sig-"));
    assert_eq!(got[0]["tx_base64"], tx.as_str());
    assert!(i2.received(Channel::Sign, "sign_request", None).is_empty());
}

#[tokio::test]
async fn without_a_winner_the_newest_connection_signs() {
    let server = start(test_config()).await;
    let a = key_a();
    let i1 = signing_mm(&server, &a, "i1", "1").await;
    let i2 = signing_mm(&server, &a, "i2", "1").await;
    server.wait_eligible(2).await;
    let (tx, _) = tx_case("compute_budget_prefixed");
    let (status, _) = server.sign("web-sign", &tx).await;
    assert_eq!(status, 200);
    assert_eq!(i2.received(Channel::Sign, "sign_request", None).len(), 1);
    assert!(i1.received(Channel::Sign, "sign_request", None).is_empty());
}

#[tokio::test]
async fn falls_back_to_another_connection_of_the_fee_payer() {
    let server = start(test_config()).await;
    let a = key_a();
    let mut i1 = signing_mm(&server, &a, "i1", "900").await;
    let i2 = signing_mm(&server, &a, "i2", "100").await;
    server.wait_eligible(2).await;
    let (tx, nonce) = tx_case("open_asset_position");
    let (_, quote) = server.rfq("web-q", &nonce).await;
    assert_eq!(quote["quote_id"], "i1");

    i1.disconnect_sign();
    server.wait_eligible(1).await;
    wait_until(
        || server.app.registry.sign_conn(&a.pubkey, "i1").is_none(),
        "i1 sign gone",
    )
    .await;

    let (status, _) = server.sign("web-sign-2", &tx).await;
    assert_eq!(status, 200);
    assert_eq!(i2.received(Channel::Sign, "sign_request", None).len(), 1);
}

#[tokio::test]
async fn mm_rejection_is_422_with_its_reason() {
    let server = start(test_config()).await;
    let _m = FakeMm::builder(&key_a(), "i1")
        .on_sign(|req| Action::Reply(json!({ "type": "sign_rejection", "request_id": req["request_id"], "reason": "quote expired" })))
        .connect(&server)
        .await;
    server.wait_eligible(1).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-r", &tx).await;
    assert_eq!(status, 422);
    assert_eq!(
        body,
        json!({ "type": "sign_rejection", "request_id": "web-sign-r", "reason": "quote expired" })
    );
    assert_eq!(server.app.stats.snapshot()["sign_rejections"], 1);
}

#[tokio::test]
async fn silence_is_504() {
    let mut cfg = test_config();
    cfg.sign_timeout = Duration::from_millis(300);
    let server = start(cfg).await;
    let _m = FakeMm::builder(&key_a(), "i1").connect(&server).await;
    server.wait_eligible(1).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-t", &tx).await;
    assert_eq!(status, 504);
    assert_eq!(body["reason"], "market maker did not respond in time");
    assert_eq!(body["request_id"], "web-sign-t");
    assert_eq!(server.app.stats.snapshot()["sign_timeouts"], 1);
}

#[tokio::test]
async fn disconnect_fails_the_sign_fast() {
    let mut cfg = test_config();
    cfg.sign_timeout = Duration::from_secs(10);
    let server = start(cfg).await;
    let _m = FakeMm::builder(&key_a(), "i1")
        .on_sign(|_| Action::Disconnect)
        .connect(&server)
        .await;
    server.wait_eligible(1).await;
    let (tx, _) = tx_case("open_position_sol");
    let started = Instant::now();
    let (status, body) = server.sign("web-sign-dc", &tx).await;
    assert_eq!(status, 504);
    assert_eq!(body["reason"], "market maker disconnected");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn unknown_fee_payer_is_422() {
    let server = start(test_config()).await;
    let _m = signing_mm(&server, &key_a(), "i1", "1").await;
    server.wait_eligible(1).await;
    // Fee payer is MM B, which is not connected.
    let (tx, _) = tx_case("open_position_mm_b");
    let (status, body) = server.sign("web-sign-u", &tx).await;
    assert_eq!(status, 422);
    let short: String = key_b().pubkey.chars().take(8).collect();
    assert_eq!(
        body["reason"],
        format!("market maker {short} is not connected")
    );
}

#[tokio::test]
async fn invalid_transactions_are_400() {
    let server = start(test_config()).await;
    let (v0, _) = tx_case("versioned_v0");
    let (status, body) = server.sign("web-sign-v", &v0).await;
    assert_eq!(status, 400);
    assert_eq!(
        body,
        json!({ "type": "sign_rejection", "request_id": "web-sign-v", "reason": "invalid transaction: versioned transactions are not supported" })
    );
    let (transfer, _) = tx_case("transfer_only");
    let (status, body) = server.sign("web-sign-v", &transfer).await;
    assert_eq!(status, 400);
    assert!(
        body["reason"]
            .as_str()
            .unwrap()
            .starts_with("invalid transaction: ")
    );
    let (status, _) = server.post("/sign", &json!({ "request_id": "x" })).await;
    assert_eq!(status, 400);
}

// ---------------------------------------------------------------------------
// Broadcast against a local mock JSON-RPC server

#[derive(Clone)]
enum RpcMode {
    Ok(&'static str),
    Error(&'static str),
    /// HTTP 429 with a JSON-RPC error body, as a throttling node answers.
    Throttled,
    NotJson,
}

#[derive(Clone)]
struct Mock {
    mode: RpcMode,
    calls: Arc<Mutex<Vec<Value>>>,
}

async fn rpc_handler(State(mock): State<Mock>, body: axum::Json<Value>) -> Response {
    mock.calls.lock().unwrap().push(body.0.clone());
    let id = body.0["id"].clone();
    match mock.mode {
        RpcMode::Ok(sig) => {
            axum::Json(json!({ "jsonrpc": "2.0", "id": id, "result": sig })).into_response()
        }
        RpcMode::Error(msg) => axum::Json(
            json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32002, "message": msg } }),
        )
        .into_response(),
        RpcMode::Throttled => (
            StatusCode::TOO_MANY_REQUESTS,
            axum::Json(
                json!({ "jsonrpc": "2.0", "id": id, "error": { "code": 429, "message": "Too many requests for a specific RPC call" } }),
            ),
        )
            .into_response(),
        RpcMode::NotJson => (StatusCode::BAD_GATEWAY, "upstream exploded").into_response(),
    }
}

async fn mock_rpc(mode: RpcMode) -> (String, Arc<Mutex<Vec<Value>>>) {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/", post(rpc_handler))
        .with_state(Mock {
            mode,
            calls: calls.clone(),
        });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/"), calls)
}

async fn broadcast_server(
    rpc_url: &str,
    desk_signature: Option<&'static str>,
) -> (TestServer, FakeMm) {
    let mut cfg = test_config();
    cfg.broadcast = true;
    cfg.solana_rpc = rpc_url.to_string();
    let server = start(cfg).await;
    let mm = FakeMm::builder(&key_a(), "i1")
        .on_sign(move |req| Action::Reply(sign_ok(req, COSIGNED, desk_signature)))
        .connect(&server)
        .await;
    server.wait_eligible(1).await;
    (server, mm)
}

#[tokio::test]
async fn broadcast_success_returns_the_signature() {
    let (url, calls) = mock_rpc(RpcMode::Ok("5igNaTuRe")).await;
    let (server, _mm) = broadcast_server(&url, None).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-b", &tx).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({ "type": "sign_response", "request_id": "web-sign-b", "tx_base64": COSIGNED, "signature": "5igNaTuRe" })
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["method"], "sendTransaction");
    assert_eq!(
        calls[0]["params"],
        json!([COSIGNED, { "encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed" }])
    );
    assert_eq!(server.app.stats.snapshot()["broadcasts"], 1);
}

#[tokio::test]
async fn rpc_error_is_422_broadcast_failed() {
    let (url, _) = mock_rpc(RpcMode::Error(
        "Transaction simulation failed: Blockhash not found",
    ))
    .await;
    let (server, _mm) = broadcast_server(&url, None).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-e", &tx).await;
    assert_eq!(status, 422);
    assert_eq!(
        body,
        json!({ "type": "sign_rejection", "request_id": "web-sign-e", "reason": "broadcast failed: Transaction simulation failed: Blockhash not found" })
    );
}

#[tokio::test]
async fn rpc_transport_failure_returns_unsent_tx() {
    // Bind then drop: nothing listens on this port any more.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let (server, _mm) = broadcast_server(&format!("http://{dead}/"), None).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-d", &tx).await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "type": "sign_response", "request_id": "web-sign-d", "tx_base64": COSIGNED })
    );
}

#[tokio::test]
async fn rpc_non_json_returns_unsent_tx() {
    let (url, calls) = mock_rpc(RpcMode::NotJson).await;
    let (server, _mm) = broadcast_server(&url, None).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-j", &tx).await;
    assert_eq!(status, 200);
    assert!(body.get("signature").is_none());
    assert_eq!(calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn rpc_throttling_returns_unsent_tx() {
    // A 429 is not the node rejecting the transaction: the dashboard broadcasts it.
    let (url, calls) = mock_rpc(RpcMode::Throttled).await;
    let (server, _mm) = broadcast_server(&url, None).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-429", &tx).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({ "type": "sign_response", "request_id": "web-sign-429", "tx_base64": COSIGNED })
    );
    assert_eq!(calls.lock().unwrap().len(), 1);
    assert_eq!(server.app.stats.snapshot()["sign_rejections"], 0);
}

#[tokio::test]
async fn desk_signature_skips_the_broadcast() {
    let (url, calls) = mock_rpc(RpcMode::Ok("unused")).await;
    let (server, _mm) = broadcast_server(&url, Some("DeskSig")).await;
    let (tx, _) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-s", &tx).await;
    assert_eq!(status, 200);
    assert_eq!(body["signature"], "DeskSig");
    assert!(calls.lock().unwrap().is_empty());
}
