//! Backend configuration (contract §3, §5.8). Every env var has a field here; the liveness,
//! faucet-confirmation, rate-limit and WebSocket-limit knobs at the bottom are contract
//! constants, kept on the struct so tests can shrink them, and are not read from the
//! environment.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use thiserror::Error;

use crate::mmreg::MmRecord;

pub const DEFAULT_BIND_ADDR: &str = "127.0.0.1:8080";
pub const DEFAULT_SOLANA_RPC: &str = "https://api.devnet.solana.com";
pub const DEFAULT_PROGRAM_ID: &str = "BeytdpJYSGP1oFRxEiBLkSRGuW6HW3Pk9dqSZCuSteQ9";
pub const DEFAULT_MM_REGISTRY_PATH: &str = "./data/mm-registry.json";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("{0} is required")]
    Missing(&'static str),
    #[error("{key} is invalid: {why}")]
    Invalid { key: &'static str, why: String },
}

fn invalid(key: &'static str, why: impl Into<String>) -> ConfigError {
    ConfigError::Invalid {
        key,
        why: why.into(),
    }
}

/// True for a base58 string that decodes to exactly 32 bytes.
pub fn is_pubkey(s: &str) -> bool {
    matches!(bs58::decode(s).into_vec(), Ok(bytes) if bytes.len() == 32)
}

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub solana_rpc: String,
    pub program_id: String,
    pub usdc_mint: String,
    /// §5: the registration file. None keeps the registry in memory only (tests).
    pub mm_registry_path: Option<PathBuf>,
    /// §5.6: the RPC used for `getProgramAccounts`; None means `solana_rpc`.
    pub solana_scan_rpc: Option<String>,
    /// §5.6: usage scan interval.
    pub exposure_sync: Duration,
    pub rfq_timeout: Duration,
    pub sign_timeout: Duration,
    pub market_timeout: Duration,
    pub health_poll: Duration,
    pub board_cache_ttl: Duration,
    pub expiries_cache_ttl: Duration,
    pub broadcast: bool,
    pub faucet_keypair: Option<PathBuf>,
    pub faucet_amount_usdc: f64,
    pub faucet_asset_usd: f64,
    pub faucet_sol_airdrop: f64,
    pub faucet_cooldown: Duration,
    pub admin_token: Option<String>,

    /// §1.2: a connection without a valid `hello` by then is closed (4001).
    pub hello_timeout: Duration,
    /// §1.3: server ping interval on every connection.
    pub ping_interval: Duration,
    /// §1.3: a connection that sent no frame (pong included) for this long is closed.
    pub idle_timeout: Duration,
    /// §2: a `health` snapshot younger than this is "fresh".
    pub health_fresh: Duration,
    /// Solana JSON-RPC request timeout.
    pub rpc_timeout: Duration,
    /// §2.8: how long a faucet mint may take to reach `confirmed`.
    pub faucet_confirm_timeout: Duration,
    /// §2.8: `getSignatureStatuses` poll interval while waiting for confirmation.
    pub faucet_poll_interval: Duration,
    /// §2.10: per-IP limits per window.
    pub rate_limits: RateLimits,
    /// §1.2: largest WebSocket message (and frame) accepted on `/ws/*`, before or
    /// after authentication; a bigger one closes the connection.
    pub ws_max_message_bytes: usize,
    /// §1.2: sockets upgraded but not yet authenticated, across all clients; past
    /// this, upgrades get 503.
    pub ws_max_unauthenticated: usize,
    /// §1.2: the same, per client IP (as §2.10 identifies it).
    pub ws_max_unauthenticated_per_ip: usize,
    /// Records added to the registry at startup when their pubkey is not in it yet.
    /// Not read from the environment: tests start with approved market makers.
    pub seed_mms: Vec<MmRecord>,
    /// §5.6: scan the chain for each MM's usage. Not read from the environment; with
    /// false (tests without a mock `getProgramAccounts`) nothing is scanned, every MM's
    /// on-chain usage counts as known and zero, and only reservations are enforced.
    pub exposure_scan: bool,
}

