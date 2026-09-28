//! Devnet faucet (contract §2.8): mints test USDC, or `FAUCET_ASSET_USD` worth of a
//! listed asset's test token, to a wallet, then best-effort airdrops devnet SOL.
//! Enabled only when `FAUCET_KEYPAIR` (the mint authority of the test USDC and of the
//! test asset mints) is set. Mirrors the desk's `faucet.js`, with the SPL instructions
//! built by hand because the spl crates do not build against the solana 4.x/5.x crates.

use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Value, json};
use solana_hash::Hash;
use solana_instruction::{AccountMeta, Instruction};
use solana_keypair::Keypair;
use solana_message::Message;
use solana_pubkey::Pubkey;
use solana_signer::Signer;
use solana_transaction::Transaction;

use crate::market;
use crate::protocol::{as_f64, num};
use crate::solana_rpc::SolanaRpc;
use crate::state::App;

/// SPL Token (legacy) program.
pub const TOKEN_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
/// SPL Associated Token Account program.
pub const ATA_PROGRAM_ID: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");
pub const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::from_str_const("11111111111111111111111111111111");

/// ATA program `CreateIdempotent`.
const CREATE_IDEMPOTENT: u8 = 1;
/// Token program `MintTo`.
const MINT_TO: u8 = 7;
const USDC_DECIMALS: u32 = 6;
const LAMPORTS_PER_SOL: f64 = 1e9;
/// Past this many cooldown entries, expired ones are swept on the next claim.
const COOLDOWN_SWEEP_AT: usize = 4096;

pub const DISABLED: &str = "faucet disabled";

/// An HTTP status and `{error}` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaucetError {
    pub status: u16,
    pub message: String,
}

fn err(status: u16, message: impl Into<String>) -> FaucetError {
    FaucetError {
        status,
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// Pure parts: rounding, addresses, instructions, transaction

/// JS `Number(x.toPrecision(4))`: rounds the exact binary value to 4 significant
/// digits, ties to the larger magnitude (Rust's own formatting would tie to even).
pub fn to_precision4(x: f64) -> f64 {
    if x == 0.0 || !x.is_finite() {
        return x;
    }
    // Every finite f64 has at most 767 significant digits, so this is exact.
    let exact = format!("{:.800e}", x.abs());
    let Some((mantissa, exp)) = exact.split_once('e') else {
        return x;
    };
    let Ok(mut exp) = exp.parse::<i32>() else {
        return x;
    };
    let digits: Vec<u8> = mantissa.bytes().filter(u8::is_ascii_digit).collect();
    let mut kept: u32 = digits
        .iter()
        .take(4)
        .fold(0, |n, d| n * 10 + u32::from(d - b'0'));
    if digits.get(4).is_some_and(|d| *d >= b'5') {
        kept += 1;
        if kept == 10_000 {
            kept = 1000;
            exp += 1;
        }
    }
    let rounded = format!("{kept}e{}", exp - 3).parse::<f64>().unwrap_or(x);
    rounded.copysign(x)
}

/// The associated token account of `owner` for `mint` (legacy token program). Off-curve
/// owners are allowed, like the desk's `getAssociatedTokenAddressSync(mint, owner, true)`.
pub fn ata_address(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(
        &[owner.as_ref(), TOKEN_PROGRAM_ID.as_ref(), mint.as_ref()],
        &ATA_PROGRAM_ID,
    )
    .0
}

/// ATA program `CreateIdempotent`: payer(s,w), ata(w), owner, mint, system, token program.
pub fn create_ata_idempotent_ix(
    payer: &Pubkey,
    ata: &Pubkey,
    owner: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    Instruction {
        program_id: ATA_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(*ata, false),
            AccountMeta::new_readonly(*owner, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![CREATE_IDEMPOTENT],
    }
}

/// Token program `MintTo`: mint(w), destination(w), authority(s); data `[7] + u64 LE`.
pub fn mint_to_ix(mint: &Pubkey, dest: &Pubkey, authority: &Pubkey, amount: u64) -> Instruction {
    let mut data = vec![MINT_TO];
    data.extend_from_slice(&amount.to_le_bytes());
    Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*mint, false),
            AccountMeta::new(*dest, false),
            AccountMeta::new_readonly(*authority, true),
        ],
        data,
    }
}

