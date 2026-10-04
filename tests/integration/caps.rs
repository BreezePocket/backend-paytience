//! §5.6-§5.7: usage from the scripted mock RPC's `getProgramAccounts`, reservations,
//! and the caps that decide who is asked for quotes and market data.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use paytience_backend::mmreg::{Caps, MmRecord};
use paytience_backend::protocol::Channel;
use reqwest::Method;
use serde_json::{Value, json};

use crate::harness::*;

const COSIGNED: &str = "Y29zaWduZWQ=";

/// The mint of the listed test asset (any valid pubkey).
fn tsla_mint() -> String {
    mm_key(5).pubkey
}

fn tsla() -> (String, String, u8) {
    (tsla_mint(), "TSLAon".into(), 8)
}

/// Applies and approves `key` with exactly these caps.
async fn register(server: &TestServer, key: &MmKey, name: &str, total: f64, assets: Value) {
    let (code, body) = server
        .post("/mm/apply", &application(key, name, total, assets))
        .await;
    assert_eq!(code, 200, "{body}");
    let (code, body) = server.approve(key, &json!({})).await;
    assert_eq!(code, 200, "{body}");
}

/// A desk that quotes `yield_amount` on everything and co-signs.
async fn quoting_mm(server: &TestServer, key: &MmKey, yield_amount: &'static str) -> FakeMm {
    let k = key.clone();
    FakeMm::builder(key, "i1")
        .on_rfq(move |req| Action::Reply(quote(req, &k, yield_amount, "q")))
        .on_sign(|req| Action::Reply(sign_ok(req, COSIGNED, None)))
        .connect(server)
        .await
}

/// The harness RFQ: sell_sol, 0.5 SOL at 250 USDC = 125 USD.
async fn sol_rfq(server: &TestServer, nonce: &str) -> (u16, Value) {
    server.rfq("web-1", nonce).await
}

/// buy_sol with 125 USDC on TSLAon = 125 USD.
async fn tsla_rfq(server: &TestServer, asset: &str) -> (u16, Value) {
    let mut body = rfq_body("web-t", "9");
    body["asset"] = json!(asset);
    body["product"] = json!("buy_sol");
    body["amount"] = json!("125000000");
    server.post("/rfq", &body).await
}

fn asked(mm: &FakeMm) -> usize {
    mm.received(Channel::Rfq, "rfq_request", None).len()
}

fn decline(reason: &str) -> Value {
    json!({ "type": "rfq_decline", "rfq_id": "web-1", "reason": reason })
}

#[tokio::test]
async fn rfq_skips_an_mm_with_no_cap_for_the_asset() {
    let (rpc, _) = chain_rpc(Chain {
        assets: vec![tsla()],
        positions: Vec::new(),
    })
    .await;
    let server = start(scan_config(&rpc.url)).await;
    let (a, b) = (key_a(), key_b());
    register(&server, &a, "alpha", 5000.0, json!({ "SOL": 5000 })).await;
    register(
        &server,
        &b,
        "bravo",
        5000.0,
        json!({ "SOL": 5000, "TSLAon": 5000 }),
    )
    .await;
    // Alpha would win on yield, if it were asked.
    let mm_a = quoting_mm(&server, &a, "9000").await;
    let mm_b = quoting_mm(&server, &b, "1000").await;
    server.wait_eligible(2).await;
    server.wait_usage_known(&a).await;
    server.wait_usage_known(&b).await;

    let (status, body) = tsla_rfq(&server, "TSLAon").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "rfq_response", "{body}");
    assert_eq!(body["mm_pubkey"], b.pubkey);
    assert_eq!(asked(&mm_a), 0, "no TSLAON cap: not asked at all");
    assert_eq!(asked(&mm_b), 1);
    assert_eq!(
        mm_b.received(Channel::Rfq, "rfq_request", None)[0]["asset"],
        "TSLAon"
    );

    // Both have a SOL cap: both are asked and the better yield wins.
    let (_, body) = sol_rfq(&server, "1").await;
    assert_eq!(body["mm_pubkey"], a.pubkey);
    assert_eq!((asked(&mm_a), asked(&mm_b)), (1, 2));
}

