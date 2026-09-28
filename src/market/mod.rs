//! Market data over `market_request` (contract §1.5, §2.1-2.5): the per-connection
//! health poller, fan-out to MMs, TTL caches with request coalescing, and the query
//! parsing that mirrors the desk's REST server.

pub mod merge;

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::OnceCell;

use crate::conn::Conn;
use crate::protocol::{new_id, num};
use crate::registry::EligibleInstance;
use crate::state::App;
use merge::MmFailure;

pub const NO_MM_ONLINE: &str = "no market maker is online";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketError {
    pub status: u16,
    pub message: String,
}

impl MarketError {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub type MarketResult = Result<Arc<Value>, MarketError>;

/// Sends one `market_request` and unwraps its `market_response`.
pub async fn market_call(
    conn: &Conn,
    method: &str,
    params: &Value,
    timeout: Duration,
) -> Result<Value, MmFailure> {
    let request_id = new_id("mkt");
    let frame = json!({ "type": "market_request", "request_id": request_id, "method": method, "params": params });
    match conn.request(&request_id, &frame, timeout).await {
        Ok(reply) => parse_market_response(&reply),
        Err(e) => Err(MmFailure {
            message: e.to_string(),
            status: None,
        }),
    }
}

/// `{ ok: true, result }` → result; `{ ok: false, error, status }` → the desk's failure.
pub fn parse_market_response(reply: &Value) -> Result<Value, MmFailure> {
    let garbage = || MmFailure {
        message: "invalid market response".into(),
        status: None,
    };
    if reply.get("type").and_then(Value::as_str) != Some("market_response") {
        return Err(garbage());
    }
    match reply.get("ok").and_then(Value::as_bool) {
        Some(true) => reply.get("result").cloned().ok_or_else(garbage),
        Some(false) => {
            let message = reply
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("market maker error")
                .to_string();
            // The desk's err.statusCode, else 400.
            let status = reply
                .get("status")
                .and_then(Value::as_u64)
                .and_then(|s| u16::try_from(s).ok())
                .filter(|s| (400..=599).contains(s))
                .unwrap_or(400);
            Err(MmFailure {
                message,
                status: Some(status),
            })
        }
        None => Err(garbage()),
    }
}

/// Polls `health` over an rfq connection until it closes; the first poll is immediate.
pub async fn health_poller(app: Arc<App>, conn: Arc<Conn>) {
    loop {
        match market_call(&conn, "health", &json!({}), app.config.market_timeout).await {
            Ok(body) if body.is_object() => app.registry.set_health(&conn, body),
            Ok(_) => tracing::debug!(mm = %conn.name, "health result is not an object"),
            Err(e) => tracing::debug!(mm = %conn.name, error = %e.message, "health poll failed"),
        }
        tokio::select! {
            _ = tokio::time::sleep(app.config.health_poll) => {}
            _ = conn.closed() => return,
        }
    }
}

/// Asks every representative MM in parallel; results keep the MM order.
async fn fan_out(
    targets: &[EligibleInstance],
    method: &str,
    params: &Value,
    timeout: Duration,
) -> Vec<Result<Value, MmFailure>> {
    let calls = targets
        .iter()
        .map(|t| market_call(&t.rfq, method, params, timeout));
    futures_util::future::join_all(calls).await
}

// ---------------------------------------------------------------------------
// Cache with request coalescing

struct Slot {
    cell: OnceCell<(MarketResult, Instant)>,
}

/// TTL cache where concurrent identical requests share one in-flight fetch. Errors are
/// handed to everyone already waiting on that fetch but never cached.
pub struct Coalescer {
    ttl: Duration,
    slots: Mutex<HashMap<String, Arc<Slot>>>,
}

const MAX_CACHE_KEYS: usize = 1024;

impl Coalescer {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn slots(&self) -> MutexGuard<'_, HashMap<String, Arc<Slot>>> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn usable(&self, slot: &Slot) -> bool {
        match slot.cell.get() {
            None => true, // in flight: join it
            Some((Ok(_), at)) => at.elapsed() < self.ttl,
            Some((Err(_), _)) => false,
        }
    }

