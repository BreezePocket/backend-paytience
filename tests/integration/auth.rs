//! §1.2 authentication and §1.3 liveness over real sockets.

use std::time::Duration;

use ed25519_dalek::SigningKey;
use futures_util::{SinkExt, StreamExt};
use paytience_backend::protocol::Channel;
use serde_json::json;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

use crate::harness::*;

#[tokio::test]
async fn auth_success_sends_welcome() {
    let server = start(test_config()).await;
    let (_ws, welcome) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_a(), "inst-1", Channel::Rfq),
    )
    .await;
    assert_eq!(
        welcome,
        json!({ "type": "welcome", "channel": "ws/rfq", "mm_pubkey": key_a().pubkey, "name": "alpha", "instance_id": "inst-1" })
    );
    let (_ws2, welcome) = dial(
        &server,
        "/ws/sign",
        &HelloSpec::new(&key_a(), "inst-1", Channel::Sign),
    )
    .await;
    assert_eq!(welcome["channel"], "ws/sign");
    server.wait_eligible(1).await;
}

async fn expect_rejection(spec: HelloSpec, path: &str, code: u16) -> String {
    let server = start(test_config()).await;
    let (mut ws, reply) = dial(&server, path, &spec).await;
    assert_eq!(reply["type"], "error", "{reply}");
    let (close, _) = read_until_close(&mut ws).await;
    assert_eq!(close, Some(code), "reason: {}", reply["reason"]);
    assert!(server.app.registry.eligible().is_empty());
    reply["reason"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn not_allowlisted_closes_4003() {
    let stranger = mm_key(7);
    expect_rejection(
        HelloSpec::new(&stranger, "i", Channel::Rfq),
        "/ws/rfq",
        4003,
    )
    .await;
}

#[tokio::test]
async fn bad_signature_closes_4001() {
    let mut spec = HelloSpec::new(&key_a(), "i", Channel::Rfq);
    spec.sign_with = Some(SigningKey::from_bytes(&[42; 32]));
    let reason = expect_rejection(spec, "/ws/rfq", 4001).await;
    assert!(reason.contains("signature"), "{reason}");
}

#[tokio::test]
async fn wrong_channel_closes_4001() {
    // Claims (and signs) ws/sign on the rfq path.
    let spec = HelloSpec::new(&key_a(), "i", Channel::Sign);
    let reason = expect_rejection(spec, "/ws/rfq", 4001).await;
    assert!(reason.contains("channel"), "{reason}");
}

#[tokio::test]
async fn bad_version_closes_4001() {
    let mut spec = HelloSpec::new(&key_a(), "i", Channel::Sign);
    spec.version = json!(2);
    expect_rejection(spec, "/ws/sign", 4001).await;
}

#[tokio::test]
async fn program_or_mint_mismatch_closes_4009() {
    let mut spec = HelloSpec::new(&key_a(), "i", Channel::Rfq);
    spec.program_id = "11111111111111111111111111111111".into();
    expect_rejection(spec, "/ws/rfq", 4009).await;

    let mut spec = HelloSpec::new(&key_b(), "i", Channel::Sign);
    spec.usdc_mint = PROGRAM_ID.into();
    expect_rejection(spec, "/ws/sign", 4009).await;
}

#[tokio::test]
async fn frames_before_hello_are_ignored() {
    let server = start(test_config()).await;
    let (mut ws, _) = connect_async(server.ws_url("/ws/rfq")).await.unwrap();
    let challenge = next_json(&mut ws).await.unwrap()["challenge"]
        .as_str()
        .unwrap()
        .to_string();
    send_json(&mut ws, &json!({ "type": "rfq_response", "rfq_id": "x" })).await;
    ws.send(Message::Text("not json".into())).await.unwrap();
    send_json(
        &mut ws,
        &HelloSpec::new(&key_a(), "i", Channel::Rfq).frame(&challenge),
    )
    .await;
    assert_eq!(next_json(&mut ws).await.unwrap()["type"], "welcome");
}

#[tokio::test]
async fn hello_timeout_closes_4001() {
    let mut cfg = test_config();
    cfg.hello_timeout = Duration::from_millis(200);
    let server = start(cfg).await;
    let (mut ws, _) = connect_async(server.ws_url("/ws/sign")).await.unwrap();
    let (close, frames) = read_until_close(&mut ws).await;
    assert_eq!(close, Some(4001));
    assert_eq!(frames[0]["type"], "challenge");
    assert_eq!(frames.last().unwrap()["type"], "error");
}

#[tokio::test]
async fn same_identity_replaces_with_4000() {
    let server = start(test_config()).await;
    let spec = HelloSpec::new(&key_a(), "inst-1", Channel::Rfq);
    let (mut old, w1) = dial(&server, "/ws/rfq", &spec).await;
    assert_eq!(w1["type"], "welcome");
    let (_new, w2) = dial(&server, "/ws/rfq", &spec).await;
    assert_eq!(w2["type"], "welcome");
    let (close, _) = read_until_close(&mut old).await;
    assert_eq!(close, Some(4000));

    // The replacement is the live connection, and other instances coexist.
    let (_sign, _) = dial(
        &server,
        "/ws/sign",
        &HelloSpec::new(&key_a(), "inst-1", Channel::Sign),
    )
    .await;
    let (_rfq2, _) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_a(), "inst-2", Channel::Rfq),
    )
    .await;
    let (_sign2, _) = dial(
        &server,
        "/ws/sign",
        &HelloSpec::new(&key_a(), "inst-2", Channel::Sign),
    )
    .await;
    server.wait_eligible(2).await;
}