#[tokio::test]
async fn rfq_skips_mms_over_their_asset_or_total_cap_while_another_wins() {
    let (a, b, c) = (key_a(), key_b(), mm_key(8));
    let (rpc, _) = chain_rpc(Chain {
        assets: vec![tsla()],
        positions: vec![
            // Alpha: 0.7 SOL at 250 = 175 USD, expired long ago but never settled.
            ChainPosition {
                expiry_ts: 1_000_000_000,
                ..ChainPosition::sol(&a, 0.7, 1)
            },
            // Bravo: 200 USDC committed on TSLAon (buy), and nothing on SOL.
            ChainPosition {
                mint: Some(tsla_mint()),
                product: 1,
                user_collateral: 200_000_000,
                ..ChainPosition::sol(&b, 0.0, 2)
            },
            // Charlie: a huge position that is settled, so it does not count.
            ChainPosition {
                settled: true,
                ..ChainPosition::sol(&c, 4000.0, 3)
            },
        ],
    })
    .await;
    let server = start(scan_config(&rpc.url)).await;
    // Alpha: 175 + 125 > 200 on SOL (asset cap), though the total has room.
    register(&server, &a, "alpha", 1000.0, json!({ "SOL": 200 })).await;
    // Bravo: SOL itself is empty, but 200 + 125 > 300 in total.
    register(
        &server,
        &b,
        "bravo",
        300.0,
        json!({ "SOL": 300, "TSLAON": 300 }),
    )
    .await;
    register(&server, &c, "charlie", 1000.0, json!({ "SOL": 1000 })).await;
    let mm_a = quoting_mm(&server, &a, "9000").await;
    let mm_b = quoting_mm(&server, &b, "8000").await;
    let mm_c = quoting_mm(&server, &c, "10").await;
    server.wait_eligible(3).await;
    for key in [&a, &b, &c] {
        server.wait_usage_known(key).await;
    }
    assert_eq!(server.app.exposure.usage(&a.pubkey).asset_usd("SOL"), 175.0);
    assert_eq!(
        server.app.exposure.usage(&b.pubkey).asset_usd("TSLAON"),
        200.0
    );
    assert_eq!(server.app.exposure.usage(&c.pubkey).total_usd, 0.0);

    let (status, body) = sol_rfq(&server, "1").await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["type"], "rfq_response", "{body}");
    assert_eq!(body["mm_pubkey"], c.pubkey, "the only MM with room wins");
    assert_eq!((asked(&mm_a), asked(&mm_b), asked(&mm_c)), (0, 0, 1));

    // With charlie gone, everyone left is excluded by a cap.
    drop(mm_c);
    server.wait_eligible(2).await;
    assert_eq!(
        sol_rfq(&server, "2").await,
        (200, decline("market-maker capacity reached for SOL"))
    );
    assert_eq!((asked(&mm_a), asked(&mm_b)), (0, 0));
}

