//! §5.1-§5.5: applying, the admin actions, and who may connect.

use std::path::PathBuf;

use paytience_backend::protocol::Channel;
use reqwest::Method;
use serde_json::{Value, json};

use crate::harness::*;

/// An empty in-memory registry with the admin API on; no usage scan.
fn reg_config() -> paytience_backend::Config {
    let mut cfg = test_config();
    cfg.seed_mms = Vec::new();
    cfg.admin_token = Some(ADMIN_TOKEN.into());
    cfg
}

async fn apply(server: &TestServer, key: &MmKey, total: f64, assets: Value) -> (u16, Value) {
    server
        .post("/mm/apply", &application(key, "Acme MM", total, assets))
        .await
}

async fn status(server: &TestServer, key: &MmKey) -> (u16, Value) {
    server
        .get(&format!("/mm/status?pubkey={}", key.pubkey))
        .await
}

/// Dials and expects the hello to be refused; returns the close code.
async fn refused(server: &TestServer, key: &MmKey, channel: Channel) -> Option<u16> {
    let path = format!("/{}", channel.as_str());
    let (mut ws, reply) = dial(server, &path, &HelloSpec::new(key, "i1", channel)).await;
    assert_eq!(reply["type"], "error", "{reply}");
    read_until_close(&mut ws).await.0
}

/// Both channels of one instance, as raw sockets so close codes can be read.
async fn connect_raw(server: &TestServer, key: &MmKey, instance: &str) -> (Ws, Ws) {
    let (rfq, welcome) = dial(
        server,
        "/ws/rfq",
        &HelloSpec::new(key, instance, Channel::Rfq),
    )
    .await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    let (sign, welcome) = dial(
        server,
        "/ws/sign",
        &HelloSpec::new(key, instance, Channel::Sign),
    )
    .await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    (rfq, sign)
}

#[tokio::test]
async fn pending_cannot_connect_until_approved() {
    let server = start(reg_config()).await;
    let c = mm_key(7);

    // No record at all.
    assert_eq!(refused(&server, &c, Channel::Rfq).await, Some(4003));

    let (code, body) = apply(&server, &c, 20000.0, json!({ "SOL": 10000, "wbtc": 5000 })).await;
    assert_eq!(code, 200, "{body}");
    assert_eq!(body, json!({ "mm_pubkey": c.pubkey, "status": "pending" }));
    assert_eq!(
        status(&server, &c).await,
        (
            200,
            json!({ "mm_pubkey": c.pubkey, "status": "pending", "approved": null })
        )
    );
    assert_eq!(refused(&server, &c, Channel::Rfq).await, Some(4003));
    assert_eq!(refused(&server, &c, Channel::Sign).await, Some(4003));

    // Approve with no body fields: the requested caps become the approved ones.
    let (code, record) = server.approve(&c, &json!({})).await;
    assert_eq!(code, 200, "{record}");
    assert_eq!(record["status"], "approved");
    assert_eq!(
        record["approved"],
        json!({ "cap_total_usd": 20000, "asset_caps_usd": { "SOL": 10000, "WBTC": 5000 } })
    );
    assert_eq!(record["requested"], record["approved"]);

    // No restart: it connects now, and is welcomed by its registered name.
    let (_ws, welcome) = dial(&server, "/ws/rfq", &HelloSpec::new(&c, "i1", Channel::Rfq)).await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    assert_eq!(welcome["name"], "Acme MM");
}