/// §2.10 requests per client IP per fixed window, by route class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimits {
    pub window: Duration,
    pub rfq: u32,
    pub sign: u32,
    pub faucet: u32,
    pub get: u32,
    /// WebSocket upgrade attempts on `/ws/rfq` and `/ws/sign`.
    pub ws: u32,
    /// Upgrades from a process on this machine (a loopback peer with no
    /// `CF-Connecting-IP`) skip both the `ws` limit and the unauthenticated-socket caps,
    /// so a flood through the public tunnel can never lock out the house desk that
    /// shares the VM. cloudflared also connects from loopback but always sets the header.
    pub exempt_local_ws: bool,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            window: Duration::from_secs(60),
            rfq: 120,
            sign: 30,
            faucet: 10,
            get: 600,
            ws: 60,
            exempt_local_ws: true,
        }
    }
}

impl Config {
    /// Contract defaults for everything except the one required value.
    pub fn new(usdc_mint: impl Into<String>) -> Self {
        Self {
            bind_addr: DEFAULT_BIND_ADDR.to_string(),
            solana_rpc: DEFAULT_SOLANA_RPC.to_string(),
            program_id: DEFAULT_PROGRAM_ID.to_string(),
            usdc_mint: usdc_mint.into(),
            mm_registry_path: Some(PathBuf::from(DEFAULT_MM_REGISTRY_PATH)),
            solana_scan_rpc: None,
            exposure_sync: Duration::from_millis(30_000),
            rfq_timeout: Duration::from_millis(1000),
            sign_timeout: Duration::from_millis(10_000),
            market_timeout: Duration::from_millis(2500),
            health_poll: Duration::from_millis(5000),
            board_cache_ttl: Duration::from_millis(5000),
            expiries_cache_ttl: Duration::from_millis(30_000),
            broadcast: true,
            faucet_keypair: None,
            faucet_amount_usdc: 1000.0,
            faucet_asset_usd: 1000.0,
            faucet_sol_airdrop: 0.5,
            faucet_cooldown: Duration::from_millis(3_600_000),
            admin_token: None,
            hello_timeout: Duration::from_secs(10),
            ping_interval: Duration::from_secs(20),
            idle_timeout: Duration::from_secs(60),
            health_fresh: Duration::from_secs(30),
            rpc_timeout: Duration::from_secs(10),
            faucet_confirm_timeout: Duration::from_secs(30),
            faucet_poll_interval: Duration::from_millis(500),
            rate_limits: RateLimits::default(),
            ws_max_message_bytes: 1 << 20,
            ws_max_unauthenticated: 256,
            ws_max_unauthenticated_per_ip: 8,
            seed_mms: Vec::new(),
            exposure_scan: true,
        }
    }

