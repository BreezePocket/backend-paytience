//! The best-yield quote auction behind `POST /rfq` (contract §2.6, §1.4).

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use serde_json::{Map, Value, json};

use crate::config::is_pubkey;
use crate::conn::RequestError;
use crate::protocol::{id_string, new_id, parse_u64, rfq_decline};
use crate::registry::prefer_live;
use crate::state::App;
use crate::stats::Stats;

pub const PRODUCTS: [&str; 2] = ["sell_sol", "buy_sol"];

/// A validated client RFQ, ready to forward.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidRfq {
    pub rfq_id: String,
    /// None for SOL (omitted on the wire, like the dashboard does).
    pub asset: Option<String>,
    pub product: String,
    pub fixed_price: u64,
    pub expiry_ts: u64,
    pub amount: u64,
    pub user_pubkey: String,
    pub nonce: u64,
}

impl ValidRfq {
    /// The `rfq_request` sent to MMs, under the backend's own `rfq_id`.
    pub fn mm_frame(&self, backend_rfq_id: &str) -> Value {
        let mut m = Map::new();
        m.insert("type".into(), json!("rfq_request"));
        m.insert("rfq_id".into(), json!(backend_rfq_id));
        if let Some(asset) = &self.asset {
            m.insert("asset".into(), json!(asset));
        }
        m.insert("product".into(), json!(self.product));
        m.insert("fixed_price".into(), json!(self.fixed_price.to_string()));
        m.insert("expiry_ts".into(), json!(self.expiry_ts));
        m.insert("amount".into(), json!(self.amount.to_string()));
        m.insert("user_pubkey".into(), json!(self.user_pubkey));
        m.insert("nonce".into(), json!(self.nonce.to_string()));
        Value::Object(m)
    }
}

/// Validates a client RFQ; the error is `(rfq_id to echo, "invalid rfq: <why>")`.
pub fn validate_rfq(body: &Value) -> Result<ValidRfq, (String, String)> {
    let rfq_id = id_string(body.get("rfq_id"))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| new_id("rfq"));
    let bad = |why: &str| (rfq_id.clone(), format!("invalid rfq: {why}"));
    if !body.is_object() {
        return Err(bad("rfq_request must be an object"));
    }
    let asset = match body.get("asset") {
        None | Some(Value::Null) => None,
        Some(Value::String(a)) if a.is_empty() || a.eq_ignore_ascii_case("sol") => None,
        Some(Value::String(a)) => Some(a.clone()),
        Some(_) => return Err(bad("asset must be a string")),
    };
    let product = match body.get("product").and_then(Value::as_str) {
        Some(p) if PRODUCTS.contains(&p) => p.to_string(),
        _ => return Err(bad("product must be one of sell_sol, buy_sol")),
    };
    let positive = |key: &str| body.get(key).and_then(parse_u64).filter(|n| *n > 0);
    let fixed_price =
        positive("fixed_price").ok_or_else(|| bad("fixed_price must be a u64 > 0"))?;
    let expiry_ts = body
        .get("expiry_ts")
        .and_then(positive_integer)
        .ok_or_else(|| bad("expiry_ts must be a positive integer"))?;
    let amount = positive("amount").ok_or_else(|| bad("amount must be a u64 > 0"))?;
    let user_pubkey = match body.get("user_pubkey").and_then(Value::as_str) {
        Some(k) if is_pubkey(k) => k.to_string(),
        _ => return Err(bad("user_pubkey is not a valid public key")),
    };
    let nonce = body
        .get("nonce")
        .and_then(parse_u64)
        .ok_or_else(|| bad("nonce must be a u64"))?;
    Ok(ValidRfq {
        rfq_id: rfq_id.clone(),
        asset,
        product,
        fixed_price,
        expiry_ts,
        amount,
        user_pubkey,
        nonce,
    })
}

/// A JSON integer (or integral number / digit string) > 0.
fn positive_integer(v: &Value) -> Option<u64> {
    parse_u64(v).filter(|n| *n > 0)
}

/// One MM's answer to an `rfq_request`.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Quote {
        frame: Value,
        yield_amount: u64,
    },
    Decline(String),
    /// Discarded: spoofed, malformed or not an RFQ answer.
    Invalid(String),
    Failed(RequestError),
}

