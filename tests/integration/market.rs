//! §2.1-2.5 market data over `market_request`, through the REST API.

use std::time::Duration;

use paytience_backend::protocol::Channel;
use serde_json::{Value, json};

use crate::harness::*;

fn board_body(index: f64, fee: f64, cells: Value) -> Value {
    json!({
        "asset": "SOL", "underlying": "SOL", "venue": "deribit", "tradable": true, "mint": null, "decimals": 9,
        "product": "sell_sol", "token": "sol", "yield_token": "usdc", "amount": "1000000000",
        "index_price": index, "fee_pct": fee, "mm_pubkey": "self-reported", "generated_at": 1759000000000u64,
        "expiries": [{ "expiry_ts": 1759824000, "days": 9.5, "forward_price": index, "atm_vol": 0.7, "quotes": cells }],
    })
}

fn cell(price: f64, yield_amount: &str, apr: f64) -> Value {
    json!({ "moneyness": price / 200.0, "fixed_price": price, "yield_amount": yield_amount, "yield_pct": 1.0, "apr_pct": apr,
            "option_mark_usdc": 1.0, "implied_vol": 0.7, "price_source": "deribit_mark", "instrument": null })
}

async fn board_mm(server: &TestServer, key: &MmKey, delay: Duration, board: Value) -> FakeMm {
    FakeMm::builder(key, "i1")
        .on_rfq(move |req| {
            if req["type"] == "market_request" && req["method"] == "board" {
                Action::ReplyAfter(delay, market_ok(req, board.clone()))
            } else {
                Action::Ignore
            }
        })
        .connect(server)
        .await
}

#[tokio::test]
async fn health_aggregates_two_mms() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let _ma = FakeMm::builder(&a, "a1")
        .health(desk_health(&a, 200.0, 1000.0))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&b, "b1")
        .health(desk_health(&b, 204.0, 234.5))
        .connect(&server)
        .await;
    server.wait_fresh(2).await;

    let (status, h) = server.get("/health").await;
    assert_eq!(status, 200);
    assert_eq!(h["ok"], true);
    assert_eq!(h["service"], "paytience-backend");
    assert_eq!(h["version"], "0.1.0");
    assert!(h.get("mm_count").is_none(), "the MM count is not public");
    assert_eq!(h["mm_pubkey"], a.pubkey);
    assert_eq!(h["program_id"], PROGRAM_ID);
    assert_eq!(h["usdc_mint"], usdc_mint());
    assert_eq!(h["dry_run"], false);
    assert_eq!(
        h["price"],
        json!({ "source": "aggregate", "spot": 202, "spotAgeMs": 1000, "vol": 0.7, "expiries": 12 })
    );
    // §5.7: totals and caps are the backend's own view (no scan here: nothing on
    // chain); the per-expiry figures stay the desks'.
    assert_eq!(h["exposure"]["totalUsd"], 0);
    assert_eq!(h["exposure"]["capTotalUsd"], 40000);
    assert_eq!(h["exposure"]["capPerExpiryUsd"], 10000);
    assert_eq!(
        h["exposure"]["perAsset"]["SOL"],
        json!({ "usedUsd": 0, "capUsd": 40000 })
    );
    assert_eq!(
        h["exposure"]["perExpiry"]["1759824000"],
        json!({ "onChainUsd": 1234.5, "reservedUsd": 0 })
    );
    assert!(
        h["assets"][0].get("mm_count").is_none(),
        "the MM count is not public"
    );
    assert_eq!(h["assets"][0]["spot"], 202);
    let stats = h["stats"].as_object().unwrap();
    for k in [
        "rfqs",
        "quoted",
        "declined",
        "timed_out",
        "signs",
        "sign_rejections",
        "sign_timeouts",
        "broadcasts",
    ] {
        assert!(stats.contains_key(k), "{k}");
    }
}

#[tokio::test]
async fn health_offline_shape() {
    let server = start(test_config()).await;
    let (status, h) = server.get("/health").await;
    assert_eq!(status, 200);
    assert_eq!(h["ok"], false);
    assert!(h.get("mm_count").is_none(), "the MM count is not public");
    assert_eq!(h["mm_pubkey"], "");
    assert_eq!(h["price"]["spot"], Value::Null);
    assert_eq!(h["assets"], json!([]));
    assert_eq!(
        h["exposure"],
        json!({ "totalUsd": 0, "capTotalUsd": 0, "capPerExpiryUsd": 0, "perExpiry": {}, "perAsset": {} })
    );
}