    pub async fn get<F, Fut>(&self, key: &str, fetch: F) -> MarketResult
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = MarketResult>,
    {
        let slot = {
            let mut slots = self.slots();
            match slots.get(key) {
                Some(s) if self.usable(s) => s.clone(),
                _ => {
                    if slots.len() >= MAX_CACHE_KEYS {
                        slots.retain(|_, s| self.usable(s));
                    }
                    let s = Arc::new(Slot {
                        cell: OnceCell::new(),
                    });
                    slots.insert(key.to_string(), s.clone());
                    s
                }
            }
        };
        // If the fetching caller goes away, the next waiter runs its own fetch.
        let (result, _) = slot
            .cell
            .get_or_init(|| async { (fetch().await, Instant::now()) })
            .await;
        if result.is_err() {
            let mut slots = self.slots();
            if slots.get(key).is_some_and(|s| Arc::ptr_eq(s, &slot)) {
                slots.remove(key);
            }
        }
        result.clone()
    }
}

// ---------------------------------------------------------------------------
// Query parsing, mirroring the desk's restServer.js

/// JS `Number(str)` for query values: trims, empty is 0, `Infinity`, 0x/0o/0b, else a
/// decimal literal; anything else is NaN.
pub fn js_number(raw: &str) -> f64 {
    let s = raw.trim();
    if s.is_empty() {
        return 0.0;
    }
    match s {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let radix = match s.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = s.get(2..).unwrap_or_default();
        return u128::from_str_radix(digits, radix).map_or(f64::NAN, |n| n as f64);
    }
    // Rust also parses "inf"/"nan"/"infinity"; JS does not.
    if !s
        .bytes()
        .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'+' | b'-'))
    {
        return f64::NAN;
    }
    s.parse::<f64>().unwrap_or(f64::NAN)
}

/// First value of each query key, like `URLSearchParams.get`.
pub fn first_values(pairs: Vec<(String, String)>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for (k, v) in pairs {
        out.entry(k).or_insert(v);
    }
    out
}

/// `/board` params to forward: only those present in the query.
#[derive(Debug, Clone, PartialEq)]
pub struct BoardQuery {
    pub asset: Option<String>,
    pub product: Option<String>,
    pub amount: Option<f64>,
    pub moneyness: Vec<f64>,
    pub max_days: Option<f64>,
    pub exact: Option<bool>,
}

impl BoardQuery {
    pub fn parse(q: &HashMap<String, String>) -> Result<Self, MarketError> {
        let finite = |key: &str| -> Result<Option<f64>, MarketError> {
            match q.get(key) {
                None => Ok(None),
                Some(v) => {
                    let n = js_number(v);
                    // JSON cannot carry NaN or Infinity; the desk would fail on them anyway.
                    if n.is_finite() {
                        Ok(Some(n))
                    } else {
                        Err(MarketError::new(
                            400,
                            format!("{key} must be a finite number"),
                        ))
                    }
                }
            }
        };
        let moneyness = q
            .get("moneyness")
            .map(|v| {
                v.split(',')
                    .map(js_number)
                    .filter(|m| m.is_finite() && *m > 0.0)
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            asset: q.get("asset").cloned(),
            product: q.get("product").cloned(),
            amount: finite("amount")?,
            moneyness,
            max_days: finite("max_days")?,
            exact: q.get("exact").map(|v| v == "true"),
        })
    }

