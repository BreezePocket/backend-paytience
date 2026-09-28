//! `POST /sign` (contract §2.7): route a user-signed transaction to the MM that pays
//! its fee, wait for the co-signature, and broadcast it unless the desk already did.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::conn::{Conn, RequestError};
use crate::protocol::{id_string, new_id, sign_rejection};
use crate::solana_rpc::RpcError;
use crate::state::App;
use crate::stats::Stats;
use crate::tx::parse_open_tx;

/// Handles a parsed request body; returns the HTTP status and typed body.
pub async fn run(app: &App, body: &Value) -> (u16, Value) {
    let request_id = id_string(body.get("request_id")).unwrap_or_default();
    let reject = |status: u16, reason: String| (status, sign_rejection(&request_id, reason));

    let Some(tx_base64) = body
        .get("tx_base64")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return reject(400, "invalid transaction: tx_base64 is required".into());
    };
    let parsed = match parse_open_tx(tx_base64, &app.config.program_id) {
        Ok(p) => p,
        Err(why) => return reject(400, format!("invalid transaction: {why}")),
    };

    let short: String = parsed.fee_payer.chars().take(8).collect();
    let not_connected = format!("market maker {short} is not connected");
    let candidates = route(app, &parsed.fee_payer, parsed.nonce);
    if candidates.is_empty() {
        return reject(422, not_connected);
    }

    let backend_id = new_id("sig");
    let frame = json!({ "type": "sign_request", "request_id": backend_id, "tx_base64": tx_base64 });
    let mut outcome = Err(RequestError::NotSent);
    let mut used: Option<&Arc<Conn>> = None;
    for conn in &candidates {
        used = Some(conn);
        outcome = conn
            .request(&backend_id, &frame, app.config.sign_timeout)
            .await;
        // Only a connection that was already gone is skipped; once sent, we wait on it.
        if outcome != Err(RequestError::NotSent) {
            break;
        }
    }
    let mm = used.map(|c| c.mm_pubkey.clone()).unwrap_or_default();

    let reply = match outcome {
        Ok(reply) => reply,
        Err(RequestError::Timeout) => {
            Stats::inc(&app.stats.sign_timeouts);
            app.registry.count(&mm, |c| c.sign_timeouts += 1);
            return reject(504, "market maker did not respond in time".into());
        }
        // Every candidate closed before we could send: same as none connected.
        Err(RequestError::NotSent) => return reject(422, not_connected),
        Err(e) => {
            // Disconnected mid-request: no answer, like a timeout (§1.3 fails it now).
            Stats::inc(&app.stats.sign_timeouts);
            app.registry.count(&mm, |c| c.sign_timeouts += 1);
            return reject(504, e.to_string());
        }
    };

    match reply.get("type").and_then(Value::as_str) {
        Some("sign_rejection") => {
            Stats::inc(&app.stats.sign_rejections);
            app.registry.count(&mm, |c| c.sign_rejections += 1);
            let reason = reply
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("rejected")
                .to_string();
            reject(422, reason)
        }
        Some("sign_response") => {
            let Some(signed) = reply
                .get("tx_base64")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
            else {
                Stats::inc(&app.stats.sign_rejections);
                app.registry.count(&mm, |c| c.sign_rejections += 1);
                return reject(422, "market maker returned no transaction".into());
            };
            app.registry.count(&mm, |c| c.signs += 1);
            let desk_signature = reply
                .get("signature")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty());
            let signature = match desk_signature {
                Some(sig) => Some(sig.to_string()),
                None if app.config.broadcast => match app.rpc.send_transaction(signed).await {
                    Ok(sig) => {
                        Stats::inc(&app.stats.broadcasts);
                        tracing::info!(signature = %sig, "broadcast co-signed transaction");
                        Some(sig)
                    }
                    Err(RpcError::Rpc { message, .. }) => {
                        Stats::inc(&app.stats.sign_rejections);
                        return reject(422, format!("broadcast failed: {message}"));
                    }
                    Err(e) => {
                        // Unknown outcome: hand the co-signed tx back; the dashboard broadcasts it.
                        tracing::warn!("broadcast outcome unknown, returning unsent: {e}");
                        None
                    }
                },
                None => None,
            };
            Stats::inc(&app.stats.signs);
            let mut out =
                json!({ "type": "sign_response", "request_id": request_id, "tx_base64": signed });
            if let (Some(sig), Some(obj)) = (signature, out.as_object_mut()) {
                obj.insert("signature".into(), json!(sig));
            }
            (200, out)
        }
        _ => {
            Stats::inc(&app.stats.sign_rejections);
            app.registry.count(&mm, |c| c.sign_rejections += 1);
            reject(422, "market maker returned an invalid response".into())
        }
    }
}

/// §2.7 step 2: the remembered winner's instance first (if it is the fee payer), then
/// every other `ws/sign` of the fee payer, most recently authenticated first.
pub fn route(app: &App, fee_payer: &str, nonce: u64) -> Vec<Arc<Conn>> {
    let mut out: Vec<Arc<Conn>> = Vec::new();
    if let Some(w) = app.winners.get(nonce).filter(|w| w.mm_pubkey == fee_payer)
        && let Some(conn) = app.registry.sign_conn(&w.mm_pubkey, &w.instance_id)
    {
        out.push(conn);
    }
    for conn in app.registry.sign_conns_for(fee_payer) {
        if !out.iter().any(|c| c.id == conn.id) {
            out.push(conn);
        }
    }
    out
}
