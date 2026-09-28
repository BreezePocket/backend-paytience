//! §2.6 quote auction through `POST /rfq`.

use std::time::{Duration, Instant};

use paytience_backend::protocol::Channel;
use serde_json::json;

use crate::harness::*;

#[tokio::test]
async fn best_of_two_wins_and_echoes_client_rfq_id() {
    let server = start(test_config()).await;
    let a = key_a();
    let ka = a.clone();
    let mm_a = FakeMm::builder(&a, "a1")
        .on_rfq(move |req| Action::Reply(quote(req, &ka, "1000", "qa")))
        .connect(&server)
        .await;
    let b = key_b();
    let kb = b.clone();
    let mm_b = FakeMm::builder(&b, "b1")
        .on_rfq(move |req| {
            Action::ReplyAfter(Duration::from_millis(30), quote(req, &kb, "2000", "qb"))
        })
        .connect(&server)
        .await;
    server.wait_eligible(2).await;

    let (status, body) = server.rfq("web-123", "42").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "rfq_response");
    assert_eq!(body["rfq_id"], "web-123");
    assert_eq!(body["quote_id"], "qb");
    assert_eq!(body["yield_amount"], "2000");
    assert_eq!(body["mm_pubkey"], b.pubkey);
    assert_eq!(body["mm_name"], "bravo");
    assert_eq!(body["quotes_received"], 2);
    assert_eq!(body["mms_asked"], 2);
    assert_eq!(
        body["valid_until"], 1759000030000u64,
        "passed through unchanged"
    );
    assert_eq!(body["implied_vol"], 0.71);

    // What the MMs saw: the backend's own id, no asset (SOL), no token, strings for u64s.
    let sent_a = &mm_a.received(Channel::Rfq, "rfq_request", None)[0];
    let sent_b = &mm_b.received(Channel::Rfq, "rfq_request", None)[0];
    assert!(sent_a["rfq_id"].as_str().unwrap().starts_with("agg-"));
    assert_eq!(sent_a["rfq_id"], sent_b["rfq_id"]);
    assert_eq!(
        *sent_a,
        json!({ "type": "rfq_request", "rfq_id": sent_a["rfq_id"], "product": "sell_sol", "fixed_price": "250000000",
                "expiry_ts": 1759824000, "amount": "500000000", "user_pubkey": user_pubkey(), "nonce": "42" })
    );

    let winner = server.app.winners.get(42).unwrap();
    assert_eq!(
        (
            winner.mm_pubkey.as_str(),
            winner.instance_id.as_str(),
            winner.quote_id.as_str()
        ),
        (b.pubkey.as_str(), "b1", "qb")
    );
    assert_eq!(server.app.registry.counters(&b.pubkey).wins, 1);
    assert_eq!(server.app.stats.snapshot()["quoted"], 1);
}

#[tokio::test]
async fn equal_yields_go_to_the_earliest() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let (ka, kb) = (a.clone(), b.clone());
    let _ma = FakeMm::builder(&a, "a1")
        .on_rfq(move |req| {
            Action::ReplyAfter(Duration::from_millis(80), quote(req, &ka, "500", "qa"))
        })
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&b, "b1")
        .on_rfq(move |req| Action::Reply(quote(req, &kb, "500", "qb")))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    let (_, body) = server.rfq("web-t", "1").await;
    assert_eq!(body["quote_id"], "qb");
}

#[tokio::test]
async fn spoofed_mm_pubkey_is_discarded() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let kb = b.clone();
    let b_for_a = b.clone();
    // A claims to be B with a huge yield.
    let _ma = FakeMm::builder(&a, "a1")
        .on_rfq(move |req| Action::Reply(quote(req, &b_for_a, "999999999", "spoof")))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&b, "b1")
        .on_rfq(move |req| Action::Reply(quote(req, &kb, "10", "honest")))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    let (status, body) = server.rfq("web-s", "1").await;
    assert_eq!(status, 200);
    assert_eq!(body["quote_id"], "honest");
    assert_eq!(body["quotes_received"], 1);
    assert_eq!(body["mms_asked"], 2);
}

