//! Per-client-IP rate limits (contract §2.10): fixed windows per route class. The
//! client is `CF-Connecting-IP` when it holds an IP (the Cloudflare tunnel sets it),
//! else the TCP peer. WebSocket upgrades have their own class; `OPTIONS` and non-GET
//! requests to other paths are not limited.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use axum::extract::{ConnectInfo, Request};
use axum::http::{HeaderMap, Method};

use crate::config::RateLimits;

/// Distinct (client, class) buckets kept per window; past this, new clients are let
/// through untracked rather than evicting ones already counted.
const MAX_BUCKETS: usize = 100_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RouteClass {
    Rfq,
    Sign,
    Faucet,
    Get,
    /// A WebSocket upgrade attempt on `/ws/rfq` or `/ws/sign`.
    Ws,
}

impl RouteClass {
    /// The class of a request, or None when it is not limited.
    pub fn of(method: &Method, path: &str) -> Option<Self> {
        match (method, path) {
            (_, "/ws/rfq" | "/ws/sign") => Some(Self::Ws),
            (&Method::POST, "/rfq") => Some(Self::Rfq),
            (&Method::POST, "/sign") => Some(Self::Sign),
            (&Method::POST, "/faucet") => Some(Self::Faucet),
            (&Method::GET | &Method::HEAD, _) => Some(Self::Get),
            _ => None,
        }
    }

    fn limit(self, limits: &RateLimits) -> u32 {
        match self {
            Self::Rfq => limits.rfq,
            Self::Sign => limits.sign,
            Self::Faucet => limits.faucet,
            Self::Get => limits.get,
            Self::Ws => limits.ws,
        }
    }
}

/// `CF-Connecting-IP` if it parses as an IP, else the peer address, else "unknown".
/// IPv6 clients are keyed by their /64, the block one subscriber usually holds, so
/// rotating addresses inside it does not buy a fresh budget.
pub fn client_key(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    let forwarded = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    match forwarded.or(peer.map(|addr| addr.ip())) {
        Some(ip) => ip_key(ip),
        None => "unknown".into(),
    }
}

fn ip_key(ip: IpAddr) -> String {
    match ip {
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_none() => {
            let s = v6.segments();
            format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
        }
        IpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map_or_else(|| v6.to_string(), |v4| v4.to_string()),
        IpAddr::V4(v4) => v4.to_string(),
    }
}

/// A process on this machine talking to us directly: a loopback peer without
/// `CF-Connecting-IP`. Public traffic arrives through cloudflared, which also connects
/// from loopback but always sets that header, so it never looks local.
pub fn is_local(headers: &HeaderMap, peer: Option<SocketAddr>) -> bool {
    !headers.contains_key("cf-connecting-ip") && peer.is_some_and(|p| p.ip().is_loopback())
}

struct Windows {
    /// Index of the current window since `started`.
    current: u64,
    counts: HashMap<(String, RouteClass), u32>,
}

pub struct RateLimiter {
    limits: RateLimits,
    started: Instant,
    windows: Mutex<Windows>,
}