    /// The `market_request` params.
    pub fn params(&self) -> Value {
        let mut p = Map::new();
        if let Some(a) = &self.asset {
            p.insert("asset".into(), json!(a));
        }
        if let Some(v) = &self.product {
            p.insert("product".into(), json!(v));
        }
        if let Some(v) = self.amount {
            p.insert("amount".into(), num(v));
        }
        // An empty list is the desk's default ladder, same as omitting it.
        if !self.moneyness.is_empty() {
            p.insert(
                "moneyness".into(),
                Value::Array(self.moneyness.iter().map(|m| num(*m)).collect()),
            );
        }
        if let Some(v) = self.max_days {
            p.insert("max_days".into(), num(v));
        }
        if let Some(v) = self.exact {
            p.insert("exact".into(), json!(v));
        }
        Value::Object(p)
    }

    /// Cache key: params sorted, asset lowercased, numbers in one canonical form.
    pub fn cache_key(&self) -> String {
        let mut parts: BTreeMap<String, String> = BTreeMap::new();
        if let Value::Object(p) = self.params() {
            for (k, v) in p {
                let s = match (k.as_str(), &v) {
                    ("asset", Value::String(a)) => a.to_lowercase(),
                    (_, Value::String(s)) => s.clone(),
                    (_, Value::Array(items)) => items
                        .iter()
                        .map(Value::to_string)
                        .collect::<Vec<_>>()
                        .join(","),
                    _ => v.to_string(),
                };
                parts.insert(k, s);
            }
        }
        parts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&")
    }
}

fn asset_params(asset: Option<&str>) -> Value {
    match asset {
        Some(a) => json!({ "asset": a }),
        None => json!({}),
    }
}

// ---------------------------------------------------------------------------
// Endpoint logic

/// `/price` (§2.3): one MM, the one with the freshest snapshot, returned verbatim.
pub async fn price(app: &App, asset: Option<&str>) -> MarketResult {
    let reps = app.registry.market_representatives();
    let fresh = app.config.health_fresh;
    let target = reps
        .iter()
        .filter_map(|r| r.fresh_health(fresh).map(|h| (r, h.at)))
        .max_by_key(|(_, at)| *at)
        .map(|(r, _)| r)
        // Right after a connect there is no snapshot yet; any online MM will do.
        .or_else(|| reps.iter().max_by_key(|r| r.rfq.id))
        .ok_or_else(|| MarketError::new(503, NO_MM_ONLINE))?;
    match market_call(
        &target.rfq,
        "price",
        &asset_params(asset),
        app.config.market_timeout,
    )
    .await
    {
        Ok(v) => Ok(Arc::new(v)),
        Err(f) => Err(merge::aggregate_failures(&[f])),
    }
}

/// `/expiries` (§2.4).
pub async fn expiries(app: &App, asset: Option<&str>) -> MarketResult {
    let key = asset.map(str::to_lowercase).unwrap_or_default();
    let params = asset_params(asset);
    app.expiries_cache
        .get(&key, || async move {
            let targets = app.registry.market_representatives();
            if targets.is_empty() {
                return Err(MarketError::new(503, NO_MM_ONLINE));
            }
            let results = fan_out(&targets, "expiries", &params, app.config.market_timeout).await;
            let (lists, failures) = split_results(results, |v| {
                v.get("expiries").and_then(Value::as_array).cloned()
            });
            if lists.is_empty() {
                return Err(merge::aggregate_failures(&failures));
            }
            let slices: Vec<&[Value]> = lists.iter().map(Vec::as_slice).collect();
            Ok(Arc::new(
                json!({ "expiries": merge::merge_expiries(&slices) }),
            ))
        })
        .await
}

