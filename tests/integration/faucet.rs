//! §2.8 faucet through `GET/POST /faucet`, against a scripted mock JSON-RPC server.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, SigningKey, Verifier};
use paytience_backend::Config;
use paytience_backend::faucet::{ATA_PROGRAM_ID, TOKEN_PROGRAM_ID};
use serde_json::{Value, json};
use solana_transaction::Transaction;

use crate::harness::*;

/// The faucet authority: seed [7; 32].
const AUTHORITY: &str = "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB";
/// A test asset mint (Keypair.fromSeed([9; 32])).
const TSLA_MINT: &str = "J2xccRtuG43drESLYznHhLhQkLTdfepcKYbiQ9BsJVaf";
/// spl-token's getAssociatedTokenAddressSync for (user, USDC) and (user, TSLA_MINT).
const USER_USDC_ATA: &str = "BW6GAT1EsPR5sPEzLefSBhGSbfpkJmGG1M9ShhQZnQMn";
const USER_TSLA_ATA: &str = "FUoM9ABbX4do8UDZaffPR3R4WHZMpBXb9sPS3tPtsdWr";
const SIG: &str = "FaucetMintSig";
const AIRDROP_SIG: &str = "AirdropSig";

fn blockhash() -> String {
    bs58::encode([3u8; 32]).into_string()
}

/// A cluster that accepts everything; statuses go null → processed → confirmed.
fn happy(method: &str, _params: &Value, seen: usize) -> RpcReply {
    match method {
        "getLatestBlockhash" => RpcReply::Result(
            json!({ "context": { "slot": 1 }, "value": { "blockhash": blockhash(), "lastValidBlockHeight": 100 } }),
        ),
        "sendTransaction" => RpcReply::Result(json!(SIG)),
        "getSignatureStatuses" => {
            let status = match seen {
                0 => Value::Null,
                1 => {
                    json!({ "slot": 5, "confirmations": 0, "err": null, "confirmationStatus": "processed" })
                }
                _ => {
                    json!({ "slot": 5, "confirmations": null, "err": null, "confirmationStatus": "confirmed" })
                }
            };
            RpcReply::Result(json!({ "context": { "slot": 5 }, "value": [status] }))
        }
        "requestAirdrop" => RpcReply::Result(json!(AIRDROP_SIG)),
        other => RpcReply::Error(format!("unexpected method {other}")),
    }
}

static KEYFILES: AtomicUsize = AtomicUsize::new(0);