#[tokio::test]
async fn approve_with_lowered_caps_and_reapply_keeps_them() {
    let server = start(reg_config()).await;
    let c = mm_key(7);
    apply(&server, &c, 20000.0, json!({ "SOL": 10000, "WBTC": 5000 })).await;

    let lowered = json!({ "cap_total_usd": 15000, "asset_caps_usd": { "sol": 10000 } });
    let (code, record) = server.approve(&c, &lowered).await;
    assert_eq!(code, 200, "{record}");
    let approved = json!({ "cap_total_usd": 15000, "asset_caps_usd": { "SOL": 10000 } });
    assert_eq!(record["approved"], approved);
    assert_eq!(
        record["requested"]["cap_total_usd"], 20000,
        "the request is kept as asked"
    );
    assert_eq!(
        status(&server, &c).await,
        (
            200,
            json!({ "mm_pubkey": c.pubkey, "status": "approved", "approved": approved })
        )
    );

    // A cap change request while approved: status and approved caps stay.
    let (code, body) = apply(&server, &c, 90000.0, json!({ "SOL": 90000, "TSLAon": 1 })).await;
    assert_eq!((code, &body["status"]), (200, &json!("approved")), "{body}");
    assert_eq!(status(&server, &c).await.1["approved"], approved);
    let (_, list) = server.admin(Method::GET, "/admin/mms", None).await;
    assert_eq!(
        list["mms"][0]["requested"],
        json!({ "cap_total_usd": 90000, "asset_caps_usd": { "SOL": 90000, "TSLAON": 1 } })
    );
    assert_eq!(list["mms"][0]["approved"], approved);

    // Given caps are validated like an application's.
    for bad in [
        json!({ "cap_total_usd": 0 }),
        json!({ "cap_total_usd": 100, "asset_caps_usd": { "SOL": 101 } }),
        json!({ "asset_caps_usd": {} }),
        json!({ "asset_caps_usd": { "S L": 1 } }),
    ] {
        let (code, body) = server.approve(&c, &bad).await;
        assert_eq!(code, 400, "{bad} -> {body}");
        assert!(body["error"].is_string());
    }
    assert_eq!(
        status(&server, &c).await.1["approved"],
        approved,
        "nothing changed"
    );

    // Approving again takes the new request.
    let (_, record) = server.approve(&c, &json!({})).await;
    assert_eq!(record["approved"]["cap_total_usd"], 90000);
}

#[tokio::test]
async fn reject_suspend_and_delete_close_every_socket_with_4003() {
    for action in ["reject", "suspend", "delete"] {
        let server = start(reg_config()).await;
        let c = mm_key(7);
        let other = mm_key(8);
        for key in [&c, &other] {
            apply(&server, key, 1000.0, json!({ "SOL": 1000 })).await;
            assert_eq!(server.approve(key, &json!({})).await.0, 200);
        }
        // Two instances of the MM, plus a half-connected third, and another MM.
        let (mut rfq1, mut sign1) = connect_raw(&server, &c, "i1").await;
        let (mut rfq2, mut sign2) = connect_raw(&server, &c, "i2").await;
        let (mut rfq3, _) = dial(&server, "/ws/rfq", &HelloSpec::new(&c, "i3", Channel::Rfq)).await;
        let (_keep_rfq, _keep_sign) = connect_raw(&server, &other, "o1").await;
        server.wait_eligible(3).await;

        let path = format!("/admin/mms/{}", c.pubkey);
        let (code, body) = match action {
            "delete" => server.admin(Method::DELETE, &path, None).await,
            _ => {
                server
                    .admin(Method::POST, &format!("{path}/{action}"), None)
                    .await
            }
        };
        assert_eq!(code, 200, "{action}: {body}");

        for ws in [&mut rfq1, &mut sign1, &mut rfq2, &mut sign2, &mut rfq3] {
            assert_eq!(read_until_close(ws).await.0, Some(4003), "{action}");
        }
        // Gone from the live registry at once; the other MM is untouched.
        let eligible = server.app.registry.eligible();
        assert_eq!(eligible.len(), 1, "{action}");
        assert_eq!(eligible[0].mm_pubkey, other.pubkey);
        assert!(server.app.registry.sign_conns_for(&c.pubkey).is_empty());
        assert_eq!(
            refused(&server, &c, Channel::Rfq).await,
            Some(4003),
            "{action}"
        );

        let (code, st) = status(&server, &c).await;
        match action {
            "reject" => {
                assert_eq!(body["status"], "rejected");
                assert_eq!(
                    st,
                    json!({ "mm_pubkey": c.pubkey, "status": "rejected", "approved": null })
                );
            }
            "suspend" => {
                assert_eq!(st["status"], "suspended");
                assert_eq!(st["approved"]["cap_total_usd"], 1000, "caps are kept");
                // Approve again to restore, with the same caps.
                assert_eq!(server.approve(&c, &json!({})).await.0, 200);
                let _back = connect_raw(&server, &c, "i1").await;
            }
            _ => {
                assert_eq!(body, json!({ "mm_pubkey": c.pubkey, "deleted": true }));
                assert_eq!(code, 404);
                let (_, list) = server.admin(Method::GET, "/admin/mms", None).await;
                assert_eq!(list["mms"].as_array().unwrap().len(), 1);
            }
        }
    }
}