/// Classifies a reply from the connection authenticated as `mm_pubkey` (§1.4).
pub fn classify(reply: &Value, mm_pubkey: &str) -> Answer {
    match reply.get("type").and_then(Value::as_str) {
        Some("rfq_response") => {
            if reply.get("mm_pubkey").and_then(Value::as_str) != Some(mm_pubkey) {
                return Answer::Invalid("mm_pubkey differs from the authenticated pubkey".into());
            }
            let Some(yield_amount) = reply
                .get("yield_amount")
                .and_then(parse_u64)
                .filter(|y| *y > 0)
            else {
                return Answer::Invalid("yield_amount is not a u64 > 0".into());
            };
            if !reply.get("valid_until").is_some_and(Value::is_number) {
                return Answer::Invalid("valid_until is not a number".into());
            }
            Answer::Quote {
                frame: reply.clone(),
                yield_amount,
            }
        }
        Some("rfq_decline") => Answer::Decline(
            reply
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or("declined")
                .to_string(),
        ),
        _ => Answer::Invalid("not an rfq_response or rfq_decline".into()),
    }
}

/// Index of the largest yield; ties go to the earliest (lowest index = first received).
pub fn select_winner(yields_in_arrival_order: &[u64]) -> Option<usize> {
    let mut best: Option<usize> = None;
    for (i, y) in yields_in_arrival_order.iter().enumerate() {
        if best.is_none_or(|b| *y > yields_in_arrival_order[b]) {
            best = Some(i);
        }
    }
    best
}

/// §2.6 rule 5: the single distinct reason, or all distinct reasons in arrival order.
pub fn decline_reason(reasons_in_arrival_order: &[String]) -> Option<String> {
    let mut distinct: Vec<&str> = Vec::new();
    for r in reasons_in_arrival_order {
        if !distinct.contains(&r.as_str()) {
            distinct.push(r);
        }
    }
    match distinct.as_slice() {
        [] => None,
        [one] => Some(one.to_string()),
        many => Some(format!("no market maker quoted: {}", many.join("; "))),
    }
}

/// Who won the auction for a nonce, so `/sign` can route to that instance.
#[derive(Debug, Clone, PartialEq)]
pub struct Winner {
    pub mm_pubkey: String,
    pub instance_id: String,
    pub quote_id: String,
    pub valid_until: Value,
}

struct WinnerInner {
    map: HashMap<u64, (Winner, Instant)>,
    order: VecDeque<(u64, Instant)>,
}

/// `nonce → Winner` for `ttl` (10 minutes), bounded to `cap` entries.
pub struct WinnerMemory {
    inner: Mutex<WinnerInner>,
    ttl: Duration,
    cap: usize,
}

impl Default for WinnerMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl WinnerMemory {
    pub fn new() -> Self {
        Self::with_limits(Duration::from_secs(600), 100_000)
    }

    pub fn with_limits(ttl: Duration, cap: usize) -> Self {
        Self {
            inner: Mutex::new(WinnerInner {
                map: HashMap::new(),
                order: VecDeque::new(),
            }),
            ttl,
            cap: cap.max(1),
        }
    }