    /// The URL the usage scan uses: `SOLANA_SCAN_RPC`, else `SOLANA_RPC`.
    pub fn scan_rpc_url(&self) -> &str {
        self.solana_scan_rpc.as_deref().unwrap_or(&self.solana_rpc)
    }

    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|key| std::env::var(key).ok())
    }

    pub fn from_map(map: &HashMap<String, String>) -> Result<Self, ConfigError> {
        Self::from_lookup(|key| map.get(key).cloned())
    }

    /// Builds the config from any key lookup; empty values count as unset.
    pub fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let get = |key: &str| {
            lookup(key)
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };

        let usdc_mint = get("USDC_MINT").ok_or(ConfigError::Missing("USDC_MINT"))?;
        if !is_pubkey(&usdc_mint) {
            return Err(invalid("USDC_MINT", "not a base58 pubkey"));
        }
        let mut cfg = Self::new(usdc_mint);

        if let Some(v) = get("BIND_ADDR") {
            cfg.bind_addr = v;
        }
        if let Some(v) = get("SOLANA_RPC") {
            cfg.solana_rpc = v;
        }
        if let Some(v) = get("PROGRAM_ID") {
            if !is_pubkey(&v) {
                return Err(invalid("PROGRAM_ID", "not a base58 pubkey"));
            }
            cfg.program_id = v;
        }
        let ms = |key: &'static str, dflt: Duration| -> Result<Duration, ConfigError> {
            match get(key) {
                None => Ok(dflt),
                Some(v) => match v.parse::<u64>() {
                    Ok(n) if n > 0 => Ok(Duration::from_millis(n)),
                    _ => Err(invalid(
                        key,
                        format!("expected a positive integer of milliseconds, got {v:?}"),
                    )),
                },
            }
        };
        let num = |key: &'static str, dflt: f64| -> Result<f64, ConfigError> {
            match get(key) {
                None => Ok(dflt),
                Some(v) => match v.parse::<f64>() {
                    Ok(n) if n.is_finite() && n >= 0.0 => Ok(n),
                    _ => Err(invalid(
                        key,
                        format!("expected a non-negative number, got {v:?}"),
                    )),
                },
            }
        };
        if let Some(v) = get("MM_REGISTRY_PATH") {
            cfg.mm_registry_path = Some(PathBuf::from(v));
        }
        cfg.solana_scan_rpc = get("SOLANA_SCAN_RPC");
        cfg.exposure_sync = ms("EXPOSURE_SYNC_MS", cfg.exposure_sync)?;
        cfg.rfq_timeout = ms("RFQ_TIMEOUT_MS", cfg.rfq_timeout)?;
        cfg.sign_timeout = ms("SIGN_TIMEOUT_MS", cfg.sign_timeout)?;
        cfg.market_timeout = ms("MARKET_TIMEOUT_MS", cfg.market_timeout)?;
        cfg.health_poll = ms("HEALTH_POLL_MS", cfg.health_poll)?;
        cfg.board_cache_ttl = ms("BOARD_CACHE_MS", cfg.board_cache_ttl)?;
        cfg.expiries_cache_ttl = ms("EXPIRIES_CACHE_MS", cfg.expiries_cache_ttl)?;
        cfg.faucet_cooldown = ms("FAUCET_COOLDOWN_MS", cfg.faucet_cooldown)?;
        if let Some(v) = get("BROADCAST") {
            cfg.broadcast = parse_bool(&v)
                .ok_or_else(|| invalid("BROADCAST", format!("expected true/false, got {v:?}")))?;
        }
        cfg.faucet_keypair = get("FAUCET_KEYPAIR").map(PathBuf::from);
        cfg.faucet_amount_usdc = num("FAUCET_AMOUNT_USDC", cfg.faucet_amount_usdc)?;
        cfg.faucet_asset_usd = num("FAUCET_ASSET_USD", cfg.faucet_asset_usd)?;
        cfg.faucet_sol_airdrop = num("FAUCET_SOL_AIRDROP", cfg.faucet_sol_airdrop)?;
        cfg.admin_token = get("ADMIN_TOKEN");
        Ok(cfg)
    }
}