/// Writes the authority keypair as a Solana CLI JSON file (read once at startup).
fn keypair_file() -> PathBuf {
    let signing = SigningKey::from_bytes(&[7; 32]);
    let mut bytes = signing.to_bytes().to_vec();
    bytes.extend_from_slice(signing.verifying_key().as_bytes());
    let path = std::env::temp_dir().join(format!(
        "paytience-faucet-{}-{}.json",
        std::process::id(),
        KEYFILES.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, serde_json::to_string(&bytes).unwrap()).unwrap();
    path
}

fn faucet_config(rpc: &MockRpc) -> Config {
    let mut cfg = test_config();
    cfg.solana_rpc = rpc.url.clone();
    cfg.faucet_keypair = Some(keypair_file());
    cfg.faucet_poll_interval = Duration::from_millis(10);
    cfg.faucet_confirm_timeout = Duration::from_secs(2);
    cfg
}

async fn faucet_server(rpc: &MockRpc) -> TestServer {
    let cfg = faucet_config(rpc);
    let path = cfg.faucet_keypair.clone().unwrap();
    let server = start(cfg).await;
    std::fs::remove_file(path).unwrap();
    server
}

async fn claim(server: &TestServer, body: Value) -> (u16, Value) {
    server.post("/faucet", &body).await
}

/// The transaction the faucet sent, checked against the wire format expectations.
fn sent_tx(rpc: &MockRpc, index: usize) -> Transaction {
    let send = &rpc.calls_to("sendTransaction")[index];
    assert_eq!(
        send["params"][1],
        json!({ "encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed" })
    );
    let wire = B64.decode(send["params"][0].as_str().unwrap()).unwrap();
    wincode::deserialize_exact(&wire).unwrap()
}

/// `(program, data, [(account, signer, writable)])` per instruction.
type Decoded = Vec<(String, Vec<u8>, Vec<(String, bool, bool)>)>;

fn decode(tx: &Transaction) -> Decoded {
    let m = &tx.message;
    let h = &m.header;
    let n = m.account_keys.len();
    let signed = usize::from(h.num_required_signatures);
    let writable = |i: usize| {
        if i < signed {
            i < signed - usize::from(h.num_readonly_signed_accounts)
        } else {
            i < n - usize::from(h.num_readonly_unsigned_accounts)
        }
    };
    m.instructions
        .iter()
        .map(|ix| {
            let accounts = ix
                .accounts
                .iter()
                .map(|a| {
                    let i = usize::from(*a);
                    (m.account_keys[i].to_string(), i < signed, writable(i))
                })
                .collect();
            (
                m.account_keys[usize::from(ix.program_id_index)].to_string(),
                ix.data.clone(),
                accounts,
            )
        })
        .collect()
}

fn acct(k: &str, signer: bool, writable: bool) -> (String, bool, bool) {
    (k.to_string(), signer, writable)
}

/// Expected CreateIdempotent + MintTo for `owner` / `mint` / `ata` / base units. Flags
/// are message-level (a compiled message merges them per account): the mint is writable
/// for MintTo and the authority is the writable fee payer, in both instructions.
fn expected_ixs(owner: &str, mint: &str, ata: &str, base: u64) -> Decoded {
    let mut mint_data = vec![7u8];
    mint_data.extend_from_slice(&base.to_le_bytes());
    vec![
        (
            ATA_PROGRAM_ID.to_string(),
            vec![1],
            vec![
                acct(AUTHORITY, true, true),
                acct(ata, false, true),
                acct(owner, false, false),
                acct(mint, false, true),
                acct("11111111111111111111111111111111", false, false),
                acct(&TOKEN_PROGRAM_ID.to_string(), false, false),
            ],
        ),
        (
            TOKEN_PROGRAM_ID.to_string(),
            mint_data,
            vec![
                acct(mint, false, true),
                acct(ata, false, true),
                acct(AUTHORITY, true, true),
            ],
        ),
    ]
}

fn assert_signed_by_authority(tx: &Transaction) {
    let m = &tx.message;
    assert_eq!(m.account_keys[0].to_string(), AUTHORITY, "fee payer");
    assert_eq!(m.header.num_required_signatures, 1);
    assert_eq!(m.recent_blockhash.to_string(), blockhash());
    let vk = SigningKey::from_bytes(&[7; 32]).verifying_key();
    let sig = Signature::from_bytes(tx.signatures[0].as_array());
    vk.verify(&tx.message_data(), &sig).unwrap();
}

#[tokio::test]
async fn disabled_faucet_is_404() {
    let server = start(test_config()).await;
    let disabled = json!({ "error": "faucet disabled" });
    assert_eq!(server.get("/faucet").await, (404, disabled.clone()));
    assert_eq!(
        claim(&server, json!({ "pubkey": user_pubkey() })).await,
        (404, disabled)
    );
}

#[tokio::test]
async fn get_describes_the_faucet() {
    let rpc = scripted_rpc(happy).await;
    let server = faucet_server(&rpc).await;
    assert_eq!(
        server.get("/faucet").await,
        (
            200,
            json!({ "enabled": true, "amount_usdc": 1000, "asset_usd": 1000, "cooldown_ms": 3600000, "usdc_mint": usdc_mint() })
        )
    );
}

#[tokio::test]
async fn usdc_claim_mints_and_airdrops() {
    let rpc = scripted_rpc(happy).await;
    let server = faucet_server(&rpc).await;
    let user = user_pubkey();
    let (status, body) = claim(&server, json!({ "pubkey": user })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        body,
        json!({ "pubkey": user, "asset": "USDC", "amount": 1000, "usdc_amount": 1000, "signature": SIG,
                "usdc_signature": SIG, "sol_airdrop_signature": AIRDROP_SIG })
    );

    assert_eq!(
        rpc.calls_to("getLatestBlockhash")[0]["params"],
        json!([{ "commitment": "confirmed" }])
    );
    let tx = sent_tx(&rpc, 0);
    assert_signed_by_authority(&tx);
    assert_eq!(
        decode(&tx),
        expected_ixs(&user, &usdc_mint(), USER_USDC_ATA, 1_000_000_000)
    );
    let statuses = rpc.calls_to("getSignatureStatuses");
    assert_eq!(statuses.len(), 3, "polled until confirmed");
    assert_eq!(
        statuses[0]["params"],
        json!([[SIG], { "searchTransactionHistory": false }])
    );
    assert_eq!(
        rpc.calls_to("requestAirdrop")[0]["params"],
        json!([user, 500_000_000u64])
    );

    // "usdc" (any case) is the same claim, and is now cooling down.
    let (status, body) = claim(&server, json!({ "pubkey": user, "asset": "usdc" })).await;
    assert_eq!(
        (status, body),
        (
            429,
            json!({ "error": "faucet cooldown: try again in 60 min" })
        )
    );
    assert_eq!(rpc.calls_to("sendTransaction").len(), 1);
}

#[tokio::test]
async fn failed_send_releases_the_cooldown() {
    let rpc = scripted_rpc(|method, params, seen| match method {
        "sendTransaction" if seen == 0 => {
            RpcReply::Error("Transaction simulation failed: Blockhash not found".into())
        }
        _ => happy(method, params, seen),
    })
    .await;
    let server = faucet_server(&rpc).await;
    let body = json!({ "pubkey": user_pubkey() });
    let (status, err) = claim(&server, body.clone()).await;
    assert_eq!(
        (status, err),
        (
            502,
            json!({ "error": "faucet mint failed: Transaction simulation failed: Blockhash not found" })
        )
    );
    assert!(
        rpc.calls_to("requestAirdrop").is_empty(),
        "no airdrop after a failed mint"
    );
    let (status, ok) = claim(&server, body).await;
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["signature"], SIG);
}