#[tokio::test]
async fn two_instances_of_one_mm_count_once() {
    let server = start(test_config()).await;
    let a = key_a();
    let _i1 = FakeMm::builder(&a, "i1")
        .health(desk_health(&a, 200.0, 1000.0))
        .connect(&server)
        .await;
    let _i2 = FakeMm::builder(&a, "i2")
        .health(desk_health(&a, 200.0, 1000.0))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    server.wait_fresh(1).await;
    let (_, h) = server.get("/health").await;
    assert!(h.get("mm_count").is_none(), "the MM count is not public");
    assert_eq!(
        h["exposure"]["perExpiry"]["1759824000"]["onChainUsd"], 1000,
        "not double counted"
    );
    assert_eq!(h["exposure"]["capTotalUsd"], 20000, "one MM, one cap");
}

#[tokio::test]
async fn assets_are_merged() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let _ma = FakeMm::builder(&a, "a1")
        .health(desk_health(&a, 200.0, 0.0))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&b, "b1")
        .health(desk_health(&b, 210.0, 0.0))
        .connect(&server)
        .await;
    server.wait_fresh(2).await;
    let (status, body) = server.get("/assets").await;
    assert_eq!(status, 200);
    let assets = body["assets"].as_array().unwrap();
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0]["asset"], "SOL");
    assert_eq!(assets[0]["spot"], 205);
    assert!(
        assets[0].get("mm_count").is_none(),
        "the MM count is not public"
    );
    assert_eq!(assets[0]["tradable"], true);
}

#[tokio::test]
async fn board_is_merged_through_rest() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let ma = board_mm(
        &server,
        &a,
        Duration::ZERO,
        board_body(
            200.0,
            0.5,
            json!([cell(210.0, "1000", 10.0), cell(220.0, "900", 9.0)]),
        ),
    )
    .await;
    let _mb = board_mm(
        &server,
        &b,
        Duration::ZERO,
        board_body(
            204.0,
            0.25,
            json!([cell(210.0, "1500", 11.0), cell(205.0, "5", 1.0)]),
        ),
    )
    .await;
    server.wait_eligible(2).await;

    let (status, board) = server
        .get("/board?asset=SOL&moneyness=1.05,abc,1.1&max_days=30&exact=false")
        .await;
    assert_eq!(status, 200, "{board}");
    assert!(
        board.get("mm_count").is_none(),
        "the MM count is not public"
    );
    assert_eq!(board["index_price"], 202);
    assert_eq!(board["fee_pct"], 0.25);
    assert_eq!(board["mm_pubkey"], b.pubkey, "B wins 2 of 3 cells");
    let quotes = board["expiries"][0]["quotes"].as_array().unwrap();
    let prices: Vec<f64> = quotes
        .iter()
        .map(|q| q["fixed_price"].as_f64().unwrap())
        .collect();
    assert_eq!(prices, [205.0, 210.0, 220.0], "nearest to 202 first");
    assert_eq!(quotes[1]["yield_amount"], "1500");
    assert_eq!(quotes[1]["mm_pubkey"], b.pubkey);
    assert_eq!(quotes[1]["fee_pct"], 0.25);
    assert_eq!(quotes[2]["mm_pubkey"], a.pubkey);
    assert_eq!(quotes[2]["fee_pct"], 0.5);

    // Only the params present in the query are forwarded; product is left to the desk.
    let req = &ma.received(Channel::Rfq, "market_request", Some("board"))[0];
    assert!(req["request_id"].as_str().unwrap().starts_with("mkt-"));
    assert_eq!(
        req["params"],
        json!({ "asset": "SOL", "moneyness": [1.05, 1.1], "max_days": 30, "exact": false })
    );
}

#[tokio::test]
async fn concurrent_identical_boards_share_one_fan_out() {
    let server = start(test_config()).await;
    let a = key_a();
    let ma = board_mm(
        &server,
        &a,
        Duration::from_millis(200),
        board_body(200.0, 0.5, json!([cell(210.0, "1", 1.0)])),
    )
    .await;
    server.wait_eligible(1).await;

    let calls = (0..8).map(|_| server.get("/board?product=sell_sol&moneyness=1.05"));
    let results = futures_util::future::join_all(calls).await;
    for (status, body) in &results {
        assert_eq!(*status, 200);
        assert_eq!(body, &results[0].1);
    }
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("board"))
            .len(),
        1
    );

    // Cached within the TTL, including an equivalent spelling of the same query.
    let (status, _) = server.get("/board?moneyness=1.050&product=sell_sol").await;
    assert_eq!(status, 200);
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("board"))
            .len(),
        1
    );

    // A different query is a different fan-out.
    server.get("/board?product=buy_sol").await;
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("board"))
            .len(),
        2
    );
}

