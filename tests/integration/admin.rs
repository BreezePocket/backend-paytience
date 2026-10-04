//! §2.9 `GET /admin/mms`.

use serde_json::{Value, json};

use crate::harness::*;

const TOKEN: &str = ADMIN_TOKEN;

async fn admin(server: &TestServer, auth: Option<&str>) -> (u16, Value) {
    let mut req = server.http.get(server.url("/admin/mms"));
    if let Some(a) = auth {
        req = req.header("authorization", a);
    }
    let r = req.send().await.unwrap();
    (r.status().as_u16(), r.json().await.unwrap())
}

#[tokio::test]
async fn unset_token_hides_the_route() {
    let server = start(test_config()).await;
    assert_eq!(
        admin(&server, Some(&format!("Bearer {TOKEN}"))).await,
        (404, json!({ "error": "not found" }))
    );
}

#[tokio::test]
async fn wrong_or_missing_token_is_401() {
    let mut cfg = test_config();
    cfg.admin_token = Some(TOKEN.into());
    let server = start(cfg).await;
    let unauthorized = (401, json!({ "error": "unauthorized" }));
    assert_eq!(admin(&server, None).await, unauthorized);
    assert_eq!(admin(&server, Some("Bearer nope")).await, unauthorized);
    assert_eq!(
        admin(&server, Some(&format!("Bearer {TOKEN}x"))).await,
        unauthorized
    );
    assert_eq!(admin(&server, Some(TOKEN)).await, unauthorized);
    assert_eq!(
        admin(&server, Some(&format!("Basic {TOKEN}"))).await,
        unauthorized
    );
    assert_eq!(
        admin(&server, Some(&format!("bearer {TOKEN}"))).await.0,
        200
    );
}

#[tokio::test]
async fn lists_instances_and_counters() {
    let mut cfg = test_config();
    cfg.admin_token = Some(TOKEN.into());
    let server = start(cfg).await;
    let a = key_a();
    let ka = a.clone();
    let _mm = FakeMm::builder(&a, "i1")
        .on_rfq(move |req| Action::Reply(quote(req, &ka, "900", "q1")))
        .on_sign(|req| Action::Reply(sign_ok(req, "Y29zaWduZWQ=", None)))
        .connect(&server)
        .await;
    // MM B only has its rfq socket up: listed, not eligible.
    let (_rfq_only, welcome) = dial(
        &server,
        "/ws/rfq",
        &HelloSpec::new(&key_b(), "b1", paytience_backend::protocol::Channel::Rfq),
    )
    .await;
    assert_eq!(welcome["type"], "welcome");
    server.wait_fresh(1).await;

    let (tx, nonce) = tx_case("open_position_sol");
    assert_eq!(server.rfq("web-1", &nonce).await.0, 200);
    assert_eq!(server.sign("web-sign-1", &tx).await.0, 200);

    let (status, body) = admin(&server, Some(&format!("Bearer {TOKEN}"))).await;
    assert_eq!(status, 200, "{body}");
    let mms = body["mms"].as_array().unwrap();
    assert_eq!(mms.len(), 2);

    let alpha = &mms[0];
    assert_eq!(
        (&alpha["mm_pubkey"], &alpha["name"], &alpha["status"]),
        (&json!(a.pubkey), &json!("alpha"), &json!("approved"))
    );
    assert_eq!(alpha["approved"]["cap_total_usd"], 20000);
    assert_eq!(alpha["approved"]["asset_caps_usd"]["SOL"], 20000);
    assert_eq!(alpha["requested"], alpha["approved"]);
    // The co-signed 0.5 SOL at 250 USDC is reserved until a scan sees it (§5.6).
    assert_eq!(
        alpha["usage"],
        json!({ "known": true, "total_usd": 125, "by_asset_usd": { "SOL": 125 }, "reserved_usd": 125 })
    );
    let inst = &alpha["instances"].as_array().unwrap()[0];
    assert_eq!(inst["instance_id"], "i1");
    assert_eq!(inst["rfq_connected"], true);
    assert_eq!(inst["sign_connected"], true);
    assert_eq!(inst["eligible"], true);
    assert!(inst["connected_since_ms"].as_u64().unwrap() > 0);
    assert!(
        inst["last_health_ms"].as_u64().unwrap() >= inst["connected_since_ms"].as_u64().unwrap()
    );
    assert_eq!(inst["health_ok"], true);
    assert_eq!(inst["dry_run"], false);
    for (k, v) in [
        ("rfqs_asked", 1),
        ("quotes", 1),
        ("declines", 0),
        ("timeouts", 0),
        ("wins", 1),
        ("signs", 1),
        ("sign_rejections", 0),
        ("sign_timeouts", 0),
    ] {
        assert_eq!(alpha[k], v, "{k}");
    }

    let bravo = &mms[1];
    assert_eq!(bravo["name"], "bravo");
    let inst = &bravo["instances"].as_array().unwrap()[0];
    assert_eq!(inst["instance_id"], "b1");
    assert_eq!(
        (
            &inst["rfq_connected"],
            &inst["sign_connected"],
            &inst["eligible"]
        ),
        (&json!(true), &json!(false), &json!(false))
    );
    assert_eq!(bravo["rfqs_asked"], 0, "not eligible, not asked");
}