#[tokio::test]
async fn nobody_to_ask_gives_the_two_decline_reasons() {
    let a = key_a();
    let (rpc, _) = chain_rpc(Chain {
        assets: vec![tsla()],
        positions: vec![ChainPosition::sol(&a, 0.4, 1)], // 100 USD
    })
    .await;
    let server = start(scan_config(&rpc.url)).await;
    register(&server, &a, "alpha", 200.0, json!({ "SOL": 200 })).await;
    let mm = quoting_mm(&server, &a, "500").await;
    server.wait_eligible(1).await;
    server.wait_usage_known(&a).await;

    // 100 + 125 > 200: excluded by a cap.
    assert_eq!(
        sol_rfq(&server, "1").await,
        (200, decline("market-maker capacity reached for SOL"))
    );
    // Listed on chain, but no online MM has a cap for it.
    let (status, body) = tsla_rfq(&server, "tslaon").await;
    assert_eq!(
        (status, body),
        (
            200,
            json!({ "type": "rfq_decline", "rfq_id": "web-t", "reason": "no market maker quotes TSLAON" })
        )
    );
    // Not SOL and no AssetConfig: the backend cannot size it.
    let (status, body) = tsla_rfq(&server, "WBTC").await;
    assert_eq!(
        (status, body),
        (
            200,
            json!({ "type": "rfq_decline", "rfq_id": "web-t", "reason": "WBTC is not listed on chain" })
        )
    );
    assert_eq!(asked(&mm), 0);
    let stats = server.app.stats.snapshot();
    assert_eq!((&stats["rfqs"], &stats["declined"]), (&json!(3), &json!(3)));

    // A smaller RFQ still fits: 100 + 75 <= 200.
    let mut small = rfq_body("web-s", "5");
    small["amount"] = json!("300000000");
    let (status, body) = server.post("/rfq", &small).await;
    assert_eq!(
        (status, &body["type"]),
        (200, &json!("rfq_response")),
        "{body}"
    );
}

#[tokio::test]
async fn unknown_usage_is_not_asked_until_a_scan_succeeds() {
    let healthy = Arc::new(AtomicBool::new(false));
    let flag = healthy.clone();
    let rpc = scripted_rpc(move |method, params, _| {
        assert_eq!(method, "getProgramAccounts");
        if flag.load(Ordering::SeqCst) {
            program_accounts(&Chain::default(), params)
        } else {
            RpcReply::Status(503, "node is behind".into())
        }
    })
    .await;
    let server = start(scan_config(&rpc.url)).await;
    let a = key_a();
    register(&server, &a, "alpha", 5000.0, json!({ "SOL": 5000 })).await;
    let mm = quoting_mm(&server, &a, "500").await;
    server.wait_eligible(1).await;
    wait_until(
        || rpc.calls_to("getProgramAccounts").len() >= 2,
        "failed scans",
    )
    .await;

    assert!(!server.app.exposure.usage(&a.pubkey).known);
    // Fail closed: online, has a cap, but its usage is unknown.
    assert_eq!(
        sol_rfq(&server, "1").await,
        (200, decline("no market maker quotes SOL"))
    );
    assert_eq!(asked(&mm), 0);
    let (_, list) = server.admin(Method::GET, "/admin/mms", None).await;
    assert_eq!(
        list["mms"][0]["usage"],
        json!({ "known": false, "total_usd": 0, "by_asset_usd": {}, "reserved_usd": 0 })
    );

    healthy.store(true, Ordering::SeqCst);
    server.wait_usage_known(&a).await;
    let (status, body) = sol_rfq(&server, "2").await;
    assert_eq!(
        (status, &body["type"]),
        (200, &json!("rfq_response")),
        "{body}"
    );
    assert_eq!(asked(&mm), 1);
}

#[tokio::test]
async fn approving_triggers_a_scan_right_away() {
    let (rpc, _) = chain_rpc(Chain::default()).await;
    let mut cfg = scan_config(&rpc.url);
    cfg.exposure_sync = Duration::from_secs(600);
    let server = start(cfg).await;
    // The startup scan: asset configs only, nobody is approved yet.
    wait_until(
        || rpc.calls_to("getProgramAccounts").len() == 1,
        "startup scan",
    )
    .await;
    let a = key_a();
    register(&server, &a, "alpha", 5000.0, json!({ "SOL": 5000 })).await;
    // Far sooner than the 10 minute interval.
    server.wait_usage_known(&a).await;

    // One AssetConfig scan per cycle, then PositionAccount and AssetPosition per MM,
    // each filtered by discriminator at 0 and market_maker at 40.
    let calls = rpc.calls_to("getProgramAccounts");
    assert_eq!(calls.len(), 4, "{calls:?}");
    let filters = |call: &Value| call["params"][1]["filters"].clone();
    let disc =
        |d: &[u8; 8]| json!({ "memcmp": { "offset": 0, "bytes": bs58::encode(d).into_string() } });
    let mm = json!({ "memcmp": { "offset": 40, "bytes": a.pubkey } });
    assert_eq!(filters(&calls[1]), json!([disc(&ASSET_CONFIG_DISC)]));
    assert_eq!(filters(&calls[2]), json!([disc(&POSITION_DISC), mm]));
    assert_eq!(filters(&calls[3]), json!([disc(&ASSET_POSITION_DISC), mm]));
    assert_eq!(calls[2]["params"][0], PROGRAM_ID);
    assert_eq!(
        (
            &calls[2]["params"][1]["encoding"],
            &calls[2]["params"][1]["commitment"]
        ),
        (&json!("base64"), &json!("confirmed"))
    );
}