    fn lock(&self) -> MutexGuard<'_, WinnerInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn insert(&self, nonce: u64, winner: Winner) {
        let now = Instant::now();
        let mut inner = self.lock();
        inner.map.insert(nonce, (winner, now));
        inner.order.push_back((nonce, now));
        // Oldest first: drop expired entries, then anything over the cap.
        while let Some(&(n, at)) = inner.order.front() {
            let over = inner.map.len() > self.cap || inner.order.len() > self.cap * 2;
            if !over && at.elapsed() < self.ttl {
                break;
            }
            inner.order.pop_front();
            if inner.map.get(&n).is_some_and(|(_, t)| *t == at) {
                inner.map.remove(&n);
            }
        }
    }

    pub fn get(&self, nonce: u64) -> Option<Winner> {
        let inner = self.lock();
        inner
            .map
            .get(&nonce)
            .filter(|(_, at)| at.elapsed() < self.ttl)
            .map(|(w, _)| w.clone())
    }

    pub fn len(&self) -> usize {
        self.lock().map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Runs the auction; returns the HTTP status and typed body.
pub async fn run(app: &App, rfq: &ValidRfq) -> (u16, Value) {
    Stats::inc(&app.stats.rfqs);
    // Dry-run MMs quote but never co-sign: ask them only when no live MM is eligible.
    let targets = prefer_live(app.registry.eligible(), app.config.health_fresh);
    if targets.is_empty() {
        Stats::inc(&app.stats.declined);
        return (503, rfq_decline(&rfq.rfq_id, "no market maker is online"));
    }
    let backend_id = new_id("agg");
    let frame = rfq.mm_frame(&backend_id);
    let timeout = app.config.rfq_timeout;

    let mut pending: FuturesUnordered<_> = targets
        .iter()
        .map(|t| {
            app.registry.count(&t.mm_pubkey, |c| c.rfqs_asked += 1);
            let (frame, backend_id) = (&frame, &backend_id);
            async move { (t, t.rfq.request(backend_id, frame, timeout).await) }
        })
        .collect();

    // Arrival order: every future is bounded by the same timeout.
    let mut quotes: Vec<(&crate::registry::EligibleInstance, Value, u64)> = Vec::new();
    let mut declines: Vec<String> = Vec::new();
    while let Some((t, result)) = pending.next().await {
        let answer = match result {
            Ok(reply) => classify(&reply, &t.mm_pubkey),
            Err(e) => Answer::Failed(e),
        };
        match answer {
            Answer::Quote {
                frame,
                yield_amount,
            } => {
                app.registry.count(&t.mm_pubkey, |c| c.quotes += 1);
                quotes.push((t, frame, yield_amount));
            }
            Answer::Decline(reason) => {
                app.registry.count(&t.mm_pubkey, |c| c.declines += 1);
                declines.push(reason);
            }
            Answer::Invalid(why) => {
                tracing::warn!(mm = %t.name, rfq_id = %backend_id, "discarding rfq answer: {why}");
            }
            Answer::Failed(RequestError::Timeout) => {
                app.registry.count(&t.mm_pubkey, |c| c.timeouts += 1)
            }
            Answer::Failed(e) => tracing::debug!(mm = %t.name, "rfq not answered: {e}"),
        }
    }

    let yields: Vec<u64> = quotes.iter().map(|(_, _, y)| *y).collect();
    if let Some(i) = select_winner(&yields) {
        let (t, mut frame, yield_amount) = quotes.swap_remove(i);
        app.registry.count(&t.mm_pubkey, |c| c.wins += 1);
        app.winners.insert(
            rfq.nonce,
            Winner {
                mm_pubkey: t.mm_pubkey.clone(),
                instance_id: t.instance_id.clone(),
                quote_id: frame
                    .get("quote_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                valid_until: frame.get("valid_until").cloned().unwrap_or(Value::Null),
            },
        );
        if let Some(obj) = frame.as_object_mut() {
            obj.insert("rfq_id".into(), json!(rfq.rfq_id));
            obj.insert("yield_amount".into(), json!(yield_amount.to_string()));
            obj.insert("mm_name".into(), json!(t.name));
            obj.insert("quotes_received".into(), json!(yields.len()));
            obj.insert("mms_asked".into(), json!(targets.len()));
        }
        Stats::inc(&app.stats.quoted);
        return (200, frame);
    }
    if let Some(reason) = decline_reason(&declines) {
        Stats::inc(&app.stats.declined);
        return (200, rfq_decline(&rfq.rfq_id, reason));
    }
    Stats::inc(&app.stats.timed_out);
    (
        504,
        rfq_decline(&rfq.rfq_id, "no market maker responded in time"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const USER: &str = "GyGKxMyg1p9SsHfm15MkNUu1u9TN2JtTspcdmrtGUdse";
    const MM: &str = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";

    fn rfq() -> Value {
        json!({ "type": "rfq_request", "rfq_id": "web-1", "product": "buy_sol", "fixed_price": "180000000",
                "expiry_ts": 1759824000, "amount": "100000000", "user_pubkey": USER, "nonce": "18446744073709551615" })
    }

    #[test]
    fn validates_a_dashboard_rfq() {
        let v = validate_rfq(&rfq()).unwrap();
        assert_eq!(v.rfq_id, "web-1");
        assert_eq!(v.asset, None);
        assert_eq!(v.nonce, u64::MAX);
        assert_eq!(
            v.mm_frame("agg-1"),
            json!({ "type": "rfq_request", "rfq_id": "agg-1", "product": "buy_sol", "fixed_price": "180000000",
                    "expiry_ts": 1759824000, "amount": "100000000", "user_pubkey": USER, "nonce": "18446744073709551615" })
        );
        let mut with_asset = rfq();
        with_asset["asset"] = json!("TSLAon");
        assert_eq!(
            validate_rfq(&with_asset).unwrap().mm_frame("x")["asset"],
            json!("TSLAon")
        );
        with_asset["asset"] = json!("sol");
        assert!(
            validate_rfq(&with_asset)
                .unwrap()
                .mm_frame("x")
                .get("asset")
                .is_none()
        );
    }

    #[test]
    fn generates_a_missing_rfq_id() {
        let mut r = rfq();
        r.as_object_mut().unwrap().remove("rfq_id");
        assert!(!validate_rfq(&r).unwrap().rfq_id.is_empty());
    }

    #[test]
    fn rejects_invalid_rfqs() {
        let cases: &[(&str, Value, &str)] = &[
            (
                "product",
                json!("call"),
                "invalid rfq: product must be one of sell_sol, buy_sol",
            ),
            (
                "fixed_price",
                json!("0"),
                "invalid rfq: fixed_price must be a u64 > 0",
            ),
            (
                "fixed_price",
                json!("1.5"),
                "invalid rfq: fixed_price must be a u64 > 0",
            ),
            ("amount", json!(-1), "invalid rfq: amount must be a u64 > 0"),
            (
                "amount",
                Value::Null,
                "invalid rfq: amount must be a u64 > 0",
            ),
            ("nonce", json!("abc"), "invalid rfq: nonce must be a u64"),
            (
                "expiry_ts",
                json!(0),
                "invalid rfq: expiry_ts must be a positive integer",
            ),
            (
                "expiry_ts",
                json!(1.5),
                "invalid rfq: expiry_ts must be a positive integer",
            ),
            (
                "user_pubkey",
                json!("abc"),
                "invalid rfq: user_pubkey is not a valid public key",
            ),
            ("asset", json!(5), "invalid rfq: asset must be a string"),
        ];
        for (key, value, reason) in cases {
            let mut r = rfq();
            r[*key] = value.clone();
            let (id, why) = validate_rfq(&r).unwrap_err();
            assert_eq!(id, "web-1");
            assert_eq!(&why, reason, "{key}={value}");
        }
        assert_eq!(
            validate_rfq(&json!([1])).unwrap_err().1,
            "invalid rfq: rfq_request must be an object"
        );
        let mut zero_nonce = rfq();
        zero_nonce["nonce"] = json!("0");
        assert!(validate_rfq(&zero_nonce).is_ok(), "nonce may be 0");
    }

    fn quote(pk: &str, y: Value) -> Value {
        json!({ "type": "rfq_response", "rfq_id": "agg-1", "quote_id": "q", "yield_amount": y, "mm_pubkey": pk, "valid_until": 1 })
    }

    #[test]
    fn classifies_answers() {
        assert!(matches!(
            classify(&quote(MM, json!("5")), MM),
            Answer::Quote {
                yield_amount: 5,
                ..
            }
        ));
        assert!(matches!(
            classify(&quote(MM, json!(7)), MM),
            Answer::Quote {
                yield_amount: 7,
                ..
            }
        ));
        assert!(
            matches!(classify(&quote(USER, json!("5")), MM), Answer::Invalid(_)),
            "spoofed pubkey"
        );
        assert!(matches!(
            classify(&quote(MM, json!("0")), MM),
            Answer::Invalid(_)
        ));
        assert!(matches!(
            classify(&quote(MM, json!("x")), MM),
            Answer::Invalid(_)
        ));
        let mut q = quote(MM, json!("5"));
        q["valid_until"] = json!("soon");
        assert!(matches!(classify(&q, MM), Answer::Invalid(_)));
        assert_eq!(
            classify(&json!({"type": "rfq_decline", "reason": "too close"}), MM),
            Answer::Decline("too close".into())
        );
        assert!(matches!(
            classify(&json!({"type": "market_response"}), MM),
            Answer::Invalid(_)
        ));
    }

    #[test]
    fn winner_is_max_yield_ties_earliest() {
        assert_eq!(select_winner(&[]), None);
        assert_eq!(select_winner(&[5]), Some(0));
        assert_eq!(select_winner(&[5, 9, 3]), Some(1));
        assert_eq!(select_winner(&[9, 5, 9]), Some(0));
        assert_eq!(select_winner(&[u64::MAX - 1, u64::MAX]), Some(1));
    }

    #[test]
    fn decline_reasons() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert_eq!(decline_reason(&[]), None);
        assert_eq!(decline_reason(&s(&["a", "a"])).unwrap(), "a");
        assert_eq!(
            decline_reason(&s(&["b", "a", "b"])).unwrap(),
            "no market maker quoted: b; a"
        );
    }

    #[test]
    fn winner_memory_expires_and_is_bounded() {
        let w = |id: &str| Winner {
            mm_pubkey: MM.into(),
            instance_id: id.into(),
            quote_id: "q".into(),
            valid_until: json!(1),
        };
        let mem = WinnerMemory::with_limits(Duration::from_millis(30), 3);
        for n in 0..5 {
            mem.insert(n, w("i"));
        }
        assert_eq!(mem.len(), 3);
        assert!(mem.get(0).is_none() && mem.get(1).is_none());
        assert_eq!(mem.get(4).unwrap().instance_id, "i");
        mem.insert(4, w("j"));
        assert_eq!(
            mem.get(4).unwrap().instance_id,
            "j",
            "a nonce re-won replaces the winner"
        );
        std::thread::sleep(Duration::from_millis(40));
        assert!(mem.get(4).is_none());
        mem.insert(9, w("k"));
        assert_eq!(mem.len(), 1, "expired entries are pruned on insert");
    }
}