/// `/board` (§2.5).
pub async fn board(app: &App, query: &BoardQuery) -> MarketResult {
    let params = query.params();
    app.board_cache
        .get(&query.cache_key(), || async move {
            let targets = app.registry.market_representatives();
            if targets.is_empty() {
                return Err(MarketError::new(503, NO_MM_ONLINE));
            }
            let results = fan_out(&targets, "board", &params, app.config.market_timeout).await;
            let mut boards: Vec<(&str, Value)> = Vec::new();
            let mut failures = Vec::new();
            for (t, r) in targets.iter().zip(results) {
                match r {
                    Ok(v) if v.is_object() => boards.push((t.mm_pubkey.as_str(), v)),
                    Ok(_) => failures.push(MmFailure {
                        message: "invalid market response".into(),
                        status: None,
                    }),
                    Err(f) => failures.push(f),
                }
            }
            let refs: Vec<(&str, &Value)> = boards.iter().map(|(pk, b)| (*pk, b)).collect();
            match merge::merge_boards(&refs) {
                Some(v) => Ok(Arc::new(v)),
                None => Err(merge::aggregate_failures(&failures)),
            }
        })
        .await
}

fn split_results<T>(
    results: Vec<Result<Value, MmFailure>>,
    extract: impl Fn(&Value) -> Option<T>,
) -> (Vec<T>, Vec<MmFailure>) {
    let mut ok = Vec::new();
    let mut failures = Vec::new();
    for r in results {
        match r.map(|v| extract(&v)) {
            Ok(Some(t)) => ok.push(t),
            Ok(None) => failures.push(MmFailure {
                message: "invalid market response".into(),
                status: None,
            }),
            Err(f) => failures.push(f),
        }
    }
    (ok, failures)
}

/// Fresh health snapshots, one per MM pubkey (§2.1 "fresh, eligible").
pub fn fresh_snapshots(app: &App) -> Vec<(String, Value)> {
    let fresh = app.config.health_fresh;
    app.registry
        .market_representatives()
        .into_iter()
        .filter_map(|r| {
            r.fresh_health(fresh)
                .map(|h| (r.mm_pubkey.clone(), h.body.clone()))
        })
        .collect()
}