/// The signed claim transaction: create `owner`'s ATA if missing, then mint into it.
/// The authority pays the fee and signs.
pub fn build_mint_tx(
    authority: &Keypair,
    owner: &Pubkey,
    mint: &Pubkey,
    amount: u64,
    blockhash: Hash,
) -> Result<Transaction, String> {
    let payer = authority.pubkey();
    let ata = ata_address(owner, mint);
    let ixs = [
        create_ata_idempotent_ix(&payer, &ata, owner, mint),
        mint_to_ix(mint, &ata, &payer, amount),
    ];
    let message = Message::new_with_blockhash(&ixs, Some(&payer), &blockhash);
    let mut tx = Transaction::new_unsigned(message);
    tx.try_sign(&[authority], blockhash)
        .map_err(|e| format!("signing failed: {e}"))?;
    Ok(tx)
}

/// Base64 of the wire format.
pub fn encode_tx(tx: &Transaction) -> Result<String, String> {
    let bytes = wincode::serialize(tx).map_err(|e| format!("serializing failed: {e}"))?;
    Ok(B64.encode(bytes))
}

/// A Solana CLI keypair file: a JSON array of 64 bytes (secret seed, then public key).
/// Errors never include the file's contents.
pub fn load_keypair(path: &Path) -> Result<Keypair, String> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let bytes: Vec<u8> = serde_json::from_str(&raw)
        .map_err(|_| format!("{} is not a JSON array of bytes", path.display()))?;
    if bytes.len() != 64 {
        return Err(format!(
            "{} holds {} bytes, expected 64",
            path.display(),
            bytes.len()
        ));
    }
    Keypair::try_from(bytes.as_slice())
        .map_err(|_| format!("{}: public key does not match the secret", path.display()))
}

// ---------------------------------------------------------------------------
// Cooldowns

/// Last claim per `(wallet, SYMBOL)`. A slot is taken before sending and released
/// only by the claim that took it, so a failure never frees someone else's slot.
struct Cooldowns {
    period: Duration,
    slots: Mutex<CooldownSlots>,
}

#[derive(Default)]
struct CooldownSlots {
    map: HashMap<String, (Instant, u64)>,
    next_token: u64,
}