#[tokio::test]
async fn board_errors_use_the_shared_status_and_are_not_cached() {
    let server = start(test_config()).await;
    let stale = |req: &Value| {
        if req["method"] == "board" {
            Action::Reply(market_err(req, "SOL reference price is stale", 400))
        } else {
            Action::Ignore
        }
    };
    let ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(stale)
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&key_b(), "b1")
        .on_rfq(stale)
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    let (status, body) = server.get("/board").await;
    assert_eq!(
        (status, body),
        (400, json!({ "error": "SOL reference price is stale" }))
    );
    let (status, _) = server.get("/board").await;
    assert_eq!(status, 400);
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("board"))
            .len(),
        2
    );
}

#[tokio::test]
async fn board_error_with_a_timeout_is_503() {
    let server = start(test_config()).await;
    let _ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(|req| {
            if req["method"] == "board" {
                Action::Reply(market_err(req, "unknown asset", 400))
            } else {
                Action::Ignore
            }
        })
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&key_b(), "b1").connect(&server).await; // never answers
    server.wait_eligible(2).await;
    let (status, body) = server.get("/board?asset=NOPE").await;
    assert_eq!(status, 503);
    assert_eq!(body["error"], "unknown asset", "tie: first seen");
}

#[tokio::test]
async fn board_one_success_is_enough() {
    let server = start(test_config()).await;
    let _ma = board_mm(
        &server,
        &key_a(),
        Duration::ZERO,
        board_body(200.0, 0.5, json!([cell(210.0, "1", 1.0)])),
    )
    .await;
    let _mb = FakeMm::builder(&key_b(), "b1").connect(&server).await; // times out
    server.wait_eligible(2).await;
    let (status, body) = server.get("/board").await;
    assert_eq!(status, 200);
    assert!(body.get("mm_count").is_none(), "the MM count is not public");
}

#[tokio::test]
async fn no_mm_online_is_503() {
    let server = start(test_config()).await;
    for path in ["/board", "/expiries", "/price"] {
        let (status, body) = server.get(path).await;
        assert_eq!(
            (status, body),
            (503, json!({ "error": "no market maker is online" })),
            "{path}"
        );
    }
}

#[tokio::test]
async fn expiries_are_merged_and_cached() {
    let server = start(test_config()).await;
    let exp = |fwd: f64, strikes: Value| {
        move |req: &Value| {
            if req["method"] == "expiries" {
                Action::Reply(market_ok(
                    req,
                    json!({ "expiries": [
                    { "expiry_ts": 1759824000, "days": 9.5, "forward_price": fwd, "atm_vol": null, "strikes": strikes.clone() }
                ] }),
                ))
            } else {
                Action::Ignore
            }
        }
    };
    let ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(exp(200.0, json!([190, 200])))
        .connect(&server)
        .await;
    let _mb = FakeMm::builder(&key_b(), "b1")
        .on_rfq(exp(202.0, json!([200, 210])))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;

    let (status, body) = server.get("/expiries?asset=SOL").await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "expiries": [{ "expiry_ts": 1759824000, "days": 9.5, "forward_price": 201, "atm_vol": null, "strikes": [190, 200, 210] }] })
    );
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("expiries"))[0]["params"],
        json!({ "asset": "SOL" })
    );
    // Case-insensitive cache key.
    server.get("/expiries?asset=sol").await;
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("expiries"))
            .len(),
        1
    );
}

#[tokio::test]
async fn price_comes_verbatim_from_the_freshest_mm() {
    let server = start(test_config()).await;
    let price = |spot: f64| {
        move |req: &Value| {
            if req["method"] == "price" {
                Action::Reply(market_ok(
                    req,
                    json!({ "source": "deribit", "spot": spot, "extra": [1, 2] }),
                ))
            } else {
                Action::Ignore
            }
        }
    };
    let ma = FakeMm::builder(&key_a(), "a1")
        .on_rfq(price(200.0))
        .connect(&server)
        .await;
    server.wait_fresh(1).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    let mb = FakeMm::builder(&key_b(), "b1")
        .on_rfq(price(204.0))
        .connect(&server)
        .await;
    server.wait_fresh(2).await;

    let (status, body) = server.get("/price?asset=SOL").await;
    assert_eq!(status, 200);
    assert_eq!(
        body,
        json!({ "source": "deribit", "spot": 204.0, "extra": [1, 2] })
    );
    assert_eq!(
        mb.received(Channel::Rfq, "market_request", Some("price"))[0]["params"],
        json!({ "asset": "SOL" })
    );
    assert!(
        ma.received(Channel::Rfq, "market_request", Some("price"))
            .is_empty()
    );
}

