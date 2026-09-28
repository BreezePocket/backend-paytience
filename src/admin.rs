//! `GET /admin/mms` (contract §2.9): every allowlisted MM with its connected instances
//! and per-pubkey counters, behind `Authorization: Bearer <ADMIN_TOKEN>`.

use std::hint::black_box;

use axum::http::{HeaderMap, header};
use serde_json::{Value, json};

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

/// The listing: allowlist order, instances oldest first.
pub fn list_mms(app: &App) -> Value {
    let fresh = app.config.health_fresh;
    let mms: Vec<Value> = app
        .config
        .allowlist
        .entries()
        .iter()
        .map(|entry| {
            let mut instances: Vec<(u64, String, Value)> = app.registry.with_instances(|all| {
                all.filter(|i| i.mm_pubkey == entry.pubkey)
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
            let c = app.registry.counters(&entry.pubkey);
            json!({
                "pubkey": entry.pubkey,
                "name": entry.name,
                "instances": instances.into_iter().map(|(_, _, v)| v).collect::<Vec<_>>(),
                "rfqs_asked": c.rfqs_asked,
                "quotes": c.quotes,
                "declines": c.declines,
                "timeouts": c.timeouts,
                "wins": c.wins,
                "signs": c.signs,
                "sign_rejections": c.sign_rejections,
                "sign_timeouts": c.sign_timeouts,
            })
        })
        .collect();
    json!({ "mms": mms })
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
