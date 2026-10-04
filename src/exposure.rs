//! What each approved market maker carries (contract §5.6) and whether an RFQ still
//! fits its caps (§5.7). On-chain positions are scanned with `getProgramAccounts` on
//! `SOLANA_SCAN_RPC`; a co-signed transaction is reserved until the scan sees it.
//!
//! Until an MM's first successful scan its usage is unknown and it is not asked for
//! quotes. If later scans fail, the last view plus reservations keeps being used.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};
use tokio::sync::{Notify, watch};

use crate::mmreg::Caps;
use crate::protocol::num;
use crate::solana_rpc::{Memcmp, RpcError};
use crate::state::App;

pub const SOL: &str = "SOL";
/// The asset bucket of an `AssetPosition` whose mint has no `AssetConfig`.
pub const UNKNOWN: &str = "UNKNOWN";
pub const SOL_DECIMALS: u8 = 9;
const USDC_DECIMALS: i32 = 6;
/// §5.6: a reservation lasts until the position shows up in a scan, or this long.
pub const RESERVATION_TTL: Duration = Duration::from_secs(120);
const MAX_RESERVATIONS: usize = 10_000;
/// Float slack on cap comparisons, in USD.
const EPSILON_USD: f64 = 1e-6;

pub const POSITION_DISCRIMINATOR: [u8; 8] = [60, 125, 250, 193, 181, 109, 238, 86];
pub const ASSET_POSITION_DISCRIMINATOR: [u8; 8] = [64, 144, 148, 81, 123, 31, 96, 233];
pub const ASSET_CONFIG_DISCRIMINATOR: [u8; 8] = [57, 112, 247, 166, 247, 64, 140, 23];
/// `market_maker` in both position layouts: discriminator 8 + user 32.
pub const MARKET_MAKER_OFFSET: usize = 40;

/// The symbol the caps are keyed by: upper-cased, SOL when the asset is omitted.
pub fn asset_symbol(asset: Option<&str>) -> String {
    match asset.map(str::trim) {
        Some(a) if !a.is_empty() => a.to_ascii_uppercase(),
        _ => SOL.to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Product {
    SellSol,
    BuySol,
}

impl Product {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::SellSol),
            1 => Some(Self::BuySol),
            _ => None,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "sell_sol" => Some(Self::SellSol),
            "buy_sol" => Some(Self::BuySol),
            _ => None,
        }
    }
}

/// USD notional at the fixed price (§5.6). `user_collateral` is the asset for SellSol
/// (in units of 10^-`decimals`) and USDC for BuySol; `fixed_price` is USDC per whole
/// asset in base units.
pub fn notional_usd(product: Product, user_collateral: u64, fixed_price: u64, decimals: u8) -> f64 {
    match product {
        Product::SellSol => {
            user_collateral as f64 / 10f64.powi(i32::from(decimals)) * fixed_price as f64
                / 10f64.powi(USDC_DECIMALS)
        }
        Product::BuySol => user_collateral as f64 / 10f64.powi(USDC_DECIMALS),
    }
}

// ---------------------------------------------------------------------------
// Account layouts (§5.6), little-endian after the 8-byte discriminator

/// A `PositionAccount` (SOL) or `AssetPosition` (`asset_mint` set).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    pub market_maker: String,
    pub asset_mint: Option<String>,
    pub product: Product,
    pub fixed_price: u64,
    pub expiry_ts: i64,
    pub user_collateral: u64,
    pub nonce: u64,
    pub settled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AssetConfig {
    pub mint: String,
    pub symbol: String,
    pub decimals: u8,
    pub expiry_time_of_day: i64,
}

struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8], discriminator: &[u8; 8], what: &str) -> Result<Self, String> {
        if data.get(..8) != Some(discriminator.as_slice()) {
            return Err(format!("not a {what} account"));
        }
        Ok(Self { data, at: 8 })
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.at.checked_add(n).ok_or("account data is too short")?;
        let bytes = self
            .data
            .get(self.at..end)
            .ok_or("account data is too short")?;
        self.at = end;
        Ok(bytes)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], String> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| "account data is too short".to_string())
    }

    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.array::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, String> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, String> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn pubkey(&mut self) -> Result<String, String> {
        Ok(bs58::encode(self.take(32)?).into_string())
    }

    fn product(&mut self) -> Result<Product, String> {
        let b = self.u8()?;
        Product::from_byte(b).ok_or_else(|| format!("unknown product {b}"))
    }
}