#[tokio::test]
async fn a_cosign_reserves_capacity_and_a_refused_broadcast_releases_it() {
    let chain = Arc::new(std::sync::Mutex::new(Chain::default()));
    let shared = chain.clone();
    let rpc = scripted_rpc(move |method, params, seen| match method {
        "getProgramAccounts" => program_accounts(&shared.lock().unwrap(), params),
        // The node refuses the first broadcast and accepts the second.
        "sendTransaction" if seen == 0 => RpcReply::Error("Transaction simulation failed".into()),
        "sendTransaction" => RpcReply::Result(json!("5igNaTuRe")),
        other => RpcReply::Error(format!("unexpected method {other}")),
    })
    .await;
    let mut cfg = scan_config(&rpc.url);
    cfg.solana_rpc = rpc.url.clone();
    cfg.broadcast = true;
    let server = start(cfg).await;
    let a = key_a();
    // Room for one 125 USD position, not two.
    register(&server, &a, "alpha", 200.0, json!({ "SOL": 200 })).await;
    let _mm = quoting_mm(&server, &a, "500").await;
    server.wait_eligible(1).await;
    server.wait_usage_known(&a).await;
    let usage = || server.app.exposure.usage(&a.pubkey);

    assert_eq!(sol_rfq(&server, "1").await.1["type"], "rfq_response");

    // Co-signed, but the node refuses the broadcast (422): the reservation is dropped.
    let (refused_tx, _) = tx_case("compute_budget_prefixed");
    let (status, body) = server.sign("web-sign-1", &refused_tx).await;
    assert_eq!(status, 422, "{body}");
    assert_eq!(
        body["reason"],
        "broadcast failed: Transaction simulation failed"
    );
    assert_eq!((usage().total_usd, usage().reserved_usd), (0.0, 0.0));
    assert_eq!(
        sol_rfq(&server, "2").await.1["type"],
        "rfq_response",
        "capacity is back"
    );

    // Co-signed and broadcast: reserved immediately, before any scan can see it.
    let (tx, nonce) = tx_case("open_position_sol");
    let (status, body) = server.sign("web-sign-2", &tx).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["signature"], "5igNaTuRe");
    assert_eq!((usage().total_usd, usage().reserved_usd), (125.0, 125.0));
    assert_eq!(
        sol_rfq(&server, "3").await,
        (200, decline("market-maker capacity reached for SOL"))
    );
    let (_, list) = server.admin(Method::GET, "/admin/mms", None).await;
    assert_eq!(
        list["mms"][0]["usage"],
        json!({ "known": true, "total_usd": 125, "by_asset_usd": { "SOL": 125 }, "reserved_usd": 125 })
    );

    // Scans that do not show the position keep the reservation.
    let scans = rpc.calls_to("getProgramAccounts").len();
    wait_until(
        || rpc.calls_to("getProgramAccounts").len() >= scans + 6,
        "two more scans",
    )
    .await;
    assert_eq!(usage().reserved_usd, 125.0);

    // The position lands: the scan counts it on chain and the reservation is cleared.
    chain
        .lock()
        .unwrap()
        .positions
        .push(ChainPosition::sol(&a, 0.5, nonce.parse().unwrap()));
    wait_until(
        || usage().reserved_usd == 0.0,
        "the scan to see the position",
    )
    .await;
    assert_eq!(
        (usage().total_usd, usage().asset_usd("SOL")),
        (125.0, 125.0)
    );
    assert_eq!(
        sol_rfq(&server, "4").await,
        (200, decline("market-maker capacity reached for SOL")),
        "still carried, now on chain"
    );
}