#[tokio::test]
async fn admin_actions_on_an_unknown_pubkey_are_404() {
    let server = start(reg_config()).await;
    let stranger = mm_key(9).pubkey;
    let unknown = (404, json!({ "error": "unknown market maker" }));
    for action in ["approve", "reject", "suspend"] {
        let path = format!("/admin/mms/{stranger}/{action}");
        assert_eq!(
            server.admin(Method::POST, &path, Some(&json!({}))).await,
            unknown
        );
    }
    let path = format!("/admin/mms/{stranger}");
    assert_eq!(server.admin(Method::DELETE, &path, None).await, unknown);
    assert_eq!(
        server
            .admin(Method::POST, "/admin/mms/not-a-key/approve", None)
            .await,
        unknown
    );
}

#[tokio::test]
async fn admin_actions_need_the_token() {
    let server = start(reg_config()).await;
    let c = mm_key(7);
    apply(&server, &c, 1000.0, json!({ "SOL": 1000 })).await;
    let approve = server.url(&format!("/admin/mms/{}/approve", c.pubkey));
    let delete = server.url(&format!("/admin/mms/{}", c.pubkey));

    let r = server
        .http
        .post(&approve)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = server
        .http
        .post(&approve)
        .header("authorization", "Bearer wrong")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let r = server.http.delete(&delete).send().await.unwrap();
    assert_eq!(r.status(), 401);
    assert_eq!(
        status(&server, &c).await.1["status"],
        "pending",
        "nothing happened"
    );

    // Without ADMIN_TOKEN the routes do not exist, so nobody can be approved.
    let mut cfg = reg_config();
    cfg.admin_token = None;
    let closed = start(cfg).await;
    apply(&closed, &c, 1000.0, json!({ "SOL": 1000 })).await;
    assert_eq!(
        closed.approve(&c, &json!({})).await,
        (404, json!({ "error": "not found" }))
    );
    assert_eq!(status(&closed, &c).await.1["status"], "pending");
}

#[tokio::test]
async fn status_endpoint() {
    let server = start(test_config()).await;
    let (code, body) = status(&server, &key_a()).await;
    assert_eq!(code, 200);
    // Never the name, the contact or the usage.
    let mut keys: Vec<&String> = body.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["approved", "mm_pubkey", "status"]);
    assert_eq!(body["status"], "approved");
    assert_eq!(body["approved"]["cap_total_usd"], 20000);

    let unknown = (404, json!({ "error": "unknown market maker" }));
    assert_eq!(status(&server, &mm_key(9)).await, unknown);
    assert_eq!(server.get("/mm/status").await, unknown);
    assert_eq!(server.get("/mm/status?pubkey=").await, unknown);
}

#[tokio::test]
async fn bad_applications_are_400() {
    let server = start(reg_config()).await;
    let c = mm_key(7);
    let good = application(&c, "Acme MM", 1000.0, json!({ "SOL": 1000 }));
    let server_ref = &server;
    let error = |body: Value| async move {
        let (code, out) = server_ref.post("/mm/apply", &body).await;
        assert_eq!(code, 400, "{out}");
        out["error"].as_str().unwrap().to_string()
    };

    // Signed by another key.
    let mut forged = application(&mm_key(8), "x", 1.0, json!({ "SOL": 1 }));
    forged["application"] = good["application"].clone();
    assert_eq!(error(forged).await, "bad signature");
    // The signature covers the exact string: re-serializing it breaks it.
    let mut edited = good.clone();
    edited["application"] = json!(
        good["application"]
            .as_str()
            .unwrap()
            .replace("Acme", "Evil")
    );
    assert_eq!(error(edited).await, "bad signature");
    assert!(
        error(json!({ "application": { "mm_pubkey": c.pubkey }, "signature": "x" }))
            .await
            .contains("JSON string")
    );
    assert!(error(json!({})).await.contains("JSON string"));
    // Valid signature, invalid content.
    assert!(
        error(application(&c, "Acme MM", 1000.0, json!({ "SOL": 1001 })))
            .await
            .contains("exceeds")
    );
    assert!(
        error(application(&c, " ", 1000.0, json!({ "SOL": 1 })))
            .await
            .contains("name")
    );
    assert!(
        error(application(&c, "Acme MM", 1000.0, json!({})))
            .await
            .contains("1-64")
    );

    assert_eq!(status(&server, &c).await.0, 404, "nothing was stored");
    assert_eq!(server.post("/mm/apply", &good).await.0, 200);
}