/// user 32, market_maker 32, product u8, token u8, fixed_price u64, expiry_ts i64,
/// user_collateral u64, mm_collateral u64, yield_amount u64, nonce u64, settled u8,
/// bump u8.
pub fn decode_position(data: &[u8]) -> Result<Position, String> {
    let mut r = Reader::new(data, &POSITION_DISCRIMINATOR, "PositionAccount")?;
    r.take(32)?; // user
    let market_maker = r.pubkey()?;
    let product = r.product()?;
    r.u8()?; // token
    let fixed_price = r.u64()?;
    let expiry_ts = r.i64()?;
    let user_collateral = r.u64()?;
    r.u64()?; // mm_collateral
    r.u64()?; // yield_amount
    let nonce = r.u64()?;
    let settled = r.u8()? != 0;
    r.u8()?; // bump
    Ok(Position {
        market_maker,
        asset_mint: None,
        product,
        fixed_price,
        expiry_ts,
        user_collateral,
        nonce,
        settled,
    })
}

/// user 32, market_maker 32, asset_mint 32, product u8, fixed_price u64, expiry_ts
/// i64, user_collateral u64, mm_collateral u64, yield_amount u64, nonce u64, settled
/// u8, bump u8.
pub fn decode_asset_position(data: &[u8]) -> Result<Position, String> {
    let mut r = Reader::new(data, &ASSET_POSITION_DISCRIMINATOR, "AssetPosition")?;
    r.take(32)?; // user
    let market_maker = r.pubkey()?;
    let asset_mint = r.pubkey()?;
    let product = r.product()?;
    let fixed_price = r.u64()?;
    let expiry_ts = r.i64()?;
    let user_collateral = r.u64()?;
    r.u64()?; // mm_collateral
    r.u64()?; // yield_amount
    let nonce = r.u64()?;
    let settled = r.u8()? != 0;
    r.u8()?; // bump
    Ok(Position {
        market_maker,
        asset_mint: Some(asset_mint),
        product,
        fixed_price,
        expiry_ts,
        user_collateral,
        nonce,
        settled,
    })
}

/// mint 32, symbol (u32 length + UTF-8), decimals u8, expiry_time_of_day i64, bump u8.
pub fn decode_asset_config(data: &[u8]) -> Result<AssetConfig, String> {
    let mut r = Reader::new(data, &ASSET_CONFIG_DISCRIMINATOR, "AssetConfig")?;
    let mint = r.pubkey()?;
    let len = usize::try_from(r.u32()?).map_err(|_| "symbol is too long".to_string())?;
    let symbol = std::str::from_utf8(r.take(len)?)
        .map_err(|_| "symbol is not UTF-8".to_string())?
        .to_string();
    let decimals = r.u8()?;
    let expiry_time_of_day = r.i64()?;
    r.u8()?; // bump
    Ok(AssetConfig {
        mint,
        symbol,
        decimals,
        expiry_time_of_day,
    })
}

// ---------------------------------------------------------------------------
// Usage and the cap check

/// One MM's usage: on chain plus reserved. `reserved_usd` is the reserved part of
/// `total_usd`; `by_asset_usd` also includes reservations.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    /// False until the MM's first successful scan.
    pub known: bool,
    pub total_usd: f64,
    pub by_asset_usd: BTreeMap<String, f64>,
    pub reserved_usd: f64,
}

impl Usage {
    pub fn asset_usd(&self, symbol: &str) -> f64 {
        self.by_asset_usd.get(symbol).copied().unwrap_or(0.0)
    }

    pub fn to_json(&self) -> Value {
        let by_asset: Map<String, Value> = self
            .by_asset_usd
            .iter()
            .map(|(symbol, usd)| (symbol.clone(), num(*usd)))
            .collect();
        json!({
            "known": self.known,
            "total_usd": num(self.total_usd),
            "by_asset_usd": by_asset,
            "reserved_usd": num(self.reserved_usd),
        })
    }
}

/// Why an MM is, or is not, asked for a quote (§5.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Ask,
    /// No approved cap for the asset: it may not quote it at all.
    NoCap,
    /// No successful scan yet (fail closed).
    UsageUnknown,
    AssetCapReached,
    TotalCapReached,
}

impl Verdict {
    /// Excluded by rule 2 or 3, which words the decline as "capacity reached".
    pub fn is_cap(self) -> bool {
        matches!(self, Verdict::AssetCapReached | Verdict::TotalCapReached)
    }
}

/// §5.7 for one MM: `symbol` upper-cased, `notional` in USD.
pub fn check(caps: &Caps, usage: &Usage, symbol: &str, notional: f64) -> Verdict {
    let Some(asset_cap) = caps.asset_cap(symbol) else {
        return Verdict::NoCap;
    };
    if !usage.known {
        return Verdict::UsageUnknown;
    }
    if usage.asset_usd(symbol) + notional > asset_cap + EPSILON_USD {
        return Verdict::AssetCapReached;
    }
    if usage.total_usd + notional > caps.cap_total_usd + EPSILON_USD {
        return Verdict::TotalCapReached;
    }
    Verdict::Ask
}