/// `/assets` (§2.2), from the same fresh health snapshots `/health` uses.
pub fn assets(app: &App) -> Value {
    let snaps = fresh_snapshots(app);
    let lists: Vec<&[Value]> = snaps
        .iter()
        .map(|(_, s)| {
            s.get("assets")
                .and_then(Value::as_array)
                .map(Vec::as_slice)
                .unwrap_or(&[])
        })
        .collect();
    json!({ "assets": merge::merge_assets(&lists) })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        first_values(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn js_number_matches_js() {
        assert_eq!(js_number(""), 0.0);
        assert_eq!(js_number("  "), 0.0);
        assert_eq!(js_number(" 2.5 "), 2.5);
        assert_eq!(js_number("1e3"), 1000.0);
        assert_eq!(js_number(".5"), 0.5);
        assert_eq!(js_number("0x10"), 16.0);
        assert_eq!(js_number("-3"), -3.0);
        assert_eq!(js_number("Infinity"), f64::INFINITY);
        assert!(js_number("abc").is_nan());
        assert!(js_number("inf").is_nan());
        assert!(js_number("NaN").is_nan());
        assert!(js_number("1,2").is_nan());
        assert!(js_number("0xZZ").is_nan());
    }

    #[test]
    fn board_query_mirrors_the_desk() {
        let bq = BoardQuery::parse(&q(&[
            ("asset", "SOL"),
            ("moneyness", "1.05,abc,,-1,0,1.1,Infinity"),
            ("amount", "2"),
            ("exact", "TRUE"),
        ]))
        .unwrap();
        assert_eq!(bq.moneyness, vec![1.05, 1.1]);
        assert_eq!(bq.exact, Some(false), "exact is === 'true'");
        assert_eq!(bq.product, None, "the MM applies its own default product");
        assert_eq!(
            bq.params(),
            json!({ "asset": "SOL", "amount": 2, "moneyness": [1.05, 1.1], "exact": false })
        );

        let none = BoardQuery::parse(&q(&[])).unwrap();
        assert_eq!(none.params(), json!({}));
        let empty_m = BoardQuery::parse(&q(&[("moneyness", "")])).unwrap();
        assert_eq!(empty_m.params(), json!({}));

        let exact = BoardQuery::parse(&q(&[
            ("exact", "true"),
            ("max_days", "30"),
            ("product", "buy_sol"),
        ]))
        .unwrap();
        assert_eq!(
            exact.params(),
            json!({ "exact": true, "max_days": 30, "product": "buy_sol" })
        );

        assert_eq!(
            BoardQuery::parse(&q(&[("amount", "abc")]))
                .unwrap_err()
                .status,
            400
        );
        assert_eq!(
            BoardQuery::parse(&q(&[("max_days", "Infinity")]))
                .unwrap_err()
                .status,
            400
        );
    }

    #[test]
    fn board_cache_key_is_normalized() {
        let a = BoardQuery::parse(&q(&[
            ("product", "sell_sol"),
            ("asset", "SOL"),
            ("moneyness", "1.10,1.05"),
        ]))
        .unwrap();
        let b = BoardQuery::parse(&q(&[
            ("asset", "sol"),
            ("moneyness", "1.1, 1.050"),
            ("product", "sell_sol"),
        ]))
        .unwrap();
        assert_eq!(a.cache_key(), b.cache_key());
        assert_eq!(
            a.cache_key(),
            "asset=sol&moneyness=1.1,1.05&product=sell_sol"
        );
        let c = BoardQuery::parse(&q(&[
            ("asset", "sol"),
            ("moneyness", "1.05,1.1"),
            ("product", "sell_sol"),
        ]))
        .unwrap();
        assert_ne!(
            a.cache_key(),
            c.cache_key(),
            "moneyness order matters to the desk"
        );
    }

    #[test]
    fn market_response_parsing() {
        assert_eq!(
            parse_market_response(
                &json!({"type": "market_response", "ok": true, "result": {"a": 1}})
            )
            .unwrap(),
            json!({"a": 1})
        );
        let e = parse_market_response(
            &json!({"type": "market_response", "ok": false, "error": "stale", "status": 404}),
        )
        .unwrap_err();
        assert_eq!(
            e,
            MmFailure {
                message: "stale".into(),
                status: Some(404)
            }
        );
        let e =
            parse_market_response(&json!({"type": "market_response", "ok": false, "error": "x"}))
                .unwrap_err();
        assert_eq!(e.status, Some(400));
        assert!(parse_market_response(&json!({"type": "rfq_response"})).is_err());
        assert!(parse_market_response(&json!({"type": "market_response", "ok": true})).is_err());
    }

    #[tokio::test]
    async fn coalescer_shares_one_fetch_and_does_not_cache_errors() {
        let cache = Arc::new(Coalescer::new(Duration::from_secs(60)));
        let calls = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..10 {
            let (cache, calls) = (cache.clone(), calls.clone());
            tasks.push(tokio::spawn(async move {
                cache
                    .get("k", || async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(Arc::new(json!(1)))
                    })
                    .await
            }));
        }
        for t in tasks {
            assert_eq!(*t.await.unwrap().unwrap(), json!(1));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Cached.
        let v = cache
            .get("k", || async { Ok(Arc::new(json!(2))) })
            .await
            .unwrap();
        assert_eq!(*v, json!(1));

        let e = cache
            .get("e", || async { Err(MarketError::new(503, "x")) })
            .await;
        assert!(e.is_err());
        let v = cache
            .get("e", || async { Ok(Arc::new(json!(3))) })
            .await
            .unwrap();
        assert_eq!(*v, json!(3), "errors are not cached");
    }

    #[tokio::test]
    async fn coalescer_expires_entries() {
        let cache = Coalescer::new(Duration::from_millis(20));
        cache
            .get("k", || async { Ok(Arc::new(json!(1))) })
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(40)).await;
        let v = cache
            .get("k", || async { Ok(Arc::new(json!(2))) })
            .await
            .unwrap();
        assert_eq!(*v, json!(2));
    }
}