#[tokio::test]
async fn applications_share_the_faucet_rate_limit() {
    let mut cfg = reg_config();
    cfg.rate_limits.faucet = 2;
    let server = start(cfg).await;
    let body = application(&mm_key(7), "Acme MM", 1000.0, json!({ "SOL": 1000 }));
    assert_eq!(server.post("/mm/apply", &body).await.0, 200);
    assert_eq!(server.post("/mm/apply", &body).await.0, 200);
    assert_eq!(
        server.post("/mm/apply", &body).await,
        (429, json!({ "error": "rate limited" }))
    );
}

fn temp_registry() -> PathBuf {
    std::env::temp_dir()
        .join(format!("paytience-it-{}", uuid_like()))
        .join("data")
        .join("mm-registry.json")
}

fn uuid_like() -> String {
    paytience_backend::protocol::new_id("reg")
}

#[tokio::test]
async fn registry_survives_a_restart() {
    let path = temp_registry();
    let mut cfg = reg_config();
    cfg.mm_registry_path = Some(path.clone());
    let c = mm_key(7);
    let pending = mm_key(8);

    let mut first = start(cfg.clone()).await;
    apply(&first, &c, 20000.0, json!({ "SOL": 10000, "WBTC": 5000 })).await;
    let lowered = json!({ "cap_total_usd": 15000, "asset_caps_usd": { "SOL": 10000 } });
    assert_eq!(first.approve(&c, &lowered).await.0, 200);
    apply(&first, &pending, 500.0, json!({ "SOL": 500 })).await;
    let (_, before) = first.admin(Method::GET, "/admin/mms", None).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "the registry file is private");
    }
    first.shutdown();

    // A new process with the same file: same records, same order, no re-approval.
    let second = start(cfg).await;
    let (_, after) = second.admin(Method::GET, "/admin/mms", None).await;
    let records = |list: &Value| -> Vec<Value> {
        list["mms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| {
                json!([
                    m["mm_pubkey"],
                    m["name"],
                    m["contact"],
                    m["status"],
                    m["requested"],
                    m["approved"],
                    m["created_at_ms"],
                    m["updated_at_ms"]
                ])
            })
            .collect()
    };
    assert_eq!(records(&after), records(&before));
    assert_eq!(after["mms"][0]["mm_pubkey"], c.pubkey);
    assert_eq!(after["mms"][1]["status"], "pending");
    assert_eq!(
        status(&second, &c).await.1,
        json!({ "mm_pubkey": c.pubkey, "status": "approved",
                "approved": { "cap_total_usd": 15000, "asset_caps_usd": { "SOL": 10000 } } })
    );
    let (_ws, welcome) = dial(&second, "/ws/rfq", &HelloSpec::new(&c, "i1", Channel::Rfq)).await;
    assert_eq!(welcome["type"], "welcome", "{welcome}");
    assert_eq!(refused(&second, &pending, Channel::Rfq).await, Some(4003));

    std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
}

#[tokio::test]
async fn a_malformed_registry_file_fails_startup() {
    let path = temp_registry();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"{ not json").unwrap();
    let mut cfg = reg_config();
    cfg.mm_registry_path = Some(path.clone());
    let err = paytience_backend::App::new(cfg).err().unwrap();
    let text = format!("{err:#}");
    assert!(
        text.contains("MM_REGISTRY_PATH") && text.contains("malformed"),
        "{text}"
    );
    std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap()).unwrap();
}