// ---------------------------------------------------------------------------
// The tracker

#[derive(Debug, Clone)]
struct AssetInfo {
    /// Upper-cased.
    symbol: String,
    decimals: u8,
}

#[derive(Default)]
struct MmView {
    /// Unsettled notional per asset symbol.
    by_asset: BTreeMap<String, f64>,
    /// Nonces of every scanned position, settled or not: a reservation with one of
    /// them has shown up on chain.
    nonces: HashSet<u64>,
}

struct Reservation {
    asset: String,
    usd: f64,
    at: Instant,
}

#[derive(Default)]
struct Inner {
    /// `AssetConfig`s by mint; None until the first successful scan.
    assets: Option<HashMap<String, AssetInfo>>,
    /// One entry per approved MM that has been scanned.
    views: HashMap<String, MmView>,
    /// Keyed by `(mm_pubkey, nonce)`.
    reservations: HashMap<(String, u64), Reservation>,
}

pub struct Exposure {
    inner: Mutex<Inner>,
    /// False (tests): nothing is scanned and every MM's on-chain usage is known to be
    /// zero, so only reservations count.
    scanning: bool,
    ttl: Duration,
    wake: Notify,
}

impl Exposure {
    pub fn new(scanning: bool) -> Self {
        Self::with_ttl(scanning, RESERVATION_TTL)
    }

    pub fn with_ttl(scanning: bool, ttl: Duration) -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
            scanning,
            ttl,
            wake: Notify::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Asks the scan task to run now (an MM was just approved).
    pub fn trigger(&self) {
        self.wake.notify_one();
    }

    /// Replaces the `AssetConfig` table.
    pub fn set_assets(&self, configs: Vec<AssetConfig>) {
        let table = configs
            .into_iter()
            .map(|c| {
                let info = AssetInfo {
                    symbol: c.symbol.to_ascii_uppercase(),
                    decimals: c.decimals,
                };
                (c.mint, info)
            })
            .collect();
        self.lock().assets = Some(table);
    }

    pub fn assets_known(&self) -> bool {
        self.lock().assets.is_some()
    }

    /// Decimals of an upper-cased symbol: 9 for SOL, else its `AssetConfig`'s; None
    /// when the backend cannot size the asset.
    pub fn decimals_of(&self, symbol: &str) -> Option<u8> {
        if symbol == SOL {
            return Some(SOL_DECIMALS);
        }
        let inner = self.lock();
        inner
            .assets
            .as_ref()?
            .values()
            .find(|a| a.symbol == symbol)
            .map(|a| a.decimals)
    }

    /// `(symbol, decimals)` of a mint: SOL for none, `UNKNOWN` (sized like SOL) for a
    /// mint without an `AssetConfig`.
    fn asset_of(inner: &Inner, mint: Option<&str>) -> (String, u8) {
        let Some(mint) = mint else {
            return (SOL.to_string(), SOL_DECIMALS);
        };
        match inner.assets.as_ref().and_then(|table| table.get(mint)) {
            Some(info) => (info.symbol.clone(), info.decimals),
            None => (UNKNOWN.to_string(), SOL_DECIMALS),
        }
    }

    /// The asset symbol and USD notional of a position or of an open instruction.
    pub fn size(
        &self,
        asset_mint: Option<&str>,
        product: Product,
        user_collateral: u64,
        fixed_price: u64,
    ) -> (String, f64) {
        let (symbol, decimals) = Self::asset_of(&self.lock(), asset_mint);
        (
            symbol,
            notional_usd(product, user_collateral, fixed_price, decimals),
        )
    }

    /// Installs a successful scan of `mm_pubkey`: unsettled positions count whatever
    /// their expiry, settled ones do not, and every reservation whose nonce showed up
    /// is dropped.
    pub fn apply_scan(&self, mm_pubkey: &str, positions: &[Position]) {
        let mut inner = self.lock();
        let mut view = MmView::default();
        for p in positions {
            view.nonces.insert(p.nonce);
            if p.settled {
                continue;
            }
            let (symbol, decimals) = Self::asset_of(&inner, p.asset_mint.as_deref());
            *view.by_asset.entry(symbol).or_insert(0.0) +=
                notional_usd(p.product, p.user_collateral, p.fixed_price, decimals);
        }
        inner
            .reservations
            .retain(|(mm, nonce), _| mm != mm_pubkey || !view.nonces.contains(nonce));
        inner.views.insert(mm_pubkey.to_string(), view);
    }

    /// Drops the views of MMs that are no longer approved, so a later approval starts
    /// from "unknown" again.
    pub fn retain_mms(&self, approved: &[String]) {
        self.lock()
            .views
            .retain(|mm, _| approved.iter().any(|a| a == mm));
    }