#[tokio::test]
async fn eligibility_requires_both_channels() {
    let server = start(test_config()).await;
    let (_rfq, _) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_a(), "i", Channel::Rfq),
    )
    .await;
    let (status, body) = server.rfq("web-1", "1").await;
    assert_eq!(status, 503);
    assert_eq!(
        body,
        json!({ "type": "rfq_decline", "rfq_id": "web-1", "reason": "no market maker is online" })
    );

    let (sign, _) = dial(
        &server,
        "/ws/sign",
        &HelloSpec::new(&key_a(), "i", Channel::Sign),
    )
    .await;
    server.wait_eligible(1).await;
    drop(sign);
    server.wait_eligible(0).await;
}

#[tokio::test]
async fn server_pings_and_closes_silent_connections() {
    let mut cfg = test_config();
    cfg.ping_interval = Duration::from_millis(100);
    cfg.idle_timeout = Duration::from_millis(400);
    let server = start(cfg).await;

    // A client that keeps reading answers pings (tungstenite pongs automatically).
    let (mut live, _) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_a(), "live", Channel::Rfq),
    )
    .await;
    let mut pings = 0;
    let until = tokio::time::Instant::now() + Duration::from_millis(900);
    while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(until, live.next()).await {
        match msg {
            Message::Ping(_) => pings += 1,
            Message::Close(_) => panic!("live connection was closed"),
            _ => {}
        }
    }
    assert!(pings >= 3, "got {pings} pings");

    // One that stops reading sends no pongs and is closed.
    let (mut silent, _) = dial(
        &server,
        "/ws/sign",
        &HelloSpec::new(&key_a(), "silent", Channel::Sign),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(700)).await;
    // The server drops the silent connection...
    let silent_gone = || {
        server.app.registry.with_instances(|i| {
            i.filter(|x| x.instance_id == "silent")
                .all(|x| x.sign.is_none())
        })
    };
    wait_until(silent_gone, "idle close of the silent connection").await;
    // ...with CLOSE_IDLE. Under load the client can see a reset instead: when it
    // resumes reading, tungstenite flushes its queued pongs into the closed socket,
    // which may fail before the close frame is read.
    let (close, _) = read_until_close(&mut silent).await;
    assert!(
        close.is_none() || close == Some(paytience_backend::ws::CLOSE_IDLE),
        "unexpected close code {close:?}"
    );
}

#[tokio::test]
async fn shutdown_closes_mm_connections_with_1001() {
    let mut server = start(test_config()).await;
    let (mut ws, _) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_a(), "i", Channel::Rfq),
    )
    .await;
    server.shutdown();
    let (close, _) = read_until_close(&mut ws).await;
    assert_eq!(close, Some(1001));
}