#[tokio::test]
async fn transactions_failed_on_chain_are_502_and_release_the_cooldown() {
    let rpc = scripted_rpc(|method, params, seen| match method {
        "getSignatureStatuses" => RpcReply::Result(json!({ "context": { "slot": 5 }, "value": [
            { "slot": 5, "confirmations": 0, "err": { "InstructionError": [1, { "Custom": 5 }] }, "confirmationStatus": "confirmed" }
        ] })),
        _ => happy(method, params, seen),
    })
    .await;
    let server = faucet_server(&rpc).await;
    let (status, body) = claim(&server, json!({ "pubkey": user_pubkey() })).await;
    assert_eq!(status, 502);
    let msg = body["error"].as_str().unwrap();
    assert!(
        msg.starts_with(&format!("faucet mint failed: transaction {SIG} failed: ")),
        "{msg}"
    );
    assert!(body.get("signature").is_none());
    // A definite failure frees the slot: the next attempt reaches the cluster again.
    claim(&server, json!({ "pubkey": user_pubkey() })).await;
    assert_eq!(rpc.calls_to("sendTransaction").len(), 2);
}

/// Mint outcome unknown: a 502 that keeps the cooldown, so a retry cannot double-mint.
/// `signature` defaults to the signature of the transaction that was sent.
async fn assert_outcome_unknown(rpc: &MockRpc, server: &TestServer, signature: Option<&str>) {
    let body = json!({ "pubkey": user_pubkey() });
    let (status, err) = claim(server, body.clone()).await;
    let tx = sent_tx(rpc, 0);
    assert_signed_by_authority(&tx);
    let signature = signature.map_or_else(|| tx.signatures[0].to_string(), str::to_string);
    assert_eq!(
        (status, err),
        (
            502,
            json!({ "error": "faucet mint outcome unknown; try again after the cooldown", "signature": signature })
        )
    );
    assert!(rpc.calls_to("requestAirdrop").is_empty());
    let (status, err) = claim(server, body).await;
    assert_eq!(
        (status, err),
        (
            429,
            json!({ "error": "faucet cooldown: try again in 60 min" })
        )
    );
    assert_eq!(rpc.calls_to("sendTransaction").len(), 1);
}