    /// Forgets one MM (rejected, suspended or deleted).
    pub fn forget(&self, mm_pubkey: &str) {
        let mut inner = self.lock();
        inner.views.remove(mm_pubkey);
        inner.reservations.retain(|(mm, _), _| mm != mm_pubkey);
    }

    /// Reserves a co-signed position until a scan shows its nonce or the TTL passes.
    pub fn reserve(&self, mm_pubkey: &str, nonce: u64, asset: &str, usd: f64) {
        let ttl = self.ttl;
        let mut inner = self.lock();
        if inner
            .views
            .get(mm_pubkey)
            .is_some_and(|v| v.nonces.contains(&nonce))
        {
            return; // already on chain and counted there
        }
        inner.reservations.retain(|_, r| r.at.elapsed() < ttl);
        if inner.reservations.len() >= MAX_RESERVATIONS {
            let oldest = inner
                .reservations
                .iter()
                .min_by_key(|(_, r)| r.at)
                .map(|(key, _)| key.clone());
            if let Some(key) = oldest {
                inner.reservations.remove(&key);
            }
        }
        inner.reservations.insert(
            (mm_pubkey.to_string(), nonce),
            Reservation {
                asset: asset.to_string(),
                usd,
                at: Instant::now(),
            },
        );
    }

    /// Drops a reservation at once (the node refused the broadcast).
    pub fn release(&self, mm_pubkey: &str, nonce: u64) {
        self.lock()
            .reservations
            .remove(&(mm_pubkey.to_string(), nonce));
    }

    /// On chain plus live reservations.
    pub fn usage(&self, mm_pubkey: &str) -> Usage {
        let inner = self.lock();
        let view = inner.views.get(mm_pubkey);
        let mut usage = Usage {
            known: !self.scanning || view.is_some(),
            by_asset_usd: view.map(|v| v.by_asset.clone()).unwrap_or_default(),
            ..Usage::default()
        };
        for ((mm, _), r) in &inner.reservations {
            if mm == mm_pubkey && r.at.elapsed() < self.ttl {
                *usage.by_asset_usd.entry(r.asset.clone()).or_insert(0.0) += r.usd;
                usage.reserved_usd += r.usd;
            }
        }
        usage.total_usd = usage.by_asset_usd.values().sum();
        usage
    }
}

/// `/health.exposure` (§5.7) over `mm_pubkeys`: `(totalUsd, capTotalUsd, perAsset)`.
pub fn backend_view(app: &App, mm_pubkeys: &[String]) -> (f64, f64, Value) {
    let (mut total, mut cap_total) = (0.0, 0.0);
    let mut per_asset: BTreeMap<String, (f64, f64)> = BTreeMap::new();
    for mm in mm_pubkeys {
        let Some(caps) = app.mms.approved_caps(mm) else {
            continue;
        };
        let usage = app.exposure.usage(mm);
        total += usage.total_usd;
        cap_total += caps.cap_total_usd;
        for (symbol, cap) in &caps.asset_caps_usd {
            let entry = per_asset.entry(symbol.clone()).or_insert((0.0, 0.0));
            entry.0 += usage.asset_usd(symbol);
            entry.1 += cap;
        }
    }
    let per_asset: Map<String, Value> = per_asset
        .into_iter()
        .map(|(symbol, (used, cap))| (symbol, json!({ "usedUsd": num(used), "capUsd": num(cap) })))
        .collect();
    (total, cap_total, Value::Object(per_asset))
}

// ---------------------------------------------------------------------------
// The scan

async fn fetch<T>(
    app: &App,
    filters: &[Memcmp],
    decode: impl Fn(&[u8]) -> Result<T, String>,
) -> Result<Vec<T>, RpcError> {
    let accounts = app
        .scan_rpc
        .get_program_accounts(&app.config.program_id, filters)
        .await?;
    accounts
        .iter()
        .map(|a| {
            decode(&a.data)
                .map_err(|why| RpcError::InvalidResponse(format!("account {}: {why}", a.pubkey)))
        })
        .collect()
}

fn discriminator(d: &[u8; 8]) -> Memcmp {
    Memcmp {
        offset: 0,
        bytes: bs58::encode(d).into_string(),
    }
}

/// Both position kinds of one MM: two `getProgramAccounts` calls.
async fn scan_mm(app: &App, mm_pubkey: &str) -> Result<Vec<Position>, RpcError> {
    let mm = Memcmp {
        offset: MARKET_MAKER_OFFSET,
        bytes: mm_pubkey.to_string(),
    };
    let mut positions = fetch(
        app,
        &[discriminator(&POSITION_DISCRIMINATOR), mm.clone()],
        decode_position,
    )
    .await?;
    positions.extend(
        fetch(
            app,
            &[discriminator(&ASSET_POSITION_DISCRIMINATOR), mm],
            decode_asset_position,
        )
        .await?,
    );
    Ok(positions)
}

