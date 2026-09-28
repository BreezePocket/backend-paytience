//! Shared application state, handed to every handler as `State<Arc<App>>`.

use std::sync::Arc;

use crate::auction::WinnerMemory;
use anyhow::Context;

use crate::config::Config;
use crate::faucet::{Faucet, load_keypair};
use crate::market::Coalescer;
use crate::preauth::PreauthGate;
use crate::ratelimit::RateLimiter;
use crate::registry::Registry;
use crate::solana_rpc::SolanaRpc;
use crate::stats::Stats;

pub struct App {
    pub config: Config,
    pub registry: Registry,
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