#[tokio::test]
async fn unconfirmed_mint_keeps_the_cooldown() {
    let rpc = scripted_rpc(|method, params, seen| match method {
        "getSignatureStatuses" => {
            RpcReply::Result(json!({ "context": { "slot": 5 }, "value": [null] }))
        }
        _ => happy(method, params, seen),
    })
    .await;
    let mut cfg = faucet_config(&rpc);
    cfg.faucet_confirm_timeout = Duration::from_millis(200);
    let keyfile = cfg.faucet_keypair.clone().unwrap();
    let server = start(cfg).await;
    std::fs::remove_file(keyfile).unwrap();
    assert_outcome_unknown(&rpc, &server, Some(SIG)).await;
}

#[tokio::test]
async fn throttled_send_releases_the_cooldown() {
    // HTTP 429 means a rate limiter refused the request before the node saw it, so the
    // mint did not happen and the wallet may retry at once.
    let rpc = scripted_rpc(|method, params, seen| match method {
        "sendTransaction" if seen == 0 => RpcReply::Status(429, "Too many requests".into()),
        _ => happy(method, params, seen),
    })
    .await;
    let server = faucet_server(&rpc).await;
    let body = json!({ "pubkey": user_pubkey() });
    let (status, err) = claim(&server, body.clone()).await;
    assert_eq!(
        (status, err),
        (
            502,
            json!({ "error": "faucet mint failed: rpc busy; try again shortly" })
        )
    );
    let (status, ok) = claim(&server, body).await;
    assert_eq!(status, 200, "{ok}");
    assert_eq!(ok["signature"], SIG);
}

#[tokio::test]
async fn failing_send_keeps_the_cooldown() {
    // HTTP 5xx is a transport failure even with a JSON-RPC error body: the node may
    // have taken the transaction, so the faucet reports its own signature.
    for status in [500, 503] {
        let rpc = scripted_rpc(move |method, params, seen| match method {
            "sendTransaction" => RpcReply::Status(status, "Too many requests".into()),
            _ => happy(method, params, seen),
        })
        .await;
        let server = faucet_server(&rpc).await;
        assert_outcome_unknown(&rpc, &server, None).await;
    }
}

#[tokio::test]
async fn rpc_transport_errors_never_reveal_the_rpc_url() {
    // Bind then drop: nothing listens on this port any more.
    let dead = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap()
        .local_addr()
        .unwrap();
    let rpc = scripted_rpc(happy).await;
    let mut cfg = faucet_config(&rpc);
    cfg.solana_rpc = format!("http://{dead}/v2/SECRETPATHKEY?api-key=SECRETQUERYKEY");
    let keyfile = cfg.faucet_keypair.clone().unwrap();
    let server = start(cfg).await;
    std::fs::remove_file(keyfile).unwrap();
    let r = server
        .http
        .post(server.url("/faucet"))
        .json(&json!({ "pubkey": user_pubkey() }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 502);
    let text = r.text().await.unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&text).unwrap(),
        json!({ "error": "faucet mint failed: rpc unavailable" })
    );
    for secret in [
        "SECRETPATHKEY",
        "SECRETQUERYKEY",
        "/v2",
        "api-key",
        "for url",
    ] {
        assert!(!text.contains(secret), "{secret} leaked: {text}");
    }
    // Nothing was sent, so the slot was released.
    let (status, _) = claim(&server, json!({ "pubkey": user_pubkey() })).await;
    assert_eq!(status, 502);
}

#[tokio::test]
async fn airdrop_failure_is_still_200() {
    let rpc = scripted_rpc(|method, params, seen| match method {
        "requestAirdrop" => RpcReply::Error("airdrop request limit reached".into()),
        _ => happy(method, params, seen),
    })
    .await;
    let server = faucet_server(&rpc).await;
    let (status, body) = claim(&server, json!({ "pubkey": user_pubkey() })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["signature"], SIG);
    assert_eq!(body["sol_airdrop_signature"], Value::Null);
}