/// One cycle: the `AssetConfig` table, then every approved MM.
pub async fn scan_once(app: &App) {
    match fetch(
        app,
        &[discriminator(&ASSET_CONFIG_DISCRIMINATOR)],
        decode_asset_config,
    )
    .await
    {
        Ok(configs) => app.exposure.set_assets(configs),
        Err(e) => {
            tracing::warn!("asset config scan failed: {e}");
            // Without the table an AssetPosition cannot be sized: stay unknown.
            if !app.exposure.assets_known() {
                return;
            }
        }
    }
    let approved = app.mms.approved_pubkeys();
    app.exposure.retain_mms(&approved);
    for mm in &approved {
        match scan_mm(app, mm).await {
            Ok(positions) => app.exposure.apply_scan(mm, &positions),
            Err(e) => tracing::warn!(mm = %mm, "usage scan failed, keeping the last view: {e}"),
        }
    }
}

/// Scans every `EXPOSURE_SYNC_MS`, and at once when an MM is approved, until `stop`
/// turns true (or its sender is gone).
pub async fn run(app: Arc<App>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = scan_once(&app) => {}
            _ = stop.wait_for(|s| *s) => return,
        }
        tokio::select! {
            _ = tokio::time::sleep(app.config.exposure_sync) => {}
            _ = app.exposure.wake.notified() => {}
            _ = stop.wait_for(|s| *s) => return,
        }
    }
}

#[cfg(test)]
pub mod testing {
    //! Account bytes built by hand, per the contract's layouts.
    use super::*;

    pub fn key(byte: u8) -> [u8; 32] {
        [byte; 32]
    }

    pub fn b58(bytes: &[u8; 32]) -> String {
        bs58::encode(bytes).into_string()
    }

