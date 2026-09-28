//! The public REST API (contract §2) and the router that also mounts the MM sockets.
//! Every route is registered in `build_router`; later features add theirs there.

use std::collections::HashMap;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::rejection::BytesRejection;
use axum::extract::{DefaultBodyLimit, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{Value, json};

use crate::admin::{self, Auth};
use crate::market::{self, BoardQuery, MarketError, first_values};
use crate::protocol::{error_body, rfq_decline, sign_rejection};
use crate::state::App;
use crate::{auction, sign, ws};

pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const SERVICE: &str = "paytience-backend";

pub fn build_router(app: Arc<App>) -> Router {
    Router::new()
        .route("/ws/rfq", get(ws::ws_rfq))
        .route("/ws/sign", get(ws::ws_sign))
        .route("/health", get(health))
        .route("/assets", get(assets))
        .route("/price", get(price))
        .route("/expiries", get(expiries))
        .route("/board", get(board))
        .route("/rfq", post(rfq))
        .route("/sign", post(sign_tx))
        .route("/faucet", get(faucet_info).post(faucet_claim))
        .route("/admin/mms", get(admin_mms))
        .fallback(not_found)
        .method_not_allowed_fallback(not_found)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // Inside CORS, so a 429 still carries the CORS headers.
        .layer(middleware::from_fn_with_state(app.clone(), rate_limit))
        .layer(middleware::from_fn(cors))
        .with_state(app)
}

/// §2.10: per-IP fixed-window limits (WebSocket upgrades included); OPTIONS passes.
async fn rate_limit(State(app): State<Arc<App>>, req: Request, next: Next) -> Response {
    if app.rate_limiter.check(&req) {
        next.run(req).await
    } else {
        json_response(429, &error_body("rate limited"))
    }
}

/// Contract §2 CORS headers on every response; `OPTIONS` short-circuits to 204.
async fn cors(req: Request, next: Next) -> Response {
    let mut res = if req.method() == Method::OPTIONS {
        StatusCode::NO_CONTENT.into_response()
    } else {
        next.run(req).await
    };
    let h = res.headers_mut();
    h.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("content-type, authorization"),
    );
    h.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    res
}

pub fn json_response(status: u16, body: &Value) -> Response {
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    (status, Json(body)).into_response()
}

fn error_response(e: &MarketError) -> Response {
    json_response(e.status, &error_body(e.message.clone()))
}

async fn not_found() -> Response {
    json_response(404, &error_body("not found"))
}

/// Reads a JSON body under the size limit; failures are untyped `{error}` bodies.
fn parse_body(body: Result<Bytes, BytesRejection>) -> Result<Value, (u16, Value)> {
    let bytes = body.map_err(|e| {
        let status = e.status();
        let msg = if status == StatusCode::PAYLOAD_TOO_LARGE {
            "body too large".to_string()
        } else {
            e.body_text()
        };
        (status.as_u16(), error_body(msg))
    })?;
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&bytes).map_err(|_| (400, error_body("body is not valid JSON")))
}

async fn health(State(app): State<Arc<App>>) -> Response {
    let snaps = market::fresh_snapshots(&app);
    let bodies: Vec<&Value> = snaps.iter().map(|(_, s)| s).collect();
    let merged = market::merge::merge_health(&bodies);
    // Older dashboards need a string here; prefer a fresh MM, else any online one.
    let mm_pubkey = snaps
        .first()
        .map(|(pk, _)| pk.clone())
        .or_else(|| {
            app.registry
                .market_representatives(app.config.health_fresh)
                .first()
                .map(|i| i.mm_pubkey.clone())
        })
        .unwrap_or_default();
    json_response(
        200,
        &json!({
            "ok": merged.ok,
            "service": SERVICE,
            "version": env!("CARGO_PKG_VERSION"),
            "mm_pubkey": mm_pubkey,
            "mm_count": snaps.len(),
            "program_id": app.config.program_id,
            "usdc_mint": app.config.usdc_mint,
            "dry_run": merged.dry_run,
            "price": merged.price,
            "assets": merged.assets,
            "exposure": merged.exposure,
            "stats": app.stats.snapshot(),
        }),
    )
}

async fn assets(State(app): State<Arc<App>>) -> Response {
    json_response(200, &market::assets(&app))
}

fn query_map(q: Query<Vec<(String, String)>>) -> HashMap<String, String> {
    first_values(q.0)
}

async fn price(State(app): State<Arc<App>>, q: Query<Vec<(String, String)>>) -> Response {
    let q = query_map(q);
    match market::price(&app, q.get("asset").map(String::as_str)).await {
        Ok(v) => json_response(200, &v),
        Err(e) => error_response(&e),
    }
}

async fn expiries(State(app): State<Arc<App>>, q: Query<Vec<(String, String)>>) -> Response {
    let q = query_map(q);
    match market::expiries(&app, q.get("asset").map(String::as_str)).await {
        Ok(v) => json_response(200, &v),
        Err(e) => error_response(&e),
    }
}

async fn board(State(app): State<Arc<App>>, q: Query<Vec<(String, String)>>) -> Response {
    let query = match BoardQuery::parse(&query_map(q)) {
        Ok(q) => q,
        Err(e) => return error_response(&e),
    };
    match market::board(&app, &query).await {
        Ok(v) => json_response(200, &v),
        Err(e) => error_response(&e),
    }
}

async fn rfq(State(app): State<Arc<App>>, body: Result<Bytes, BytesRejection>) -> Response {
    let body = match parse_body(body) {
        Ok(b) => b,
        Err((status, err)) => return json_response(status, &err),
    };
    match auction::validate_rfq(&body) {
        Err((rfq_id, reason)) => json_response(400, &rfq_decline(&rfq_id, reason)),
        Ok(valid) => {
            let (status, out) = auction::run(&app, &valid).await;
            json_response(status, &out)
        }
    }
}

async fn sign_tx(State(app): State<Arc<App>>, body: Result<Bytes, BytesRejection>) -> Response {
    let body = match parse_body(body) {
        Ok(b) => b,
        Err((status, err)) => return json_response(status, &err),
    };
    if !body.is_object() {
        return json_response(
            400,
            &sign_rejection("", "invalid transaction: sign_request must be an object"),
        );
    }
    let (status, out) = sign::run(&app, &body).await;
    json_response(status, &out)
}

async fn faucet_info(State(app): State<Arc<App>>) -> Response {
    match &app.faucet {
        Some(f) => json_response(200, &f.info(&app)),
        None => json_response(404, &error_body(crate::faucet::DISABLED)),
    }
}

async fn faucet_claim(
    State(app): State<Arc<App>>,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Some(faucet) = &app.faucet else {
        return json_response(404, &error_body(crate::faucet::DISABLED));
    };
    let body = match parse_body(body) {
        Ok(b) => b,
        Err((status, err)) => return json_response(status, &err),
    };
    match faucet.claim(&app, &body).await {
        Ok(v) => json_response(200, &v),
        Err(e) => {
            let mut body = error_body(e.message);
            if let (Some(sig), Some(obj)) = (e.signature, body.as_object_mut()) {
                obj.insert("signature".into(), json!(sig));
            }
            json_response(e.status, &body)
        }
    }
}

async fn admin_mms(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    match admin::authorize(&app, &headers) {
        Auth::Disabled => not_found().await,
        Auth::Denied => {
            let mut res = json_response(401, &error_body("unauthorized"));
            res.headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            res
        }
        Auth::Allowed => json_response(200, &admin::list_mms(&app)),
    }
}
