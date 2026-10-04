//! Shared application state, handed to every handler as `State<Arc<App>>`.

use std::sync::Arc;

use crate::auction::WinnerMemory;
use anyhow::Context;

use crate::config::Config;
use crate::exposure::Exposure;
use crate::faucet::{Faucet, load_keypair};
use crate::market::Coalescer;
use crate::mmreg::MmRegistry;
use crate::preauth::PreauthGate;
use crate::ratelimit::RateLimiter;
use crate::registry::Registry;
use crate::solana_rpc::SolanaRpc;
use crate::stats::Stats;

pub struct App {
    pub config: Config,
    /// Live connections.
    pub registry: Registry,
    /// §5: registrations and caps (who may connect, and how much each may carry).
    pub mms: MmRegistry,
    /// §5.6: each approved MM's usage, on chain and reserved.
    pub exposure: Exposure,
    /// `SOLANA_SCAN_RPC`: the `getProgramAccounts` client of the usage scan.
    pub scan_rpc: SolanaRpc,
    pub stats: Stats,
    pub winners: WinnerMemory,
    pub rpc: SolanaRpc,
    pub board_cache: Coalescer,
    pub expiries_cache: Coalescer,
    /// Present only when `FAUCET_KEYPAIR` is set.
    pub faucet: Option<Faucet>,
    pub rate_limiter: RateLimiter,
    /// Slots for WebSocket connections that have not authenticated yet (§1.2).
    pub preauth: PreauthGate,
}

impl App {
    pub fn new(config: Config) -> anyhow::Result<Arc<Self>> {
        let rpc = SolanaRpc::new(&config.solana_rpc, config.rpc_timeout)?;
        let scan_rpc = SolanaRpc::new(config.scan_rpc_url(), config.rpc_timeout)?;
        let mms = match &config.mm_registry_path {
            Some(path) => MmRegistry::load(path)
                .map_err(anyhow::Error::msg)
                .context("MM_REGISTRY_PATH")?,
            None => MmRegistry::in_memory(Vec::new()),
        };
        mms.seed(&config.seed_mms)
            .map_err(|e| anyhow::Error::msg(e.to_string()))
            .context("MM_REGISTRY_PATH")?;
        let faucet = match &config.faucet_keypair {
            None => None,
            Some(path) => {
                let authority = load_keypair(path)
                    .map_err(anyhow::Error::msg)
                    .context("FAUCET_KEYPAIR")?;
                let faucet = Faucet::new(authority, &config.usdc_mint, config.faucet_cooldown)
                    .map_err(anyhow::Error::msg)?;
                tracing::info!(
                    authority = %faucet.authority(),
                    amount_usdc = config.faucet_amount_usdc,
                    asset_usd = config.faucet_asset_usd,
                    "faucet enabled"
                );
                Some(faucet)
            }
        };
        Ok(Arc::new(Self {
            registry: Registry::new(),
            mms,
            exposure: Exposure::new(config.exposure_scan),
            scan_rpc,
            stats: Stats::default(),
            winners: WinnerMemory::new(),
            rpc,
            board_cache: Coalescer::new(config.board_cache_ttl),
            expiries_cache: Coalescer::new(config.expiries_cache_ttl),
            faucet,
            rate_limiter: RateLimiter::new(config.rate_limits),
            preauth: PreauthGate::new(
                config.ws_max_unauthenticated,
                config.ws_max_unauthenticated_per_ip,
            ),
            config,
        }))
    }
}
