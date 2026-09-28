//! Wire-level helpers shared by the WebSocket and REST sides (contract conventions):
//! u64s travel as decimal strings (safe JSON integers also accepted), ids are
//! prefixed uuids, and every frame is a JSON object with a `type`.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// Largest integer a JS number holds exactly (2^53 - 1).
pub const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    Rfq,
    Sign,
}

impl Channel {
    /// The name used in auth: the path without the leading slash.
    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Rfq => "ws/rfq",
            Channel::Sign => "ws/sign",
        }
    }
}

/// The exact bytes an MM signs in `hello`.
pub fn auth_message(channel: Channel, challenge: &str) -> String {
    format!("paytience-mm-auth:v1:{}:{challenge}", channel.as_str())
}

/// A u64 from a decimal string, or from a JSON number that is a safe non-negative integer.
pub fn parse_u64(v: &Value) -> Option<u64> {
    match v {
        Value::String(s) => {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            s.parse::<u64>().ok()
        }
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                return (u <= MAX_SAFE_INTEGER).then_some(u);
            }
            // `5.0` is still an integer to JS.
            let f = n.as_f64()?;
            (f.fract() == 0.0 && (0.0..=MAX_SAFE_INTEGER as f64).contains(&f)).then_some(f as u64)
        }
        _ => None,
    }
}

/// A finite JSON number as f64.
pub fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64().filter(|f| f.is_finite())
}

/// A JSON number that prints integral values as integers, like JS would.
pub fn num(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64 {
        if f < 0.0 {
            json!(f as i64)
        } else {
            json!(f as u64)
        }
    } else {
        json!(f)
    }
}

/// `prefix-<uuid v4>`: backend ids never collide with client ids.
pub fn new_id(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4())
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 32 random bytes as 64 lowercase hex chars.
pub fn random_challenge() -> std::io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// A string field, or the JS `String(x)` of a scalar (the desk echoes ids this way).
pub fn id_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

pub fn error_body(message: impl Into<String>) -> Value {
    json!({ "error": message.into() })
}

pub fn rfq_decline(rfq_id: &str, reason: impl Into<String>) -> Value {
    json!({ "type": "rfq_decline", "rfq_id": rfq_id, "reason": reason.into() })
}

pub fn sign_rejection(request_id: &str, reason: impl Into<String>) -> Value {
    json!({ "type": "sign_rejection", "request_id": request_id, "reason": reason.into() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u64_from_strings_and_safe_integers() {
        assert_eq!(parse_u64(&json!("1500000")), Some(1_500_000));
        assert_eq!(parse_u64(&json!("18446744073709551615")), Some(u64::MAX));
        assert_eq!(parse_u64(&json!("18446744073709551616")), None);
        assert_eq!(parse_u64(&json!("0")), Some(0));
        assert_eq!(parse_u64(&json!("")), None);
        assert_eq!(parse_u64(&json!("-1")), None);
        assert_eq!(parse_u64(&json!("+1")), None);
        assert_eq!(parse_u64(&json!(" 1")), None);
        assert_eq!(parse_u64(&json!("1.5")), None);
        assert_eq!(parse_u64(&json!(42)), Some(42));
        assert_eq!(parse_u64(&json!(5.0)), Some(5));
        assert_eq!(parse_u64(&json!(5.5)), None);
        assert_eq!(parse_u64(&json!(-3)), None);
        assert_eq!(parse_u64(&json!(MAX_SAFE_INTEGER)), Some(MAX_SAFE_INTEGER));
        assert_eq!(parse_u64(&json!(MAX_SAFE_INTEGER + 1)), None);
        assert_eq!(parse_u64(&json!(null)), None);
        assert_eq!(parse_u64(&json!(true)), None);
    }

    #[test]
    fn auth_message_format() {
        assert_eq!(
            auth_message(Channel::Rfq, "ab12"),
            "paytience-mm-auth:v1:ws/rfq:ab12"
        );
        assert_eq!(
            auth_message(Channel::Sign, "ab12"),
            "paytience-mm-auth:v1:ws/sign:ab12"
        );
    }

    #[test]
    fn challenge_is_64_lowercase_hex() {
        let c = random_challenge().unwrap();
        assert_eq!(c.len(), 64);
        assert!(
            c.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_ne!(c, random_challenge().unwrap());
    }

    #[test]
    fn num_prints_integers_as_integers() {
        assert_eq!(num(1000.0), json!(1000));
        assert_eq!(num(-2.0), json!(-2));
        assert_eq!(num(1.5), json!(1.5));
    }
}