    #[allow(clippy::too_many_arguments)]
    pub fn position_bytes(
        mm: &[u8; 32],
        product: u8,
        fixed_price: u64,
        expiry_ts: i64,
        user_collateral: u64,
        nonce: u64,
        settled: bool,
    ) -> Vec<u8> {
        let mut d = POSITION_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&key(0xaa)); // user
        d.extend_from_slice(mm);
        d.push(product);
        d.push(product); // token
        d.extend_from_slice(&fixed_price.to_le_bytes());
        d.extend_from_slice(&expiry_ts.to_le_bytes());
        d.extend_from_slice(&user_collateral.to_le_bytes());
        d.extend_from_slice(&7u64.to_le_bytes()); // mm_collateral
        d.extend_from_slice(&8u64.to_le_bytes()); // yield_amount
        d.extend_from_slice(&nonce.to_le_bytes());
        d.push(u8::from(settled));
        d.push(254); // bump
        d
    }

    #[allow(clippy::too_many_arguments)]
    pub fn asset_position_bytes(
        mm: &[u8; 32],
        mint: &[u8; 32],
        product: u8,
        fixed_price: u64,
        expiry_ts: i64,
        user_collateral: u64,
        nonce: u64,
        settled: bool,
    ) -> Vec<u8> {
        let mut d = ASSET_POSITION_DISCRIMINATOR.to_vec();
        d.extend_from_slice(&key(0xaa)); // user
        d.extend_from_slice(mm);
        d.extend_from_slice(mint);
        d.push(product);
        d.extend_from_slice(&fixed_price.to_le_bytes());
        d.extend_from_slice(&expiry_ts.to_le_bytes());
        d.extend_from_slice(&user_collateral.to_le_bytes());
        d.extend_from_slice(&7u64.to_le_bytes());
        d.extend_from_slice(&8u64.to_le_bytes());
        d.extend_from_slice(&nonce.to_le_bytes());
        d.push(u8::from(settled));
        d.push(253);
        d
    }

    pub fn asset_config_bytes(mint: &[u8; 32], symbol: &str, decimals: u8) -> Vec<u8> {
        let mut d = ASSET_CONFIG_DISCRIMINATOR.to_vec();
        d.extend_from_slice(mint);
        d.extend_from_slice(&(symbol.len() as u32).to_le_bytes());
        d.extend_from_slice(symbol.as_bytes());
        d.push(decimals);
        d.extend_from_slice(&(13 * 3600i64).to_le_bytes());
        d.push(252);
        d
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    const MM: [u8; 32] = [1; 32];
    const MINT: [u8; 32] = [5; 32];
    const PAST: i64 = 1_000_000_000;
    const FUTURE: i64 = 4_000_000_000;

    #[test]
    fn decodes_a_position_account() {
        let data = position_bytes(
            &MM,
            0,
            250_000_000,
            FUTURE,
            500_000_000,
            0xfedc_ba98_7654_3210,
            false,
        );
        assert_eq!(data.len(), 124);
        assert_eq!(&data[MARKET_MAKER_OFFSET..MARKET_MAKER_OFFSET + 32], &MM);
        assert_eq!(
            decode_position(&data).unwrap(),
            Position {
                market_maker: b58(&MM),
                asset_mint: None,
                product: Product::SellSol,
                fixed_price: 250_000_000,
                expiry_ts: FUTURE,
                user_collateral: 500_000_000,
                nonce: 0xfedc_ba98_7654_3210,
                settled: false,
            }
        );
        let settled = position_bytes(&MM, 1, 1, -5, 2, 3, true);
        let p = decode_position(&settled).unwrap();
        assert_eq!(
            (p.product, p.expiry_ts, p.settled),
            (Product::BuySol, -5, true)
        );

        assert!(
            decode_position(&data[..123])
                .unwrap_err()
                .contains("too short")
        );
        assert!(decode_position(&[]).is_err());
        let mut other = data.clone();
        other[0] ^= 1;
        assert!(
            decode_position(&other)
                .unwrap_err()
                .contains("not a PositionAccount")
        );
        let mut bad_product = data.clone();
        bad_product[72] = 9;
        assert!(
            decode_position(&bad_product)
                .unwrap_err()
                .contains("unknown product")
        );
        // Trailing bytes (account padding) are fine.
        let mut padded = data;
        padded.extend_from_slice(&[0; 16]);
        assert!(decode_position(&padded).is_ok());
    }

    #[test]
    fn decodes_an_asset_position() {
        let data = asset_position_bytes(&MM, &MINT, 1, 300_000_000, PAST, 100_000_000, 77, false);
        assert_eq!(data.len(), 155);
        assert_eq!(&data[MARKET_MAKER_OFFSET..MARKET_MAKER_OFFSET + 32], &MM);
        assert_eq!(
            decode_asset_position(&data).unwrap(),
            Position {
                market_maker: b58(&MM),
                asset_mint: Some(b58(&MINT)),
                product: Product::BuySol,
                fixed_price: 300_000_000,
                expiry_ts: PAST,
                user_collateral: 100_000_000,
                nonce: 77,
                settled: false,
            }
        );
        assert!(decode_asset_position(&data[..154]).is_err());
        // The two layouts do not decode as each other.
        assert!(decode_position(&data).is_err());
        assert!(decode_asset_position(&position_bytes(&MM, 0, 1, 1, 1, 1, false)).is_err());
    }

    #[test]
    fn decodes_an_asset_config() {
        let data = asset_config_bytes(&MINT, "TSLAon", 8);
        assert_eq!(
            decode_asset_config(&data).unwrap(),
            AssetConfig {
                mint: b58(&MINT),
                symbol: "TSLAon".into(),
                decimals: 8,
                expiry_time_of_day: 13 * 3600,
            }
        );
        assert!(decode_asset_config(&data[..data.len() - 1]).is_err());
        // A symbol length that runs past the data.
        let mut long = data.clone();
        long[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(
            decode_asset_config(&long)
                .unwrap_err()
                .contains("too short")
        );
        let mut not_utf8 = data;
        not_utf8[44] = 0xff;
        assert!(
            decode_asset_config(&not_utf8)
                .unwrap_err()
                .contains("UTF-8")
        );
    }

    #[test]
    fn notional_math() {
        // 0.5 SOL at 250 USDC.
        assert_eq!(
            notional_usd(Product::SellSol, 500_000_000, 250_000_000, 9),
            125.0
        );
        // 2 tokens with 8 decimals at 300.5 USDC.
        assert_eq!(
            notional_usd(Product::SellSol, 200_000_000, 300_500_000, 8),
            601.0
        );
        // BuySol: the USDC collateral, whatever the price or the decimals.
        assert_eq!(
            notional_usd(Product::BuySol, 100_000_000, 180_000_000, 9),
            100.0
        );
        assert_eq!(notional_usd(Product::BuySol, 1_500_000, 1, 0), 1.5);
        assert!(notional_usd(Product::SellSol, u64::MAX, u64::MAX, 0).is_finite());
        assert_eq!(asset_symbol(None), "SOL");
        assert_eq!(asset_symbol(Some("")), "SOL");
        assert_eq!(asset_symbol(Some("sol")), "SOL");
        assert_eq!(asset_symbol(Some(" TSLAon ")), "TSLAON");
    }

    fn known(by_asset: &[(&str, f64)]) -> Usage {
        let by_asset_usd: BTreeMap<String, f64> =
            by_asset.iter().map(|(s, u)| (s.to_string(), *u)).collect();
        Usage {
            known: true,
            total_usd: by_asset_usd.values().sum(),
            by_asset_usd,
            reserved_usd: 0.0,
        }
    }

    #[test]
    fn cap_check_rules() {
        let caps = Caps::new(1000.0, &[("SOL", 600.0), ("TSLAON", 800.0)]);
        let empty = known(&[]);
        assert_eq!(
            check(&caps, &empty, "SOL", 600.0),
            Verdict::Ask,
            "exactly the cap fits"
        );
        assert_eq!(
            check(&caps, &empty, "SOL", 600.01),
            Verdict::AssetCapReached
        );
        assert_eq!(check(&caps, &empty, "WBTC", 1.0), Verdict::NoCap);
        assert_eq!(
            check(&caps, &Usage::default(), "SOL", 1.0),
            Verdict::UsageUnknown,
            "fail closed before the first scan"
        );
        assert_eq!(
            check(&caps, &Usage::default(), "WBTC", 1.0),
            Verdict::NoCap,
            "no cap is reported before unknown usage"
        );

        let used = known(&[("SOL", 500.0), ("TSLAON", 400.0)]);
        assert_eq!(check(&caps, &used, "SOL", 100.0), Verdict::Ask);
        assert_eq!(check(&caps, &used, "SOL", 100.5), Verdict::AssetCapReached);
        // The asset cap has room (400 + 300 <= 800) but the total does not.
        assert_eq!(
            check(&caps, &used, "TSLAON", 300.0),
            Verdict::TotalCapReached
        );
        assert_eq!(check(&caps, &used, "TSLAON", 100.0), Verdict::Ask);
        // Usage under UNKNOWN counts against the total only.
        let unknown = known(&[("UNKNOWN", 950.0)]);
        assert_eq!(
            check(&caps, &unknown, "SOL", 60.0),
            Verdict::TotalCapReached
        );
        assert_eq!(check(&caps, &unknown, "SOL", 50.0), Verdict::Ask);
        assert!(Verdict::TotalCapReached.is_cap() && Verdict::AssetCapReached.is_cap());
        assert!(!Verdict::NoCap.is_cap() && !Verdict::UsageUnknown.is_cap());
    }

    fn sol(collateral: u64, expiry_ts: i64, nonce: u64, settled: bool) -> Position {
        decode_position(&position_bytes(
            &MM,
            0,
            250_000_000,
            expiry_ts,
            collateral,
            nonce,
            settled,
        ))
        .unwrap()
    }

    #[test]
    fn expired_but_unsettled_counts_and_settled_does_not() {
        let exposure = Exposure::new(true);
        let mm = b58(&MM);
        assert!(!exposure.usage(&mm).known, "unknown until the first scan");

        exposure.set_assets(vec![
            decode_asset_config(&asset_config_bytes(&MINT, "TSLAon", 8)).unwrap(),
        ]);
        let asset = |product, collateral, expiry, nonce, settled| {
            decode_asset_position(&asset_position_bytes(
                &MM,
                &MINT,
                product,
                300_000_000,
                expiry,
                collateral,
                nonce,
                settled,
            ))
            .unwrap()
        };
        exposure.apply_scan(
            &mm,
            &[
                sol(500_000_000, FUTURE, 1, false),     // 125, live
                sol(1_000_000_000, PAST, 2, false),     // 250, expired but not settled: counts
                sol(4_000_000_000, PAST, 3, true),      // settled: does not count
                asset(0, 200_000_000, PAST, 4, false),  // 2 TSLAon at 300 = 600, expired
                asset(1, 50_000_000, FUTURE, 5, false), // 50 USDC
                asset(1, 999_000_000, FUTURE, 6, true), // settled
            ],
        );
        let usage = exposure.usage(&mm);
        assert!(usage.known);
        assert_eq!(usage.asset_usd("SOL"), 375.0);
        assert_eq!(usage.asset_usd("TSLAON"), 650.0);
        assert_eq!(usage.total_usd, 1025.0);
        assert_eq!(usage.reserved_usd, 0.0);

        let caps = Caps::new(1100.0, &[("SOL", 400.0), ("TSLAON", 1000.0)]);
        assert_eq!(check(&caps, &usage, "SOL", 25.0), Verdict::Ask);
        assert_eq!(
            check(&caps, &usage, "SOL", 26.0),
            Verdict::AssetCapReached,
            "the expired, unsettled 250 still takes capacity"
        );

        // Once it is settled the capacity is back.
        exposure.apply_scan(
            &mm,
            &[
                sol(500_000_000, FUTURE, 1, false),
                sol(1_000_000_000, PAST, 2, true),
            ],
        );
        let usage = exposure.usage(&mm);
        assert_eq!((usage.asset_usd("SOL"), usage.total_usd), (125.0, 125.0));
        assert_eq!(check(&caps, &usage, "SOL", 275.0), Verdict::Ask);
    }

    #[test]
    fn an_unlisted_mint_counts_under_unknown() {
        let exposure = Exposure::new(true);
        let mm = b58(&MM);
        exposure.set_assets(Vec::new());
        let position = decode_asset_position(&asset_position_bytes(
            &MM, &MINT, 1, 1, FUTURE, 40_000_000, 1, false,
        ))
        .unwrap();
        exposure.apply_scan(&mm, &[position]);
        let usage = exposure.usage(&mm);
        assert_eq!(usage.asset_usd(UNKNOWN), 40.0);
        assert_eq!(usage.total_usd, 40.0);
        assert_eq!(exposure.decimals_of("TSLAON"), None);
        assert_eq!(exposure.decimals_of("SOL"), Some(9));
    }

    #[test]
    fn reservations_count_until_scanned_released_or_expired() {
        let exposure = Exposure::new(true);
        let mm = b58(&MM);
        exposure.set_assets(Vec::new());
        exposure.apply_scan(&mm, &[sol(500_000_000, FUTURE, 1, false)]);

        exposure.reserve(&mm, 10, "SOL", 100.0);
        exposure.reserve(&mm, 11, "SOL", 50.0);
        exposure.reserve("other", 10, "SOL", 999.0);
        let usage = exposure.usage(&mm);
        assert_eq!((usage.total_usd, usage.reserved_usd), (275.0, 150.0));
        assert_eq!(usage.asset_usd("SOL"), 275.0);

        // The same nonce again replaces, never doubles.
        exposure.reserve(&mm, 10, "SOL", 100.0);
        assert_eq!(exposure.usage(&mm).reserved_usd, 150.0);

        // The node refused the broadcast: dropped at once.
        exposure.release(&mm, 11);
        assert_eq!(exposure.usage(&mm).reserved_usd, 100.0);

        // A scan that shows nonce 10 clears its reservation and counts it on chain;
        // the other MM's reservation under the same nonce stays.
        exposure.apply_scan(
            &mm,
            &[
                sol(500_000_000, FUTURE, 1, false),
                sol(400_000_000, FUTURE, 10, false),
            ],
        );
        let usage = exposure.usage(&mm);
        assert_eq!((usage.total_usd, usage.reserved_usd), (225.0, 0.0));
        assert_eq!(exposure.usage("other").reserved_usd, 999.0);

        // Reserving a nonce the scan already holds would count it twice.
        exposure.reserve(&mm, 10, "SOL", 100.0);
        assert_eq!(exposure.usage(&mm).reserved_usd, 0.0);

        // A scan without the nonce keeps the reservation.
        exposure.reserve(&mm, 12, "SOL", 30.0);
        exposure.apply_scan(&mm, &[]);
        assert_eq!(exposure.usage(&mm).total_usd, 30.0);

        exposure.forget(&mm);
        let usage = exposure.usage(&mm);
        assert!(!usage.known);
        assert_eq!(usage.total_usd, 0.0);
    }

    #[test]
    fn reservations_expire_and_are_bounded() {
        let exposure = Exposure::with_ttl(true, Duration::from_millis(30));
        exposure.reserve("mm", 1, "SOL", 100.0);
        assert_eq!(exposure.usage("mm").reserved_usd, 100.0);
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(exposure.usage("mm").reserved_usd, 0.0);
        exposure.reserve("mm", 2, "SOL", 1.0);
        assert_eq!(
            exposure.lock().reservations.len(),
            1,
            "expired ones are pruned"
        );

        let exposure = Exposure::new(true);
        for nonce in 0..(MAX_RESERVATIONS as u64 + 5) {
            exposure.reserve("mm", nonce, "SOL", 1.0);
        }
        assert_eq!(exposure.lock().reservations.len(), MAX_RESERVATIONS);
    }

    #[test]
    fn without_scanning_usage_is_known_and_only_reservations_count() {
        let exposure = Exposure::new(false);
        let usage = exposure.usage("mm");
        assert!(usage.known);
        assert_eq!(usage.total_usd, 0.0);
        exposure.reserve("mm", 1, "SOL", 125.0);
        assert_eq!(exposure.usage("mm").total_usd, 125.0);
    }

    #[test]
    fn views_of_unapproved_mms_are_dropped() {
        let exposure = Exposure::new(true);
        exposure.apply_scan("a", &[]);
        exposure.apply_scan("b", &[]);
        exposure.retain_mms(&["b".to_string()]);
        assert!(!exposure.usage("a").known);
        assert!(exposure.usage("b").known);
    }

    #[test]
    fn usage_json() {
        let mut usage = known(&[("SOL", 125.0), ("TSLAON", 0.5)]);
        usage.reserved_usd = 25.0;
        assert_eq!(
            usage.to_json(),
            json!({ "known": true, "total_usd": 125.5, "by_asset_usd": { "SOL": 125, "TSLAON": 0.5 }, "reserved_usd": 25 })
        );
    }
}