/// A desk health listing SOL and TSLAon at `spot`.
fn health_with_tsla(key: &MmKey, spot: f64) -> Value {
    let mut h = desk_health(key, spot, 0.0);
    h["assets"].as_array_mut().unwrap().push(json!({
        "asset": "TSLAon", "underlying": "TSLA", "venue": "pyth", "tradable": true, "mint": tsla_mint(),
        "decimals": 8, "expiry_time_of_day": 46800, "synthetic": false, "fresh": true, "spot": spot,
        "atm_vol": 0.5, "expiries": 3,
    }));
    h
}

#[tokio::test]
async fn market_data_comes_only_from_mms_permitted_for_the_asset() {
    let (a, b) = (key_a(), key_b());
    let mut cfg = test_config();
    cfg.seed_mms = vec![
        MmRecord::approved(
            a.pubkey.clone(),
            "alpha",
            Caps::new(5000.0, &[("SOL", 5000.0)]),
        ),
        MmRecord::approved(
            b.pubkey.clone(),
            "bravo",
            Caps::new(5000.0, &[("SOL", 5000.0), ("TSLAON", 5000.0)]),
        ),
    ];
    let server = start(cfg).await;
    let answer = |req: &Value| match req["method"].as_str() {
        Some("board") => Action::Reply(market_ok(
            req,
            json!({ "asset": req["params"]["asset"], "expiries": [] }),
        )),
        Some("expiries") => Action::Reply(market_ok(req, json!({ "expiries": [] }))),
        Some("price") => Action::Reply(market_ok(req, json!({ "spot": 1 }))),
        _ => Action::Ignore,
    };
    // Alpha also lists TSLAon (at 100), but has no approved cap for it.
    let mm_a = FakeMm::builder(&a, "a1")
        .health(health_with_tsla(&a, 100.0))
        .on_rfq(answer)
        .connect(&server)
        .await;
    let mm_b = FakeMm::builder(&b, "b1")
        .health(health_with_tsla(&b, 300.0))
        .on_rfq(answer)
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    server.wait_fresh(2).await;

    // /assets and /health.assets: SOL from both (median 200), TSLAon from bravo only.
    let (_, assets) = server.get("/assets").await;
    let (_, health) = server.get("/health").await;
    assert_eq!(assets["assets"], health["assets"]);
    let list = assets["assets"].as_array().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!(
        (&list[0]["asset"], &list[0]["spot"]),
        (&json!("SOL"), &json!(200))
    );
    assert_eq!(
        (&list[1]["asset"], &list[1]["spot"]),
        (&json!("TSLAon"), &json!(300))
    );

    let sent = |mm: &FakeMm, method: &str| {
        mm.received(Channel::Rfq, "market_request", Some(method))
            .len()
    };
    for (path, method) in [
        ("/board?asset=TSLAon", "board"),
        ("/expiries?asset=tslaon", "expiries"),
        ("/price?asset=TSLAon", "price"),
    ] {
        let (status, body) = server.get(path).await;
        assert_eq!(status, 200, "{path}: {body}");
        assert_eq!((sent(&mm_a, method), sent(&mm_b, method)), (0, 1), "{path}");
    }
    // SOL (also the default asset) asks both.
    assert_eq!(server.get("/board?asset=SOL").await.0, 200);
    assert_eq!((sent(&mm_a, "board"), sent(&mm_b, "board")), (1, 2));
    assert_eq!(server.get("/expiries").await.0, 200);
    assert_eq!((sent(&mm_a, "expiries"), sent(&mm_b, "expiries")), (1, 2));

    // Online, but nobody may quote it.
    let nobody = (404, json!({ "error": "no market maker quotes WBTC" }));
    assert_eq!(server.get("/board?asset=WBTC").await, nobody);
    assert_eq!(server.get("/expiries?asset=wbtc").await, nobody);
    assert_eq!(server.get("/price?asset=WBTC").await, nobody);
    assert_eq!((sent(&mm_a, "board"), sent(&mm_b, "board")), (1, 2));
}