#[tokio::test]
async fn server_answers_client_pings() {
    let server = start(test_config()).await;
    let (mut ws, _) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_a(), "i", Channel::Rfq),
    )
    .await;
    ws.send(Message::Ping(b"desk".to_vec().into()))
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, ws.next()).await {
            Ok(Some(Ok(Message::Pong(p)))) => {
                assert_eq!(&p[..], b"desk");
                break;
            }
            Ok(Some(Ok(_))) => {}
            other => panic!("no pong: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// §1.2 limits on unauthenticated sockets and message sizes

/// Reads the challenge on a raw socket and authenticates it as `spec`.
async fn authenticate(ws: &mut Ws, spec: &HelloSpec) {
    let challenge = next_json(ws).await.unwrap();
    let c = challenge["challenge"].as_str().unwrap().to_string();
    send_json(ws, &spec.frame(&c)).await;
    assert_eq!(next_json(ws).await.unwrap()["type"], "welcome");
}

#[tokio::test]
async fn oversized_frame_before_hello_closes_4001_unparsed() {
    let server = start(test_config()).await;
    let big = "x".repeat(paytience_backend::ws::MAX_PREAUTH_FRAME_BYTES + 1);
    for msg in [
        Message::Text(format!("[{big}]").into()),
        Message::Binary(big.clone().into_bytes().into()),
    ] {
        let (mut ws, _) = connect_async(server.ws_url("/ws/rfq")).await.unwrap();
        ws.send(msg).await.unwrap();
        let (close, frames) = read_until_close(&mut ws).await;
        assert_eq!(close, Some(4001));
        assert_eq!(frames[0]["type"], "challenge");
        assert_eq!(
            frames.last().unwrap(),
            &json!({ "type": "error", "reason": "hello too large" })
        );
    }
    // A frame at the limit is still just ignored, and the server is fine.
    let (mut ws, _) = connect_async(server.ws_url("/ws/sign")).await.unwrap();
    let padded = json!({ "type": "noise", "pad": "" }).to_string();
    let pad = "y".repeat(paytience_backend::ws::MAX_PREAUTH_FRAME_BYTES - padded.len());
    send_json(&mut ws, &json!({ "type": "noise", "pad": pad })).await;
    authenticate(&mut ws, &HelloSpec::new(&key_a(), "i", Channel::Sign)).await;
    server.wait_preauth(0).await;
    assert_eq!(server.get("/health").await.0, 200);
}

#[tokio::test]
async fn unauthenticated_sockets_are_capped_globally() {
    let mut cfg = test_config();
    cfg.hello_timeout = Duration::from_secs(10);
    cfg.ws_max_unauthenticated = 2;
    let server = start(cfg).await;
    let mut first = upgrade(&server, "/ws/rfq", Some("203.0.113.1"))
        .await
        .unwrap();
    let second = upgrade(&server, "/ws/sign", Some("203.0.113.2"))
        .await
        .unwrap();
    server.wait_preauth(2).await;
    assert_eq!(
        upgrade(&server, "/ws/rfq", Some("203.0.113.3")).await.err(),
        Some(503)
    );

    // Authenticating frees the slot...
    authenticate(&mut first, &HelloSpec::new(&key_a(), "i", Channel::Rfq)).await;
    server.wait_preauth(1).await;
    let _third = upgrade(&server, "/ws/rfq", Some("203.0.113.3"))
        .await
        .unwrap();
    server.wait_preauth(2).await;
    assert_eq!(
        upgrade(&server, "/ws/sign", Some("203.0.113.4"))
            .await
            .err(),
        Some(503)
    );
    // ...and so does closing.
    drop(second);
    server.wait_preauth(1).await;
    assert!(
        upgrade(&server, "/ws/sign", Some("203.0.113.4"))
            .await
            .is_ok()
    );
    // Authenticated sockets never counted.
    assert_eq!(server.app.registry.with_instances(|i| i.count()), 1);
}

#[tokio::test]
async fn unauthenticated_sockets_are_capped_per_client_ip() {
    let mut cfg = test_config();
    cfg.hello_timeout = Duration::from_secs(10);
    cfg.ws_max_unauthenticated_per_ip = 2;
    let server = start(cfg).await;
    let a = upgrade(&server, "/ws/rfq", None).await.unwrap();
    let _b = upgrade(&server, "/ws/sign", None).await.unwrap();
    assert_eq!(upgrade(&server, "/ws/rfq", None).await.err(), Some(503));
    // Another client behind the tunnel has its own slots; a non-IP header does not.
    let _c = upgrade(&server, "/ws/rfq", Some("203.0.113.9"))
        .await
        .unwrap();
    assert_eq!(
        upgrade(&server, "/ws/rfq", Some("not-an-ip")).await.err(),
        Some(503)
    );
    assert_eq!(server.app.preauth.open_for("127.0.0.1"), 2);

    drop(a);
    wait_until(
        || server.app.preauth.open_for("127.0.0.1") == 1,
        "slot release",
    )
    .await;
    assert!(upgrade(&server, "/ws/rfq", None).await.is_ok());
}

#[tokio::test]
async fn hello_timeout_frees_the_slot() {
    let mut cfg = test_config();
    cfg.hello_timeout = Duration::from_millis(200);
    cfg.ws_max_unauthenticated = 1;
    let server = start(cfg).await;
    let mut ws = upgrade(&server, "/ws/rfq", None).await.unwrap();
    assert_eq!(upgrade(&server, "/ws/rfq", None).await.err(), Some(503));
    assert_eq!(read_until_close(&mut ws).await.0, Some(4001));
    server.wait_preauth(0).await;
    assert!(upgrade(&server, "/ws/rfq", None).await.is_ok());
}

#[tokio::test]
async fn message_over_the_cap_disconnects_an_authenticated_mm() {
    let mut cfg = test_config();
    cfg.ws_max_message_bytes = 64 * 1024;
    cfg.market_timeout = Duration::from_secs(10);
    let server = start(cfg).await;
    let pad = "z".repeat(100 * 1024);
    let _mm = FakeMm::builder(&key_a(), "i1")
        .on_rfq(move |req| Action::Reply(market_ok(req, json!({ "pad": pad }))))
        .connect(&server)
        .await;
    server.wait_eligible(1).await;

    // The pending board request fails at once, not after the 10 s market timeout.
    let started = std::time::Instant::now();
    let (status, body) = server.get("/board").await;
    assert_eq!(
        (status, body),
        (503, json!({ "error": "market maker disconnected" }))
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // Its rfq connection is gone, so the instance is no longer eligible.
    server.wait_eligible(0).await;

    // The server is fine: another MM connects and is served.
    let _other = FakeMm::builder(&key_b(), "i1").connect(&server).await;
    server.wait_eligible(1).await;
    assert_eq!(server.get("/health").await.0, 200);
}

#[tokio::test]
async fn a_desk_on_this_machine_skips_the_public_caps() {
    // With the exemption on (the production default), a loopback client with no
    // CF-Connecting-IP (the house desk on the VM) takes no unauthenticated slot and no
    // upgrade budget, so a flood through the tunnel cannot lock it out.
    let mut cfg = test_config();
    cfg.hello_timeout = Duration::from_secs(10);
    cfg.ws_max_unauthenticated = 1;
    cfg.ws_max_unauthenticated_per_ip = 1;
    cfg.rate_limits.ws = 1;
    cfg.rate_limits.exempt_local_ws = true;
    let server = start(cfg).await;

    // A public client fills the only slot and spends its upgrade budget.
    let _public = upgrade(&server, "/ws/rfq", Some("203.0.113.1"))
        .await
        .unwrap();
    server.wait_preauth(1).await;
    assert_eq!(
        upgrade(&server, "/ws/rfq", Some("203.0.113.2")).await.err(),
        Some(503),
        "the public pool is full"
    );
    assert_eq!(
        upgrade(&server, "/ws/rfq", Some("203.0.113.1")).await.err(),
        Some(429),
        "and that client's upgrade budget is spent"
    );

    // Local sockets still get in, beyond both caps and the budget, and hold no slot.
    let _l1 = upgrade(&server, "/ws/rfq", None).await.unwrap();
    let _l2 = upgrade(&server, "/ws/sign", None).await.unwrap();
    assert_eq!(server.app.preauth.open(), 1);
    let _desk = FakeMm::builder(&key_a(), "house").connect(&server).await;
    server.wait_eligible(1).await;
}
