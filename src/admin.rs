//! The admin API (contract §2.9, §5.4), behind `Authorization: Bearer <ADMIN_TOKEN>`:
//! `GET /admin/mms` lists every registration with its connected instances, counters
//! and usage; approve / reject / suspend / delete change who may connect, at once.

use std::hint::black_box;

use axum::http::{HeaderMap, header};
use serde_json::{Value, json};

use crate::auth::CLOSE_NOT_APPROVED;
use crate::mmreg::RegError;
use crate::protocol::now_ms;
use crate::state::App;

/// Byte comparison whose time depends only on the length of `given`.
pub fn constant_time_eq(given: &[u8], expected: &[u8]) -> bool {
    let mut diff = given.len() ^ expected.len();
    for (i, b) in given.iter().enumerate() {
        diff |= usize::from(b ^ expected.get(i).copied().unwrap_or(!b));
    }
    black_box(diff) == 0
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Auth {
    /// `ADMIN_TOKEN` is unset: the route does not exist.
    Disabled,
    Denied,
    Allowed,
}

/// Checks `Authorization: Bearer <token>` (scheme case-insensitive).
pub fn authorize(app: &App, headers: &HeaderMap) -> Auth {
    let Some(expected) = app.config.admin_token.as_deref() else {
        return Auth::Disabled;
    };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim());
    match given {
        Some(token) if constant_time_eq(token.as_bytes(), expected.as_bytes()) => Auth::Allowed,
        _ => Auth::Denied,
    }
}

/// The listing: records in creation order, instances oldest first.
pub fn list_mms(app: &App) -> Value {
    let fresh = app.config.health_fresh;
    let mms: Vec<Value> = app
        .mms
        .list()
        .iter()
        .map(|record| {
            let mut instances: Vec<(u64, String, Value)> = app.registry.with_instances(|all| {
                all.filter(|i| i.mm_pubkey == record.mm_pubkey)
                    .map(|i| {
                        let health = i.health.as_ref();
                        let body = health.map(|h| &h.body);
                        let flag = |key: &str| {
                            body.and_then(|b| b.get(key))
                                .and_then(Value::as_bool)
                                .map_or(Value::Null, Value::Bool)
                        };
                        let v = json!({
                            "instance_id": i.instance_id,
                            "rfq_connected": i.rfq.is_some(),
                            "sign_connected": i.sign.is_some(),
                            "eligible": i.eligible(),
                            "connected_since_ms": i.connected_since_ms,
                            "last_health_ms": health.map(|h| h.at_ms),
                            "health_fresh": health.is_some_and(|h| h.at.elapsed() <= fresh),
                            "health_ok": flag("ok"),
                            "dry_run": flag("dry_run"),
                        });
                        (i.connected_since_ms, i.instance_id.clone(), v)
                    })
                    .collect()
            });
            instances.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
            let c = app.registry.counters(&record.mm_pubkey);
            let mut out = record.to_json();
            if let Some(obj) = out.as_object_mut() {
                let instances = instances.into_iter().map(|(_, _, v)| v).collect::<Vec<_>>();
                obj.insert("instances".into(), json!(instances));
                for (key, n) in [
                    ("rfqs_asked", c.rfqs_asked),
                    ("quotes", c.quotes),
                    ("declines", c.declines),
                    ("timeouts", c.timeouts),
                    ("wins", c.wins),
                    ("signs", c.signs),
                    ("sign_rejections", c.sign_rejections),
                    ("sign_timeouts", c.sign_timeouts),
                ] {
                    obj.insert(key.into(), json!(n));
                }
                obj.insert(
                    "usage".into(),
                    app.exposure.usage(&record.mm_pubkey).to_json(),
                );
            }
            out
        })
        .collect();
    json!({ "mms": mms })
}

/// Closes every live socket of an MM that may no longer connect (4003) and forgets
/// its usage view.
fn disconnect(app: &App, mm_pubkey: &str, reason: &str) {
    let closed = app
        .registry
        .close_pubkey(mm_pubkey, CLOSE_NOT_APPROVED, reason);
    app.exposure.forget(mm_pubkey);
    tracing::info!(mm = %mm_pubkey, closed, "market maker {reason}");
}

/// §5.4: status `approved` with the given (else the requested) caps, and a usage scan
/// right away so the MM is asked for quotes without waiting a full interval.
pub fn approve(app: &App, mm_pubkey: &str, body: &Value) -> Result<Value, RegError> {
    let record = app.mms.approve(mm_pubkey, body, now_ms())?;
    tracing::info!(mm = %mm_pubkey, name = %record.name, "market maker approved");
    app.exposure.trigger();
    Ok(record.to_json())
}

pub fn reject(app: &App, mm_pubkey: &str) -> Result<Value, RegError> {
    let record = app.mms.reject(mm_pubkey, now_ms())?;
    disconnect(app, mm_pubkey, "rejected");
    Ok(record.to_json())
}

pub fn suspend(app: &App, mm_pubkey: &str) -> Result<Value, RegError> {
    let record = app.mms.suspend(mm_pubkey, now_ms())?;
    disconnect(app, mm_pubkey, "suspended");
    Ok(record.to_json())
}

pub fn delete(app: &App, mm_pubkey: &str) -> Result<Value, RegError> {
    app.mms.delete(mm_pubkey)?;
    disconnect(app, mm_pubkey, "deleted");
    Ok(json!({ "mm_pubkey": mm_pubkey, "deleted": true }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_is_equality() {
        assert!(constant_time_eq(b"s3cret", b"s3cret"));
        assert!(!constant_time_eq(b"s3cres", b"s3cret"));
        assert!(!constant_time_eq(b"s3cre", b"s3cret"));
        assert!(!constant_time_eq(b"s3crett", b"s3cret"));
        assert!(!constant_time_eq(b"", b"s3cret"));
        assert!(constant_time_eq(b"", b""));
        // A byte that is missing from `expected` never compares equal.
        assert!(!constant_time_eq(&[0], &[]));
    }
}