#[tokio::test]
async fn declines_are_aggregated() {
    let server = start(test_config()).await;
    let _ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(|req| Action::Reply(decline(req, "expiry too close")))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&key_b(), "b1")
        .on_rfq(|req| Action::ReplyAfter(Duration::from_millis(50), decline(req, "size too large")))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    let (status, body) = server.rfq("web-d", "1").await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "type": "rfq_decline", "rfq_id": "web-d", "reason": "no market maker quoted: expiry too close; size too large" })
    );
}

#[tokio::test]
async fn one_distinct_decline_reason_is_passed_through() {
    let server = start(test_config()).await;
    let _ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(|req| Action::Reply(decline(req, "stale")))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&key_b(), "b1")
        .on_rfq(|req| Action::Reply(decline(req, "stale")))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    let (status, body) = server.rfq("web-d", "1").await;
    assert_eq!((status, body["reason"].as_str().unwrap()), (200, "stale"));
}

#[tokio::test]
async fn silence_times_out_with_504() {
    let server = start(test_config()).await; // rfq_timeout 500 ms
    let _ma = FakeMm::builder(&key_a(), "a1").connect(&server).await;
    server.wait_eligible(1).await;
    let started = Instant::now();
    let (status, body) = server.rfq("web-x", "1").await;
    let elapsed = started.elapsed();
    assert_eq!(status, 504);
    assert_eq!(
        body,
        json!({ "type": "rfq_decline", "rfq_id": "web-x", "reason": "no market maker responded in time" })
    );
    assert!(
        elapsed >= Duration::from_millis(450) && elapsed < Duration::from_secs(2),
        "{elapsed:?}"
    );
    assert_eq!(server.app.registry.counters(&key_a().pubkey).timeouts, 1);
}

#[tokio::test]
async fn late_quote_after_the_window_is_ignored() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let (ka, kb) = (a.clone(), b.clone());
    let _ma = FakeMm::builder(&a, "a1")
        .on_rfq(move |req| Action::Reply(quote(req, &ka, "1", "fast")))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&b, "b1")
        .on_rfq(move |req| {
            Action::ReplyAfter(Duration::from_millis(900), quote(req, &kb, "1000", "slow"))
        })
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    let (_, body) = server.rfq("web-l", "1").await;
    assert_eq!(body["quote_id"], "fast");
    assert_eq!(body["quotes_received"], 1);
    assert_eq!(body["mms_asked"], 2);
}

#[tokio::test]
async fn no_market_maker_is_503() {
    let server = start(test_config()).await;
    let (status, body) = server.rfq("web-n", "1").await;
    assert_eq!(status, 503);
    assert_eq!(body["reason"], "no market maker is online");
}

#[tokio::test]
async fn invalid_rfq_is_400_typed() {
    let server = start(test_config()).await;
    let mut body = rfq_body("web-bad", "1");
    body["product"] = json!("straddle");
    let (status, out) = server.post("/rfq", &body).await;
    assert_eq!(status, 400);
    assert_eq!(
        out,
        json!({ "type": "rfq_decline", "rfq_id": "web-bad", "reason": "invalid rfq: product must be one of sell_sol, buy_sol" })
    );

    let mut body = rfq_body("web-bad", "1");
    body["user_pubkey"] = json!("nope");
    let (status, out) = server.post("/rfq", &body).await;
    assert_eq!(
        (status, out["reason"].as_str().unwrap()),
        (400, "invalid rfq: user_pubkey is not a valid public key")
    );
}

#[tokio::test]
async fn disconnect_fails_the_auction_fast() {
    let mut cfg = test_config();
    cfg.rfq_timeout = Duration::from_secs(5);
    let server = start(cfg).await;
    let _ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(|_| Action::Disconnect)
        .connect(&server)
        .await;
    server.wait_eligible(1).await;
    let started = Instant::now();
    let (status, _) = server.rfq("web-dc", "1").await;
    assert_eq!(status, 504);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}