#[tokio::test]
async fn health_exposure_and_admin_listing_show_the_backends_view() {
    let (a, b, pending) = (key_a(), key_b(), mm_key(8));
    let (rpc, _) = chain_rpc(Chain {
        assets: vec![tsla()],
        positions: vec![
            ChainPosition::sol(&a, 0.5, 1), // 125 USD
            // 0.5 TSLAon (8 decimals) sold at 100 USDC = 50 USD.
            ChainPosition {
                mint: Some(tsla_mint()),
                fixed_price: 100_000_000,
                user_collateral: 50_000_000,
                ..ChainPosition::sol(&b, 0.0, 2)
            },
        ],
    })
    .await;
    let server = start(scan_config(&rpc.url)).await;
    register(&server, &a, "alpha", 1000.0, json!({ "SOL": 600 })).await;
    register(
        &server,
        &b,
        "bravo",
        500.0,
        json!({ "SOL": 300, "TSLAon": 400 }),
    )
    .await;
    server
        .post(
            "/mm/apply",
            &application(&pending, "later", 9999.0, json!({ "SOL": 9999 })),
        )
        .await;
    let _mm_a = FakeMm::builder(&a, "a1")
        .health(desk_health(&a, 200.0, 1000.0))
        .connect(&server)
        .await;
    let _mm_b = FakeMm::builder(&b, "b1")
        .health(desk_health(&b, 204.0, 234.5))
        .connect(&server)
        .await;
    server.wait_eligible(2).await;
    server.wait_fresh(2).await;
    server.wait_usage_known(&a).await;
    server.wait_usage_known(&b).await;

    let (_, h) = server.get("/health").await;
    assert_eq!(
        h["exposure"],
        json!({
            // The backend's own view over the online MMs: usage and approved caps.
            "totalUsd": 175, "capTotalUsd": 1500,
            "perAsset": { "SOL": { "usedUsd": 125, "capUsd": 900 }, "TSLAON": { "usedUsd": 50, "capUsd": 400 } },
            // Still the desks' own figures.
            "capPerExpiryUsd": 10000,
            "perExpiry": { "1759824000": { "onChainUsd": 1234.5, "reservedUsd": 0 } },
        })
    );

    let (status, list) = server.admin(Method::GET, "/admin/mms", None).await;
    assert_eq!(status, 200);
    let mms = list["mms"].as_array().unwrap();
    assert_eq!(
        mms.iter()
            .map(|m| m["name"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha", "bravo", "later"],
        "creation order"
    );
    assert_eq!(
        mms[0]["usage"],
        json!({ "known": true, "total_usd": 125, "by_asset_usd": { "SOL": 125 }, "reserved_usd": 0 })
    );
    assert_eq!(
        mms[1]["usage"],
        json!({ "known": true, "total_usd": 50, "by_asset_usd": { "TSLAON": 50 }, "reserved_usd": 0 })
    );
    // Record fields, instances, counters and usage, for a pending MM too.
    let later = &mms[2];
    assert_eq!(
        (&later["status"], &later["approved"]),
        (&json!("pending"), &Value::Null)
    );
    assert_eq!(later["contact"], "ops@example.test");
    assert_eq!(later["instances"], json!([]));
    assert_eq!(later["rfqs_asked"], 0);
    assert_eq!(
        later["usage"]["known"], false,
        "only approved MMs are scanned"
    );
    assert_eq!(mms[0]["instances"][0]["instance_id"], "a1");
}