#[tokio::test]
async fn bad_requests_are_400() {
    let rpc = scripted_rpc(happy).await;
    let server = faucet_server(&rpc).await;
    let bad_key = json!({ "error": "pubkey is not a valid public key" });
    assert_eq!(claim(&server, json!({})).await, (400, bad_key.clone()));
    assert_eq!(
        claim(&server, json!({ "pubkey": "nope" })).await,
        (400, bad_key)
    );
    assert_eq!(
        claim(&server, json!({ "pubkey": user_pubkey(), "asset": 5 }))
            .await
            .0,
        400
    );
    // No MM online: nothing is listed.
    assert_eq!(
        claim(
            &server,
            json!({ "pubkey": user_pubkey(), "asset": "TSLAon" })
        )
        .await,
        (400, json!({ "error": "TSLAon is not listed on chain" }))
    );
    // SOL is tradable but has no mint.
    let _mm = FakeMm::builder(&key_a(), "i1").connect(&server).await;
    server.wait_fresh(1).await;
    assert_eq!(
        claim(&server, json!({ "pubkey": user_pubkey(), "asset": "sol" })).await,
        (400, json!({ "error": "sol is not listed on chain" }))
    );
    assert!(rpc.calls_to("sendTransaction").is_empty());
}

/// A desk health listing TSLAon (tradable, minted) and NVDAon (no price yet).
fn health_with_assets() -> Value {
    let mut h = desk_health(&key_a(), 200.0, 0.0);
    let assets = h["assets"].as_array_mut().unwrap();
    assets.push(json!({ "asset": "TSLAon", "underlying": "TSLA", "venue": "pyth", "tradable": true, "mint": TSLA_MINT,
                        "decimals": 8, "expiry_time_of_day": null, "synthetic": false, "fresh": true, "spot": 412.37,
                        "atm_vol": 0.5, "expiries": 4 }));
    assets.push(json!({ "asset": "NVDAon", "underlying": "NVDA", "venue": "pyth", "tradable": true, "mint": TSLA_MINT,
                        "decimals": 8, "expiry_time_of_day": null, "synthetic": false, "fresh": false, "spot": null,
                        "atm_vol": null, "expiries": 0 }));
    h
}

#[tokio::test]
async fn listed_asset_claim_mints_its_token() {
    let rpc = scripted_rpc(happy).await;
    let server = faucet_server(&rpc).await;
    let _mm = FakeMm::builder(&key_a(), "i1")
        .health(health_with_assets())
        .connect(&server)
        .await;
    server.wait_fresh(1).await;
    let user = user_pubkey();

    let (status, body) = claim(&server, json!({ "pubkey": user, "asset": "tslaon" })).await;
    assert_eq!(status, 200, "{body}");
    // 1000 USD / 412.37 = 2.42500...; toPrecision(4) → 2.425.
    assert_eq!(
        body,
        json!({ "pubkey": user, "asset": "TSLAon", "amount": 2.425, "usdc_amount": 0, "signature": SIG,
                "usdc_signature": SIG, "sol_airdrop_signature": AIRDROP_SIG })
    );
    let tx = sent_tx(&rpc, 0);
    assert_signed_by_authority(&tx);
    assert_eq!(
        decode(&tx),
        expected_ixs(&user, TSLA_MINT, USER_TSLA_ATA, 242_500_000)
    );

    // The cooldown is per symbol: TSLAon is cooling down, USDC is not.
    assert_eq!(
        claim(&server, json!({ "pubkey": user, "asset": "TSLAON" }))
            .await
            .0,
        429
    );
    assert_eq!(claim(&server, json!({ "pubkey": user })).await.0, 200);

    assert_eq!(
        claim(&server, json!({ "pubkey": user, "asset": "NVDAon" })).await,
        (
            503,
            json!({ "error": "NVDAon has no price yet; try again shortly" })
        )
    );
}