#[tokio::test]
async fn health_is_polled_on_the_configured_interval() {
    let mut cfg = test_config();
    cfg.health_poll = Duration::from_millis(100);
    let server = start(cfg).await;
    let ma = FakeMm::builder(&key_a(), "a1").connect(&server).await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    let n = ma
        .received(Channel::Rfq, "market_request", Some("health"))
        .len();
    assert!((3..=7).contains(&n), "{n} health polls");
    assert_eq!(
        ma.received(Channel::Rfq, "market_request", Some("health"))[0]["params"],
        json!({})
    );
}

/// A board MM whose health says `dry_run` as given.
async fn board_mm_with_health(
    server: &TestServer,
    key: &MmKey,
    health: Value,
    board: Value,
) -> FakeMm {
    FakeMm::builder(key, "i1")
        .health(health)
        .on_rfq(move |req| {
            if req["type"] == "market_request" && req["method"] == "board" {
                Action::Reply(market_ok(req, board.clone()))
            } else {
                Action::Ignore
            }
        })
        .connect(server)
        .await
}

#[tokio::test]
async fn market_data_prefers_live_mms_over_dry_run_ones() {
    let server = start(test_config()).await;
    let (a, b) = (key_a(), key_b());
    let _live = board_mm_with_health(
        &server,
        &a,
        desk_health(&a, 200.0, 100.0),
        board_body(200.0, 0.5, json!([cell(210.0, "1000", 10.0)])),
    )
    .await;
    // The dry-run MM prices higher everywhere; none of it is shown.
    let dry = board_mm_with_health(
        &server,
        &b,
        dry_run_health(&b, 300.0),
        board_body(
            300.0,
            0.0,
            json!([cell(210.0, "9000", 90.0), cell(220.0, "9", 1.0)]),
        ),
    )
    .await;
    server.wait_eligible(2).await;
    server.wait_health(2).await;

    let (status, board) = server.get("/board").await;
    assert_eq!(status, 200, "{board}");
    assert!(
        board.get("mm_count").is_none(),
        "the MM count is not public"
    );
    assert_eq!(board["index_price"], 200);
    let quotes = board["expiries"][0]["quotes"].as_array().unwrap();
    assert_eq!(quotes.len(), 1);
    assert_eq!(quotes[0]["mm_pubkey"], a.pubkey);
    assert_eq!(quotes[0]["yield_amount"], "1000");
    assert!(
        dry.received(Channel::Rfq, "market_request", Some("board"))
            .is_empty()
    );

    let (status, h) = server.get("/health").await;
    assert_eq!(status, 200);
    assert_eq!(h["dry_run"], json!(false));
    assert!(h.get("mm_count").is_none(), "the MM count is not public");
    assert_eq!(h["mm_pubkey"], a.pubkey);
    assert_eq!(h["price"]["spot"], 200);
    let (_, assets) = server.get("/assets").await;
    assert!(
        assets["assets"][0].get("mm_count").is_none(),
        "the MM count is not public"
    );
    assert_eq!(assets["assets"][0]["spot"], 200);
}

#[tokio::test]
async fn dry_run_only_mms_still_serve_market_data() {
    let server = start(test_config()).await;
    let b = key_b();
    let _dry = board_mm_with_health(
        &server,
        &b,
        dry_run_health(&b, 300.0),
        board_body(300.0, 0.0, json!([cell(310.0, "9000", 90.0)])),
    )
    .await;
    server.wait_eligible(1).await;
    server.wait_health(1).await;

    let (status, board) = server.get("/board").await;
    assert_eq!(status, 200, "{board}");
    assert!(
        board.get("mm_count").is_none(),
        "the MM count is not public"
    );
    assert_eq!(board["expiries"][0]["quotes"][0]["mm_pubkey"], b.pubkey);
    let (_, h) = server.get("/health").await;
    assert_eq!(h["dry_run"], json!(true));
    assert!(h.get("mm_count").is_none(), "the MM count is not public");
    assert_eq!(h["price"]["spot"], 300);
}