/// Same spellings the desk accepts (`1/true/yes/on`), plus their negatives.
fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";
    const B: &str = "9hSR6S7WPtxmTojgo6GG3k4yDPecgJY292j7xrsUGWBu";
    const MINT: &str = "EdmxWPmx2WH6WgFfTdu9xfkYf3k1g5wD1zccTVySEEh1";

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn requires_only_the_usdc_mint() {
        assert_eq!(
            Config::from_map(&map(&[])).unwrap_err(),
            ConfigError::Missing("USDC_MINT")
        );
        assert!(Config::from_map(&map(&[("USDC_MINT", "x")])).is_err());
        // MM_ALLOWLIST is no longer read, whatever it holds.
        assert!(
            Config::from_map(&map(&[("USDC_MINT", MINT), ("MM_ALLOWLIST", "garbage")])).is_ok()
        );
        assert!(is_pubkey(A) && is_pubkey(B) && !is_pubkey("1111"));
    }

    #[test]
    fn defaults_match_the_contract() {
        let cfg = Config::from_map(&map(&[
            ("USDC_MINT", MINT),
            ("ADMIN_TOKEN", ""),
            ("SOLANA_SCAN_RPC", ""),
        ]))
        .unwrap();
        assert_eq!(
            cfg.mm_registry_path,
            Some(PathBuf::from("./data/mm-registry.json"))
        );
        assert_eq!(cfg.solana_scan_rpc, None);
        assert_eq!(cfg.scan_rpc_url(), "https://api.devnet.solana.com");
        assert_eq!(cfg.exposure_sync, Duration::from_millis(30_000));
        assert!(cfg.exposure_scan);
        assert!(cfg.seed_mms.is_empty());
        assert_eq!(cfg.bind_addr, "127.0.0.1:8080");
        assert_eq!(cfg.solana_rpc, "https://api.devnet.solana.com");
        assert_eq!(cfg.program_id, DEFAULT_PROGRAM_ID);
        assert_eq!(cfg.rfq_timeout, Duration::from_millis(1000));
        assert_eq!(cfg.sign_timeout, Duration::from_millis(10_000));
        assert_eq!(cfg.market_timeout, Duration::from_millis(2500));
        assert_eq!(cfg.health_poll, Duration::from_millis(5000));
        assert_eq!(cfg.board_cache_ttl, Duration::from_millis(5000));
        assert_eq!(cfg.expiries_cache_ttl, Duration::from_millis(30_000));
        assert!(cfg.broadcast);
        assert!(cfg.faucet_keypair.is_none());
        assert_eq!(cfg.faucet_amount_usdc, 1000.0);
        assert_eq!(cfg.faucet_sol_airdrop, 0.5);
        assert_eq!(cfg.faucet_cooldown, Duration::from_millis(3_600_000));
        assert!(cfg.admin_token.is_none());
        assert_eq!(cfg.ping_interval, Duration::from_secs(20));
        assert_eq!(cfg.idle_timeout, Duration::from_secs(60));
        assert_eq!(cfg.hello_timeout, Duration::from_secs(10));
        assert_eq!(cfg.faucet_confirm_timeout, Duration::from_secs(30));
        let rl = cfg.rate_limits;
        assert_eq!(
            (rl.window, rl.rfq, rl.sign, rl.faucet, rl.get, rl.ws),
            (Duration::from_secs(60), 120, 30, 10, 600, 60)
        );
        assert_eq!(cfg.ws_max_message_bytes, 1024 * 1024);
        assert_eq!(cfg.ws_max_unauthenticated, 256);
        assert_eq!(cfg.ws_max_unauthenticated_per_ip, 8);
    }

    #[test]
    fn overrides_parse() {
        let cfg = Config::from_map(&map(&[
            ("USDC_MINT", MINT),
            ("MM_REGISTRY_PATH", "/opt/paytience/data/mm-registry.json"),
            ("SOLANA_SCAN_RPC", "https://scan.example/KEY"),
            ("EXPOSURE_SYNC_MS", "15000"),
            ("BIND_ADDR", "0.0.0.0:9000"),
            ("RFQ_TIMEOUT_MS", "250"),
            ("BROADCAST", "false"),
            ("FAUCET_KEYPAIR", "/keys/faucet.json"),
            ("FAUCET_SOL_AIRDROP", "0.25"),
            ("ADMIN_TOKEN", "t0ken"),
        ]))
        .unwrap();
        assert_eq!(cfg.bind_addr, "0.0.0.0:9000");
        assert_eq!(
            cfg.mm_registry_path,
            Some(PathBuf::from("/opt/paytience/data/mm-registry.json"))
        );
        assert_eq!(cfg.scan_rpc_url(), "https://scan.example/KEY");
        assert_eq!(cfg.exposure_sync, Duration::from_millis(15_000));
        assert_eq!(cfg.rfq_timeout, Duration::from_millis(250));
        assert!(!cfg.broadcast);
        assert_eq!(cfg.faucet_keypair, Some(PathBuf::from("/keys/faucet.json")));
        assert_eq!(cfg.faucet_sol_airdrop, 0.25);
        assert_eq!(cfg.admin_token.as_deref(), Some("t0ken"));
    }

    #[test]
    fn rejects_bad_numbers() {
        let base = [("USDC_MINT", MINT)];
        for (k, v) in [
            ("RFQ_TIMEOUT_MS", "abc"),
            ("SIGN_TIMEOUT_MS", "0"),
            ("EXPOSURE_SYNC_MS", "0"),
            ("BROADCAST", "maybe"),
            ("FAUCET_ASSET_USD", "-1"),
        ] {
            let mut m = map(&base);
            m.insert(k.to_string(), v.to_string());
            assert!(Config::from_map(&m).is_err(), "{k}={v} should fail");
        }
    }
}