impl Cooldowns {
    fn new(period: Duration) -> Self {
        Self {
            period,
            slots: Mutex::new(CooldownSlots::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CooldownSlots> {
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the slot; `Err(remaining)` while it is cooling down.
    fn take(&self, key: &str) -> Result<u64, Duration> {
        let mut slots = self.lock();
        if let Some((at, _)) = slots.map.get(key) {
            let elapsed = at.elapsed();
            if elapsed < self.period {
                return Err(self.period - elapsed);
            }
        }
        if slots.map.len() >= COOLDOWN_SWEEP_AT {
            let period = self.period;
            slots.map.retain(|_, (at, _)| at.elapsed() < period);
        }
        slots.next_token += 1;
        let token = slots.next_token;
        slots.map.insert(key.to_string(), (Instant::now(), token));
        Ok(token)
    }

    fn release(&self, key: &str, token: u64) {
        let mut slots = self.lock();
        if slots.map.get(key).is_some_and(|(_, t)| *t == token) {
            slots.map.remove(key);
        }
    }
}

/// `Math.ceil(remaining / 60_000)`, at least 1 while cooling down.
pub fn cooldown_minutes(remaining: Duration) -> u128 {
    remaining.as_millis().max(1).div_ceil(60_000)
}

// ---------------------------------------------------------------------------
// The faucet

pub struct Faucet {
    authority: Keypair,
    usdc_mint: Pubkey,
    cooldowns: Cooldowns,
}

/// What one claim mints.
#[derive(Debug, Clone, PartialEq)]
pub struct Claim {
    pub mint: Pubkey,
    pub base_units: u64,
    pub amount: f64,
    pub symbol: String,
}

impl Faucet {
    pub fn new(authority: Keypair, usdc_mint: &str, cooldown: Duration) -> Result<Self, String> {
        let usdc_mint =
            Pubkey::from_str(usdc_mint).map_err(|_| "USDC_MINT is not a pubkey".to_string())?;
        Ok(Self {
            authority,
            usdc_mint,
            cooldowns: Cooldowns::new(cooldown),
        })
    }

    pub fn authority(&self) -> Pubkey {
        self.authority.pubkey()
    }

    /// `GET /faucet`.
    pub fn info(&self, app: &App) -> Value {
        let cfg = &app.config;
        json!({
            "enabled": true,
            "amount_usdc": num(cfg.faucet_amount_usdc),
            "asset_usd": num(cfg.faucet_asset_usd),
            "cooldown_ms": u64::try_from(cfg.faucet_cooldown.as_millis()).unwrap_or(u64::MAX),
            "usdc_mint": self.usdc_mint.to_string(),
        })
    }

    /// Resolves `asset` (None or `USDC` for test USDC) to what a claim mints.
    pub fn claim_for(&self, app: &App, asset: Option<&str>) -> Result<Claim, FaucetError> {
        let cfg = &app.config;
        let asset = asset.filter(|a| !a.is_empty());
        let Some(asset) = asset.filter(|a| !a.eq_ignore_ascii_case("USDC")) else {
            let amount = cfg.faucet_amount_usdc;
            return Ok(Claim {
                mint: self.usdc_mint,
                base_units: base_units(amount, USDC_DECIMALS)
                    .ok_or_else(|| err(400, "USDC claim amount is out of range"))?,
                amount,
                symbol: "USDC".into(),
            });
        };

        // The same merged view `/assets` serves (§2.2).
        let merged = market::assets(app);
        let entry = merged
            .get("assets")
            .and_then(Value::as_array)
            .and_then(|list| {
                list.iter().find(|a| {
                    a.get("asset")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case(asset))
                })
            });
        let not_listed = || err(400, format!("{asset} is not listed on chain"));
        let entry = entry.ok_or_else(not_listed)?;
        let mint = entry
            .get("mint")
            .and_then(Value::as_str)
            .and_then(|m| Pubkey::from_str(m).ok());
        let decimals = entry
            .get("decimals")
            .and_then(Value::as_u64)
            .and_then(|d| u32::try_from(d).ok());
        let (Some(mint), Some(decimals), Some(true)) = (
            mint,
            decimals,
            entry.get("tradable").and_then(Value::as_bool),
        ) else {
            return Err(not_listed());
        };
        let symbol = entry
            .get("asset")
            .and_then(Value::as_str)
            .unwrap_or(asset)
            .to_string();
        let spot = entry
            .get("spot")
            .and_then(as_f64)
            .filter(|s| *s > 0.0)
            .ok_or_else(|| err(503, format!("{symbol} has no price yet; try again shortly")))?;
        // FAUCET_ASSET_USD worth of the token, rounded to 4 significant figures.
        let amount = to_precision4(cfg.faucet_asset_usd / spot);
        let base_units = base_units(amount, decimals)
            .ok_or_else(|| err(400, format!("{symbol} claim amount is out of range")))?;
        Ok(Claim {
            mint,
            base_units,
            amount,
            symbol,
        })
    }

    /// `POST /faucet` (§2.8); returns the 200 body.
    pub async fn claim(&self, app: &App, body: &Value) -> Result<Value, FaucetError> {
        let owner = body
            .get("pubkey")
            .and_then(Value::as_str)
            .and_then(|s| Pubkey::from_str(s.trim()).ok())
            .ok_or_else(|| err(400, "pubkey is not a valid public key"))?;
        let asset = match body.get("asset") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.trim()),
            Some(_) => return Err(err(400, "asset must be a string")),
        };
        let claim = self.claim_for(app, asset)?;

        let key = format!("{owner}:{}", claim.symbol);
        let token = self.cooldowns.take(&key).map_err(|remaining| {
            err(
                429,
                format!(
                    "faucet cooldown: try again in {} min",
                    cooldown_minutes(remaining)
                ),
            )
        })?;
        let signature = match self.mint(app, &owner, &claim).await {
            Ok(sig) => sig,
            Err(e) => {
                self.cooldowns.release(&key, token);
                tracing::warn!(owner = %owner, asset = %claim.symbol, "faucet mint failed: {e}");
                return Err(err(502, format!("faucet mint failed: {e}")));
            }
        };

        // Devnet SOL is rate-limited at the RPC; failure here is not an error for the claim.
        let lamports = (app.config.faucet_sol_airdrop * LAMPORTS_PER_SOL).round();
        let sol_signature = if lamports >= 1.0 && lamports < u64::MAX as f64 {
            match app
                .rpc
                .request_airdrop(&owner.to_string(), lamports as u64)
                .await
            {
                Ok(sig) => Some(sig),
                Err(e) => {
                    tracing::debug!(owner = %owner, "airdrop skipped: {e}");
                    None
                }
            }
        } else {
            None
        };
        tracing::info!(
            owner = %owner, asset = %claim.symbol, amount = claim.amount, signature = %signature,
            airdrop = sol_signature.is_some(), "faucet claim"
        );
        Ok(json!({
            "pubkey": owner.to_string(),
            "asset": claim.symbol,
            "amount": num(claim.amount),
            // Kept for older frontends, which only ever asked for USDC.
            "usdc_amount": if claim.symbol == "USDC" { num(claim.amount) } else { json!(0) },
            "signature": signature,
            "usdc_signature": signature,
            "sol_airdrop_signature": sol_signature,
        }))
    }

    /// Blockhash, sign, send, then wait for `confirmed`.
    async fn mint(&self, app: &App, owner: &Pubkey, claim: &Claim) -> Result<String, String> {
        let rpc = &app.rpc;
        let blockhash = rpc
            .get_latest_blockhash()
            .await
            .map_err(|e| e.to_string())?;
        let blockhash =
            Hash::from_str(&blockhash).map_err(|_| "rpc returned an invalid blockhash")?;
        let tx = build_mint_tx(
            &self.authority,
            owner,
            &claim.mint,
            claim.base_units,
            blockhash,
        )?;
        let signature = rpc
            .send_transaction(&encode_tx(&tx)?)
            .await
            .map_err(|e| e.to_string())?;
        confirm(
            rpc,
            &signature,
            app.config.faucet_confirm_timeout,
            app.config.faucet_poll_interval,
        )
        .await?;
        Ok(signature)
    }
}

/// `round(amount × 10^decimals)` when it fits a u64.
fn base_units(amount: f64, decimals: u32) -> Option<u64> {
    let scale = format!("1e{decimals}").parse::<f64>().ok()?;
    let base = (amount * scale).round();
    (base.is_finite() && base >= 0.0 && base < u64::MAX as f64).then_some(base as u64)
}

/// Polls `getSignatureStatuses` until `confirmed`/`finalized`, the transaction fails,
/// or `timeout` passes. Poll errors are retried until the deadline.
async fn confirm(
    rpc: &SolanaRpc,
    signature: &str,
    timeout: Duration,
    interval: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        match rpc.get_signature_status(signature).await {
            Ok(Some(status)) if status.err.as_ref().is_some_and(|e| !e.is_null()) => {
                let why = status.err.unwrap_or_default();
                return Err(format!("transaction {signature} failed: {why}"));
            }
            Ok(Some(status)) if status.is_confirmed() => return Ok(()),
            Ok(_) => {}
            Err(e) => tracing::debug!("signature status poll failed: {e}"),
        }
        if Instant::now() + interval > deadline {
            return Err(format!(
                "transaction {signature} not confirmed within {} s",
                timeout.as_secs_f64()
            ));
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    // Expected values from @solana/spl-token 0.4.15 (getAssociatedTokenAddressSync with
    // allowOwnerOffCurve = true, and the two instruction builders).
    const USDC: &str = "EdmxWPmx2WH6WgFfTdu9xfkYf3k1g5wD1zccTVySEEh1";
    const USER: &str = "GyGKxMyg1p9SsHfm15MkNUu1u9TN2JtTspcdmrtGUdse";
    const MM_A: &str = "AKnL4NNf3DGWZJS6cPknBuEGnVsV4A4m5tgebLHaRSZ9";
    /// Keypair::fromSeed([9; 32]).
    const TSLA: &str = "J2xccRtuG43drESLYznHhLhQkLTdfepcKYbiQ9BsJVaf";
    /// A PDA (off curve) of the breezepocket program, seed "vault".
    const PDA: &str = "743x8c6yXs6x2KBKhPinA1Q3mAkKzndcjWwiAwtZxFmA";
    /// Keypair::fromSeed([7; 32]).
    const AUTHORITY: &str = "GmaDrppBC7P5ARKV8g3djiwP89vz1jLK23V2GBjuAEGB";
    const ATAS: [(&str, &str, &str); 4] = [
        (USER, USDC, "BW6GAT1EsPR5sPEzLefSBhGSbfpkJmGG1M9ShhQZnQMn"),
        (MM_A, USDC, "AS4agAYaHGkZ7qvSRkNeNjPcoricha1WhSko5qv28eLY"),
        (USER, TSLA, "FUoM9ABbX4do8UDZaffPR3R4WHZMpBXb9sPS3tPtsdWr"),
        (PDA, USDC, "6HZ6FVZmiWAhJdJNbovaQNngRd5bzvQbjTGtWKimE6Dp"),
    ];

    fn pk(s: &str) -> Pubkey {
        Pubkey::from_str(s).unwrap()
    }

    fn authority() -> Keypair {
        Keypair::new_from_array([7; 32])
    }

    fn metas(ix: &Instruction) -> Vec<(String, bool, bool)> {
        ix.accounts
            .iter()
            .map(|m| (m.pubkey.to_string(), m.is_signer, m.is_writable))
            .collect()
    }

    fn meta(k: &str, signer: bool, writable: bool) -> (String, bool, bool) {
        (k.to_string(), signer, writable)
    }

    #[test]
    fn ata_matches_spl_token() {
        assert_eq!(authority().pubkey().to_string(), AUTHORITY);
        for (owner, mint, ata) in ATAS {
            assert_eq!(
                ata_address(&pk(owner), &pk(mint)).to_string(),
                ata,
                "{owner}/{mint}"
            );
        }
    }

    #[test]
    fn instructions_match_spl_token() {
        let ata = pk(ATAS[0].2);
        let create = create_ata_idempotent_ix(&pk(AUTHORITY), &ata, &pk(USER), &pk(USDC));
        assert_eq!(create.program_id.to_string(), ATA_PROGRAM_ID.to_string());
        assert_eq!(create.data, vec![1]);
        assert_eq!(
            metas(&create),
            vec![
                meta(AUTHORITY, true, true),
                meta(ATAS[0].2, false, true),
                meta(USER, false, false),
                meta(USDC, false, false),
                meta("11111111111111111111111111111111", false, false),
                meta("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA", false, false),
            ]
        );
        let mint = mint_to_ix(&pk(USDC), &ata, &pk(AUTHORITY), 1_000_000_000);
        assert_eq!(mint.program_id, TOKEN_PROGRAM_ID);
        assert_eq!(mint.data, vec![7, 0, 202, 154, 59, 0, 0, 0, 0]);
        assert_eq!(
            metas(&mint),
            vec![
                meta(USDC, false, true),
                meta(ATAS[0].2, false, true),
                meta(AUTHORITY, true, false),
            ]
        );
    }

    #[test]
    fn built_transaction_round_trips_and_verifies() {
        let kp = authority();
        let blockhash = Hash::new_from_array([3; 32]);
        let tx = build_mint_tx(&kp, &pk(USER), &pk(USDC), 42, blockhash.clone()).unwrap();
        let wire = B64.decode(encode_tx(&tx).unwrap()).unwrap();
        let back: Transaction = wincode::deserialize_exact(&wire).unwrap();
        assert_eq!(back, tx);

        let msg = &back.message;
        assert_eq!(msg.account_keys[0].to_string(), AUTHORITY, "fee payer");
        assert_eq!(msg.header.num_required_signatures, 1);
        assert_eq!(msg.recent_blockhash, blockhash);
        assert_eq!(back.signatures.len(), 1);
        let vk = VerifyingKey::from_bytes(msg.account_keys[0].as_array()).unwrap();
        let sig = Signature::from_bytes(back.signatures[0].as_array());
        vk.verify(&back.message_data(), &sig).unwrap();

        let programs: Vec<String> = msg
            .instructions
            .iter()
            .map(|ix| msg.account_keys[usize::from(ix.program_id_index)].to_string())
            .collect();
        assert_eq!(
            programs,
            [ATA_PROGRAM_ID.to_string(), TOKEN_PROGRAM_ID.to_string()]
        );
        assert_eq!(
            msg.instructions[1].data,
            [&[7u8][..], &42u64.to_le_bytes()].concat()
        );
    }

    #[test]
    fn to_precision4_matches_js() {
        // [x, Number(x.toPrecision(4))] from Node.
        let cases: &[(f64, f64)] = &[
            (1000.0 / 200.12, 4.997),
            (1000.0 / 3.0, 333.3),
            (12345.0, 12350.0),
            (12355.0, 12360.0),
            (1012.5, 1013.0),
            (101.25, 101.3),
            (99995.0, 100000.0),
            (0.99995, 1.0),
            (1.0005, 1.0),
            (1000.0 / 0.07, 14290.0),
            (0.00012345, 0.0001234),
            (2.5e-7, 2.5e-7),
            (123456789.0, 123500000.0),
            (5.0, 5.0),
            (1000.0 / 7.0, 142.9),
            (1000.0 / 412.37, 2.425),
            (0.125, 0.125),
            (8.125, 8.125),
            (0.03125, 0.03125),
            (1.2345e25, 1.234e25),
            (1000.0 / 1e-9, 1e12),
            (0.0, 0.0),
        ];
        for (x, want) in cases {
            assert_eq!(to_precision4(*x), *want, "{x}");
        }
        assert_eq!(to_precision4(-12345.0), -12350.0);
    }

    #[test]
    fn base_units_and_cooldown_minutes() {
        assert_eq!(base_units(1000.0, 6), Some(1_000_000_000));
        assert_eq!(base_units(4.997, 9), Some(4_997_000_000));
        assert_eq!(base_units(0.1234, 8), Some(12_340_000));
        assert_eq!(base_units(1.0, 30), None);
        assert_eq!(base_units(f64::INFINITY, 0), None);
        assert_eq!(cooldown_minutes(Duration::from_millis(3_600_000)), 60);
        assert_eq!(cooldown_minutes(Duration::from_millis(60_001)), 2);
        assert_eq!(cooldown_minutes(Duration::from_micros(10)), 1);
    }

    #[test]
    fn cooldown_slots_release_only_their_own() {
        let c = Cooldowns::new(Duration::from_secs(60));
        let t1 = c.take("w:USDC").unwrap();
        assert!(c.take("w:USDC").is_err());
        assert!(c.take("w:TSLAon").is_ok(), "per symbol");
        c.release("w:USDC", t1 + 100);
        assert!(c.take("w:USDC").is_err(), "a stale token frees nothing");
        c.release("w:USDC", t1);
        assert!(c.take("w:USDC").is_ok());
    }

    #[test]
    fn keypair_file_loading() {
        let dir = std::env::temp_dir().join(format!("faucet-kp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let good = dir.join("good.json");
        let bytes = authority().to_bytes().to_vec();
        std::fs::write(&good, serde_json::to_string(&bytes).unwrap()).unwrap();
        assert_eq!(load_keypair(&good).unwrap().pubkey().to_string(), AUTHORITY);

        let short = dir.join("short.json");
        std::fs::write(&short, "[1,2,3]").unwrap();
        assert!(load_keypair(&short).unwrap_err().contains("expected 64"));
        let mismatched = dir.join("mismatched.json");
        let mut bad = bytes.clone();
        bad[63] ^= 1;
        std::fs::write(&mismatched, serde_json::to_string(&bad).unwrap()).unwrap();
        let e = load_keypair(&mismatched).unwrap_err();
        assert!(e.contains("does not match"), "{e}");
        assert!(!e.contains('['), "never echoes the key bytes: {e}");
        assert!(load_keypair(&dir.join("missing.json")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