impl RateLimiter {
    pub fn new(limits: RateLimits) -> Self {
        Self {
            limits,
            started: Instant::now(),
            windows: Mutex::new(Windows {
                current: 0,
                counts: HashMap::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Windows> {
        self.windows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn window_index(&self) -> u64 {
        let window = self.limits.window.as_nanos().max(1);
        u64::try_from(self.started.elapsed().as_nanos() / window).unwrap_or(u64::MAX)
    }

    /// Counts one request; false when it is over the class limit for this window.
    pub fn allow(&self, client: &str, class: RouteClass) -> bool {
        let limit = class.limit(&self.limits);
        let index = self.window_index();
        let mut w = self.lock();
        if w.current != index {
            // A new window: every old count is void, so drop them all.
            w.current = index;
            w.counts.clear();
        }
        let key = (client.to_string(), class);
        let full = w.counts.len() >= MAX_BUCKETS;
        match w.counts.get_mut(&key) {
            Some(n) => {
                if *n >= limit {
                    return false;
                }
                *n += 1;
                true
            }
            None if full => true,
            None => {
                w.counts.insert(key, 1);
                limit > 0
            }
        }
    }

    /// Buckets held for the current window (for tests).
    pub fn tracked(&self) -> usize {
        self.lock().counts.len()
    }

    /// The limiter's decision for an incoming request: true when it may proceed.
    pub fn check(&self, req: &Request) -> bool {
        let Some(class) = RouteClass::of(req.method(), req.uri().path()) else {
            return true;
        };
        let peer = req
            .extensions()
            .get::<ConnectInfo<SocketAddr>>()
            .map(|c| c.0);
        if class == RouteClass::Ws && self.limits.exempt_local_ws && is_local(req.headers(), peer) {
            return true;
        }
        self.allow(&client_key(req.headers(), peer), class)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use std::time::Duration;

    fn limits(window_ms: u64) -> RateLimits {
        RateLimits {
            window: Duration::from_millis(window_ms),
            rfq: 3,
            sign: 2,
            faucet: 1,
            get: 4,
            ws: 2,
            exempt_local_ws: false,
        }
    }

    #[test]
    fn classes() {
        let c = |m: Method, p: &str| RouteClass::of(&m, p);
        assert_eq!(c(Method::POST, "/rfq"), Some(RouteClass::Rfq));
        assert_eq!(c(Method::POST, "/sign"), Some(RouteClass::Sign));
        assert_eq!(c(Method::POST, "/faucet"), Some(RouteClass::Faucet));
        assert_eq!(c(Method::GET, "/faucet"), Some(RouteClass::Get));
        assert_eq!(c(Method::GET, "/board"), Some(RouteClass::Get));
        assert_eq!(c(Method::HEAD, "/health"), Some(RouteClass::Get));
        assert_eq!(c(Method::GET, "/nope"), Some(RouteClass::Get));
        assert_eq!(c(Method::GET, "/ws/rfq"), Some(RouteClass::Ws));
        assert_eq!(c(Method::GET, "/ws/sign"), Some(RouteClass::Ws));
        assert_eq!(c(Method::OPTIONS, "/rfq"), None);
        assert_eq!(c(Method::POST, "/health"), None);
    }

    #[test]
    fn counts_per_client_and_class() {
        let rl = RateLimiter::new(limits(60_000));
        for _ in 0..3 {
            assert!(rl.allow("1.1.1.1", RouteClass::Rfq));
        }
        assert!(!rl.allow("1.1.1.1", RouteClass::Rfq));
        assert!(rl.allow("2.2.2.2", RouteClass::Rfq), "per client");
        assert!(rl.allow("1.1.1.1", RouteClass::Sign), "per class");
        assert!(rl.allow("1.1.1.1", RouteClass::Faucet));
        assert!(!rl.allow("1.1.1.1", RouteClass::Faucet));
    }

    #[test]
    fn windows_reset_and_old_ones_are_dropped() {
        let rl = RateLimiter::new(limits(200));
        // Start at a window boundary so the burst lands in one window.
        while rl.started.elapsed().as_millis() % 200 > 10 {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(rl.allow("a", RouteClass::Faucet));
        assert!(!rl.allow("a", RouteClass::Faucet));
        assert!(rl.allow("b", RouteClass::Get));
        assert_eq!(rl.tracked(), 2);
        std::thread::sleep(Duration::from_millis(250));
        assert!(rl.allow("a", RouteClass::Faucet));
        assert_eq!(rl.tracked(), 1, "the old window's buckets are gone");
    }

    #[test]
    fn client_key_precedence() {
        let peer: SocketAddr = "10.0.0.1:5555".parse().unwrap();
        let mut h = HeaderMap::new();
        assert_eq!(client_key(&h, Some(peer)), "10.0.0.1");
        assert_eq!(client_key(&h, None), "unknown");
        h.insert(
            "cf-connecting-ip",
            HeaderValue::from_static(" 203.0.113.9 "),
        );
        assert_eq!(client_key(&h, Some(peer)), "203.0.113.9");
        h.insert("cf-connecting-ip", HeaderValue::from_static("2001:db8::1"));
        assert_eq!(client_key(&h, Some(peer)), "2001:db8:0:0::/64");
        h.insert(
            "cf-connecting-ip",
            HeaderValue::from_static("2001:db8:0:0:ffff:1:2:3"),
        );
        assert_eq!(
            client_key(&h, Some(peer)),
            "2001:db8:0:0::/64",
            "same /64, same key"
        );
        h.insert(
            "cf-connecting-ip",
            HeaderValue::from_static("::ffff:198.51.100.7"),
        );
        assert_eq!(client_key(&h, Some(peer)), "198.51.100.7");
        h.insert("cf-connecting-ip", HeaderValue::from_static("not-an-ip"));
        assert_eq!(client_key(&h, Some(peer)), "10.0.0.1");
    }

    #[test]
    fn local_means_loopback_without_the_tunnel_header() {
        let lo: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let lo6: SocketAddr = "[::1]:5555".parse().unwrap();
        let remote: SocketAddr = "10.0.0.1:5555".parse().unwrap();
        let mut h = HeaderMap::new();
        assert!(is_local(&h, Some(lo)));
        assert!(is_local(&h, Some(lo6)));
        assert!(!is_local(&h, Some(remote)));
        assert!(!is_local(&h, None));
        // cloudflared connects from loopback but always forwards the client's address.
        h.insert("cf-connecting-ip", HeaderValue::from_static("203.0.113.9"));
        assert!(!is_local(&h, Some(lo)));
        h.insert("cf-connecting-ip", HeaderValue::from_static("garbage"));
        assert!(
            !is_local(&h, Some(lo)),
            "any tunnel header, even a bad one, is not local"
        );
    }
}
