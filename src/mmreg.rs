//! Market-maker registration (contract §5.1-§5.5): who may connect and how much each
//! market maker may carry. Records live in one JSON file (`MM_REGISTRY_PATH`) that is
//! rewritten atomically on every change; nothing but the endpoints changes it.
//!
//! Not to be confused with [`crate::registry`], which tracks live connections.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::auth::signature_valid;
use crate::config::is_pubkey;
use crate::protocol::num;

/// The signed text is this prefix followed by the exact `application` string (§5.2).
pub const APPLY_PREFIX: &str = "paytience-mm-apply:v1:";
/// §5.2: `timestamp_ms` must be within this of the backend clock.
pub const MAX_CLOCK_SKEW_MS: u64 = 10 * 60 * 1000;
/// §5.2: non-approved records kept; a new application past this is refused (429).
pub const MAX_NON_APPROVED: usize = 200;
const MAX_NAME_CHARS: usize = 64;
const MAX_CONTACT_CHARS: usize = 128;
const MAX_ASSET_CAPS: usize = 64;
const MAX_SYMBOL_CHARS: usize = 16;
const FILE_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Approved,
    Rejected,
    Suspended,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pending => "pending",
            Status::Approved => "approved",
            Status::Rejected => "rejected",
            Status::Suspended => "suspended",
        }
    }
}

/// USD notional caps at the fixed price: one total and one per asset symbol
/// (upper-cased). An asset without an entry may not be quoted at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Caps {
    pub cap_total_usd: f64,
    pub asset_caps_usd: BTreeMap<String, f64>,
}

impl Caps {
    /// Convenience for seeds and tests; symbols are upper-cased.
    pub fn new(cap_total_usd: f64, assets: &[(&str, f64)]) -> Self {
        Self {
            cap_total_usd,
            asset_caps_usd: assets
                .iter()
                .map(|(symbol, usd)| (symbol.to_ascii_uppercase(), *usd))
                .collect(),
        }
    }

    /// The cap for an upper-cased symbol, if this MM may quote it.
    pub fn asset_cap(&self, symbol: &str) -> Option<f64> {
        self.asset_caps_usd.get(symbol).copied()
    }

    pub fn to_json(&self) -> Value {
        let assets: Map<String, Value> = self
            .asset_caps_usd
            .iter()
            .map(|(symbol, usd)| (symbol.clone(), num(*usd)))
            .collect();
        json!({ "cap_total_usd": num(self.cap_total_usd), "asset_caps_usd": assets })
    }
}

/// One market maker's record (§5.1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MmRecord {
    pub mm_pubkey: String,
    pub name: String,
    pub contact: String,
    pub status: Status,
    pub requested: Caps,
    /// Null until an admin approves; only these caps are enforced.
    pub approved: Option<Caps>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

impl MmRecord {
    /// An already approved record, for seeding a registry (tests).
    pub fn approved(mm_pubkey: impl Into<String>, name: impl Into<String>, caps: Caps) -> Self {
        Self {
            mm_pubkey: mm_pubkey.into(),
            name: name.into(),
            contact: "seed".into(),
            status: Status::Approved,
            requested: caps.clone(),
            approved: Some(caps),
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    /// The caps in force: only while the status is `approved`.
    pub fn enforced(&self) -> Option<&Caps> {
        match self.status {
            Status::Approved => self.approved.as_ref(),
            _ => None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "mm_pubkey": self.mm_pubkey,
            "name": self.name,
            "contact": self.contact,
            "status": self.status.as_str(),
            "requested": self.requested.to_json(),
            "approved": self.approved.as_ref().map(Caps::to_json),
            "created_at_ms": self.created_at_ms,
            "updated_at_ms": self.updated_at_ms,
        })
    }

    /// The public view (§5.3): no name, contact or usage.
    pub fn status_json(&self) -> Value {
        json!({
            "mm_pubkey": self.mm_pubkey,
            "status": self.status.as_str(),
            "approved": self.approved.as_ref().map(Caps::to_json),
        })
    }
}

// ---------------------------------------------------------------------------
// Validation (§5.2)

/// A verified, validated application.
#[derive(Debug, Clone, PartialEq)]
pub struct Application {
    pub mm_pubkey: String,
    pub name: String,
    pub contact: String,
    pub caps: Caps,
}

/// The exact bytes an MM signs for `POST /mm/apply`.
pub fn apply_message(application: &str) -> String {
    format!("{APPLY_PREFIX}{application}")
}

/// Checks a `POST /mm/apply` body in the contract's order: pubkey, signature,
/// timestamp, name, contact, caps. The error is the 400 message.
pub fn parse_application(body: &Value, now_ms: u64) -> Result<Application, String> {
    let Some(raw) = body.get("application").and_then(Value::as_str) else {
        return Err("application must be a JSON string".into());
    };
    let app: Value =
        serde_json::from_str(raw).map_err(|_| "application is not valid JSON".to_string())?;
    if !app.is_object() {
        return Err("application must be a JSON object".into());
    }
    let mm_pubkey = match app.get("mm_pubkey").and_then(Value::as_str) {
        Some(k) if is_pubkey(k) => k,
        _ => return Err("mm_pubkey is not a valid public key".into()),
    };
    let signature = body
        .get("signature")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !signature_valid(mm_pubkey, signature, apply_message(raw).as_bytes()) {
        return Err("bad signature".into());
    }
    let within = app
        .get("timestamp_ms")
        .and_then(crate::protocol::as_f64)
        .is_some_and(|ts| (ts - now_ms as f64).abs() <= MAX_CLOCK_SKEW_MS as f64);
    if !within {
        return Err("timestamp_ms is not within 10 minutes of the backend clock".into());
    }
    let name = text_field(&app, "name", MAX_NAME_CHARS)?;
    let contact = text_field(&app, "contact", MAX_CONTACT_CHARS)?;
    let caps = parse_caps(app.get("cap_total_usd"), app.get("asset_caps_usd"))?;
    Ok(Application {
        mm_pubkey: mm_pubkey.to_string(),
        name,
        contact,
        caps,
    })
}

/// A trimmed string of 1..=`max` chars without control characters.
fn text_field(app: &Value, key: &str, max: usize) -> Result<String, String> {
    let bad = || format!("{key} must be 1-{max} characters without control characters");
    let value = app.get(key).and_then(Value::as_str).ok_or_else(bad)?.trim();
    let chars = value.chars().count();
    if chars == 0 || chars > max || value.chars().any(char::is_control) {
        return Err(bad());
    }
    Ok(value.to_string())
}

fn positive(v: &Value) -> Option<f64> {
    crate::protocol::as_f64(v).filter(|n| *n > 0.0)
}

/// `cap_total_usd` finite and > 0; 1-64 asset caps keyed by 1-16 chars of
/// `[A-Za-z0-9]` (stored upper-cased), each finite, > 0 and at most the total.
pub fn parse_caps(total: Option<&Value>, assets: Option<&Value>) -> Result<Caps, String> {
    let cap_total_usd = total
        .and_then(positive)
        .ok_or("cap_total_usd must be a number > 0")?;
    let entries = assets
        .and_then(Value::as_object)
        .ok_or("asset_caps_usd must be an object")?;
    if entries.is_empty() || entries.len() > MAX_ASSET_CAPS {
        return Err(format!(
            "asset_caps_usd must have 1-{MAX_ASSET_CAPS} entries"
        ));
    }
    let mut asset_caps_usd = BTreeMap::new();
    for (key, value) in entries {
        let symbol_ok = !key.is_empty()
            && key.len() <= MAX_SYMBOL_CHARS
            && key.bytes().all(|b| b.is_ascii_alphanumeric());
        if !symbol_ok {
            return Err(format!(
                "asset_caps_usd key {key:?} must be 1-{MAX_SYMBOL_CHARS} characters of A-Z, a-z, 0-9"
            ));
        }
        let usd =
            positive(value).ok_or_else(|| format!("asset_caps_usd.{key} must be a number > 0"))?;
        if usd > cap_total_usd {
            return Err(format!("asset_caps_usd.{key} exceeds cap_total_usd"));
        }
        let symbol = key.to_ascii_uppercase();
        if asset_caps_usd.insert(symbol.clone(), usd).is_some() {
            return Err(format!("asset_caps_usd lists {symbol} twice"));
        }
    }
    Ok(Caps {
        cap_total_usd,
        asset_caps_usd,
    })
}

// ---------------------------------------------------------------------------
// The registry

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegError {
    #[error("unknown market maker")]
    NotFound,
    #[error("{0}")]
    Invalid(String),
    #[error("too many pending applications; try again later")]
    Full,
    /// The file could not be written; the change was not applied.
    #[error("cannot save the registry: {0}")]
    Storage(String),
}

#[derive(Serialize, Deserialize)]
struct FileModel {
    version: u32,
    mms: Vec<MmRecord>,
}

/// Records in creation order, behind one lock; every change is persisted before it
/// becomes visible, so memory never runs ahead of the file.
pub struct MmRegistry {
    /// None keeps the registry in memory only (tests).
    path: Option<PathBuf>,
    records: Mutex<Vec<MmRecord>>,
}

impl MmRegistry {
    /// A registry that is never written to disk.
    pub fn in_memory(records: Vec<MmRecord>) -> Self {
        Self {
            path: None,
            records: Mutex::new(records),
        }
    }

    /// Loads `path`: a missing file is an empty registry, a malformed one an error.
    pub fn load(path: &Path) -> Result<Self, String> {
        let records = match std::fs::read(path) {
            Ok(bytes) => {
                let model: FileModel = serde_json::from_slice(&bytes)
                    .map_err(|e| format!("{} is malformed: {e}", path.display()))?;
                check_records(&model.mms).map_err(|e| format!("{}: {e}", path.display()))?;
                model.mms
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        Ok(Self {
            path: Some(path.to_path_buf()),
            records: Mutex::new(records),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Vec<MmRecord>> {
        self.records.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies `f` to a copy, persists the copy, then swaps it in.
    fn mutate<T>(
        &self,
        f: impl FnOnce(&mut Vec<MmRecord>) -> Result<T, RegError>,
    ) -> Result<T, RegError> {
        let mut records = self.lock();
        let mut next = records.clone();
        let out = f(&mut next)?;
        if let Some(path) = &self.path {
            persist(path, &next).map_err(|e| RegError::Storage(e.to_string()))?;
        }
        *records = next;
        Ok(out)
    }

    /// Adds records whose pubkey is not present yet (test seeding).
    pub fn seed(&self, seeds: &[MmRecord]) -> Result<(), RegError> {
        if seeds.is_empty() {
            return Ok(());
        }
        self.mutate(|records| {
            for seed in seeds {
                if !records.iter().any(|r| r.mm_pubkey == seed.mm_pubkey) {
                    records.push(seed.clone());
                }
            }
            Ok(())
        })
    }

    /// §5.2: stores an application and returns the record's status afterwards.
    pub fn apply(&self, app: Application, now_ms: u64) -> Result<Status, RegError> {
        self.mutate(|records| {
            match records.iter_mut().find(|r| r.mm_pubkey == app.mm_pubkey) {
                Some(record) => {
                    record.name = app.name;
                    record.contact = app.contact;
                    record.requested = app.caps;
                    record.updated_at_ms = now_ms;
                    // An approved or suspended MM keeps its status and approved caps: a
                    // cap change takes effect only when an admin approves again.
                    if matches!(record.status, Status::Pending | Status::Rejected) {
                        record.status = Status::Pending;
                    }
                    Ok(record.status)
                }
                None => {
                    let waiting = records
                        .iter()
                        .filter(|r| r.status != Status::Approved)
                        .count();
                    if waiting >= MAX_NON_APPROVED {
                        return Err(RegError::Full);
                    }
                    records.push(MmRecord {
                        mm_pubkey: app.mm_pubkey,
                        name: app.name,
                        contact: app.contact,
                        status: Status::Pending,
                        requested: app.caps,
                        approved: None,
                        created_at_ms: now_ms,
                        updated_at_ms: now_ms,
                    });
                    Ok(Status::Pending)
                }
            }
        })
    }

    /// §5.4 approve: omitted fields default to the requested values; works from any
    /// status.
    pub fn approve(
        &self,
        mm_pubkey: &str,
        body: &Value,
        now_ms: u64,
    ) -> Result<MmRecord, RegError> {
        self.mutate(|records| {
            let record = find(records, mm_pubkey)?;
            let given = |key: &str| body.get(key).filter(|v| !v.is_null()).cloned();
            let total =
                given("cap_total_usd").unwrap_or_else(|| num(record.requested.cap_total_usd));
            let assets = given("asset_caps_usd")
                .unwrap_or_else(|| record.requested.to_json()["asset_caps_usd"].clone());
            let caps = parse_caps(Some(&total), Some(&assets)).map_err(RegError::Invalid)?;
            record.status = Status::Approved;
            record.approved = Some(caps);
            record.updated_at_ms = now_ms;
            Ok(record.clone())
        })
    }

    /// §5.4 reject: the approved caps are cleared.
    pub fn reject(&self, mm_pubkey: &str, now_ms: u64) -> Result<MmRecord, RegError> {
        self.mutate(|records| {
            let record = find(records, mm_pubkey)?;
            record.status = Status::Rejected;
            record.approved = None;
            record.updated_at_ms = now_ms;
            Ok(record.clone())
        })
    }

    /// §5.4 suspend: the approved caps are kept for a later approve.
    pub fn suspend(&self, mm_pubkey: &str, now_ms: u64) -> Result<MmRecord, RegError> {
        self.mutate(|records| {
            let record = find(records, mm_pubkey)?;
            record.status = Status::Suspended;
            record.updated_at_ms = now_ms;
            Ok(record.clone())
        })
    }

    pub fn delete(&self, mm_pubkey: &str) -> Result<(), RegError> {
        self.mutate(|records| {
            let before = records.len();
            records.retain(|r| r.mm_pubkey != mm_pubkey);
            if records.len() == before {
                return Err(RegError::NotFound);
            }
            Ok(())
        })
    }

    pub fn get(&self, mm_pubkey: &str) -> Option<MmRecord> {
        self.lock()
            .iter()
            .find(|r| r.mm_pubkey == mm_pubkey)
            .cloned()
    }

    /// Every record, in creation order.
    pub fn list(&self) -> Vec<MmRecord> {
        self.lock().clone()
    }

    /// The display name of an approved MM (§5.5: only those may connect).
    pub fn approved_name(&self, mm_pubkey: &str) -> Option<String> {
        self.lock()
            .iter()
            .find(|r| r.mm_pubkey == mm_pubkey && r.enforced().is_some())
            .map(|r| r.name.clone())
    }

    /// The caps in force for an approved MM.
    pub fn approved_caps(&self, mm_pubkey: &str) -> Option<Caps> {
        self.lock()
            .iter()
            .find(|r| r.mm_pubkey == mm_pubkey)
            .and_then(|r| r.enforced().cloned())
    }

    /// Pubkeys of every approved MM, in creation order.
    pub fn approved_pubkeys(&self) -> Vec<String> {
        self.lock()
            .iter()
            .filter(|r| r.enforced().is_some())
            .map(|r| r.mm_pubkey.clone())
            .collect()
    }

    pub fn approved_count(&self) -> usize {
        self.lock()
            .iter()
            .filter(|r| r.enforced().is_some())
            .count()
    }
}

fn find<'a>(records: &'a mut [MmRecord], mm_pubkey: &str) -> Result<&'a mut MmRecord, RegError> {
    records
        .iter_mut()
        .find(|r| r.mm_pubkey == mm_pubkey)
        .ok_or(RegError::NotFound)
}

fn check_records(records: &[MmRecord]) -> Result<(), String> {
    for (i, record) in records.iter().enumerate() {
        if !is_pubkey(&record.mm_pubkey) {
            return Err(format!("record {i} has an invalid mm_pubkey"));
        }
        if records
            .iter()
            .take(i)
            .any(|r| r.mm_pubkey == record.mm_pubkey)
        {
            return Err(format!("{} is listed twice", record.mm_pubkey));
        }
    }
    Ok(())
}

/// Temp file + rename, so a crash never leaves a half-written registry. The file may
/// hold contact details: mode 0600 on unix. The parent directory is created if missing.
fn persist(path: &Path, records: &[MmRecord]) -> std::io::Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let model = FileModel {
        version: FILE_VERSION,
        mms: records.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&model).map_err(std::io::Error::other)?;
    bytes.push(b'\n');

    let mut tmp = path.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    #[cfg(unix)]
    {
        // A leftover temp file keeps its old mode; `mode` above only applies on create.
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(&bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use base64::engine::general_purpose::STANDARD as B64;
    use ed25519_dalek::{Signer, SigningKey};

    // Signed with node:crypto through the desk's own signApplication helper; see
    // tests/fixtures/gen-mm-apply.mjs for how it was generated.
    const FIXTURE: &str = include_str!("../tests/fixtures/mm-apply.json");
    const NOW: u64 = 1_759_000_000_000;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    fn pubkey(k: &SigningKey) -> String {
        bs58::encode(k.verifying_key().as_bytes()).into_string()
    }

    fn application(k: &SigningKey) -> Value {
        json!({ "mm_pubkey": pubkey(k), "name": "Acme MM", "contact": "ops@acme.example",
                "cap_total_usd": 20000, "asset_caps_usd": { "SOL": 10000, "wbtc": 5000 },
                "timestamp_ms": NOW })
    }

    fn signed(k: &SigningKey, application: &Value) -> Value {
        let raw = application.to_string();
        let sig = k.sign(apply_message(&raw).as_bytes());
        json!({ "application": raw, "signature": B64.encode(sig.to_bytes()) })
    }

    fn error_of(mutate: impl FnOnce(&mut Value)) -> String {
        let k = key(1);
        let mut app = application(&k);
        mutate(&mut app);
        parse_application(&signed(&k, &app), NOW).unwrap_err()
    }

    #[test]
    fn verifies_an_application_signed_by_node() {
        let fx: Value = serde_json::from_str(FIXTURE).unwrap();
        let body = &fx["body"];
        let raw = body["application"].as_str().unwrap();
        assert_eq!(fx["message"].as_str().unwrap(), apply_message(raw));
        let now = fx["timestamp_ms"].as_u64().unwrap();
        let app = parse_application(body, now).unwrap();
        assert_eq!(app.mm_pubkey, fx["pubkey"].as_str().unwrap());
        assert_eq!(app.mm_pubkey, pubkey(&key(1)), "same seed, same key");
        assert_eq!(app.name, "Acme MM");
        assert_eq!(app.contact, "ops@acme.example");
        assert_eq!(
            app.caps,
            Caps::new(20000.0, &[("SOL", 10000.0), ("WBTC", 5000.0)])
        );

        // One changed byte in the string no longer verifies.
        let mut tampered = body.clone();
        tampered["application"] = json!(raw.replace("20000", "20001"));
        assert_eq!(
            parse_application(&tampered, now).unwrap_err(),
            "bad signature"
        );
    }

    #[test]
    fn accepts_a_valid_application_and_upper_cases_symbols() {
        let k = key(1);
        let app = parse_application(&signed(&k, &application(&k)), NOW).unwrap();
        assert_eq!(
            app.caps.asset_caps_usd.keys().collect::<Vec<_>>(),
            ["SOL", "WBTC"]
        );
        assert_eq!(app.caps.asset_cap("WBTC"), Some(5000.0));
        assert_eq!(app.caps.asset_cap("ETH"), None);
    }

    #[test]
    fn rejects_bad_envelopes_and_signatures() {
        let k = key(1);
        let good = signed(&k, &application(&k));
        assert!(parse_application(&json!({}), NOW).is_err());
        assert!(
            parse_application(
                &json!({ "application": application(&k), "signature": "x" }),
                NOW
            )
            .unwrap_err()
            .contains("JSON string"),
            "the application must be a string, not an object"
        );
        assert!(
            parse_application(&json!({ "application": "{", "signature": "x" }), NOW)
                .unwrap_err()
                .contains("not valid JSON")
        );
        assert!(parse_application(&json!({ "application": "[]", "signature": "x" }), NOW).is_err());

        let mut no_sig = good.clone();
        no_sig.as_object_mut().unwrap().remove("signature");
        assert_eq!(
            parse_application(&no_sig, NOW).unwrap_err(),
            "bad signature"
        );

        // Signed by another key than the one it names.
        let other = signed(&key(2), &application(&k));
        assert_eq!(parse_application(&other, NOW).unwrap_err(), "bad signature");

        assert!(error_of(|a| a["mm_pubkey"] = json!("nope")).contains("public key"));
    }

    #[test]
    fn timestamp_must_be_within_ten_minutes() {
        let k = key(1);
        let body = signed(&k, &application(&k));
        assert!(parse_application(&body, NOW + MAX_CLOCK_SKEW_MS).is_ok());
        assert!(parse_application(&body, NOW - MAX_CLOCK_SKEW_MS).is_ok());
        assert!(parse_application(&body, NOW + MAX_CLOCK_SKEW_MS + 1).is_err());
        assert!(parse_application(&body, NOW - MAX_CLOCK_SKEW_MS - 1).is_err());
        assert!(error_of(|a| a["timestamp_ms"] = json!("now")).contains("timestamp_ms"));
        assert!(
            error_of(|a| {
                a.as_object_mut().unwrap().remove("timestamp_ms");
            })
            .contains("timestamp_ms")
        );
    }

    #[test]
    fn name_and_contact_rules() {
        assert!(error_of(|a| a["name"] = json!("   ")).contains("name"));
        assert!(error_of(|a| a["name"] = json!("x".repeat(65))).contains("name"));
        assert!(error_of(|a| a["name"] = json!("a\nb")).contains("name"));
        assert!(error_of(|a| a["name"] = json!(5)).contains("name"));
        assert!(error_of(|a| a["contact"] = json!("")).contains("contact"));
        assert!(error_of(|a| a["contact"] = json!("x".repeat(129))).contains("contact"));
        assert!(error_of(|a| a["contact"] = json!("a\u{7}b")).contains("contact"));

        let k = key(1);
        let mut app = application(&k);
        app["name"] = json!(format!("  {}  ", "é".repeat(64)));
        app["contact"] = json!("x".repeat(128));
        let parsed = parse_application(&signed(&k, &app), NOW).unwrap();
        assert_eq!(parsed.name, "é".repeat(64), "trimmed; chars, not bytes");
    }

    #[test]
    fn cap_rules() {
        assert!(error_of(|a| a["cap_total_usd"] = json!(0)).contains("cap_total_usd"));
        assert!(error_of(|a| a["cap_total_usd"] = json!(-5)).contains("cap_total_usd"));
        assert!(error_of(|a| a["cap_total_usd"] = json!("20000")).contains("cap_total_usd"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({})).contains("1-64"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!([1])).contains("object"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({ "SOL": 0 })).contains("SOL"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({ "SOL": "1" })).contains("SOL"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({ "SOL": 20000.01 })).contains("exceeds"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({ "S-L": 1 })).contains("S-L"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({ "": 1 })).contains("1-16"));
        assert!(error_of(|a| a["asset_caps_usd"] = json!({ "A".repeat(17): 1 })).contains("1-16"));
        assert!(
            error_of(|a| a["asset_caps_usd"] = json!({ "sol": 1, "SOL": 2 })).contains("twice")
        );
        let many: Map<String, Value> = (0..65).map(|i| (format!("A{i}"), json!(1))).collect();
        assert!(error_of(|a| a["asset_caps_usd"] = Value::Object(many)).contains("1-64"));

        // The limits themselves pass.
        let edge: Map<String, Value> = (0..64).map(|i| (format!("A{i}"), json!(20000))).collect();
        let caps = parse_caps(Some(&json!(20000)), Some(&Value::Object(edge))).unwrap();
        assert_eq!(caps.asset_caps_usd.len(), 64);
        assert!(parse_caps(Some(&json!(1)), Some(&json!({ "A".repeat(16): 0.5 }))).is_ok());
    }

    fn app(seed: u8, total: f64) -> Application {
        Application {
            mm_pubkey: pubkey(&key(seed)),
            name: format!("mm-{seed}"),
            contact: "c".into(),
            caps: Caps::new(total, &[("SOL", total)]),
        }
    }

    #[test]
    fn state_transitions() {
        let reg = MmRegistry::in_memory(Vec::new());
        let pk = pubkey(&key(1));
        assert_eq!(reg.apply(app(1, 100.0), 10).unwrap(), Status::Pending);
        assert!(reg.approved_name(&pk).is_none(), "pending may not connect");
        assert!(reg.approved_caps(&pk).is_none());

        // Re-applying while pending replaces the request.
        assert_eq!(reg.apply(app(1, 200.0), 11).unwrap(), Status::Pending);
        let r = reg.get(&pk).unwrap();
        assert_eq!(
            (r.requested.cap_total_usd, r.created_at_ms, r.updated_at_ms),
            (200.0, 10, 11)
        );
        assert_eq!(r.approved, None);

        // Approve with the requested values, then with lowered ones.
        let r = reg.approve(&pk, &json!({}), 12).unwrap();
        assert_eq!(r.status, Status::Approved);
        assert_eq!(r.approved, Some(Caps::new(200.0, &[("SOL", 200.0)])));
        let lowered = json!({ "cap_total_usd": 150, "asset_caps_usd": { "sol": 50 } });
        let r = reg.approve(&pk, &lowered, 13).unwrap();
        assert_eq!(r.approved, Some(Caps::new(150.0, &[("SOL", 50.0)])));
        assert_eq!(reg.approved_name(&pk).as_deref(), Some("mm-1"));
        assert_eq!(reg.approved_pubkeys(), std::slice::from_ref(&pk));

        // A total below the requested asset caps cannot default the rest.
        assert!(matches!(
            reg.approve(&pk, &json!({ "cap_total_usd": 100 }), 14),
            Err(RegError::Invalid(_))
        ));
        assert_eq!(
            reg.get(&pk).unwrap().updated_at_ms,
            13,
            "a failed change is not applied"
        );

        // Re-applying while approved keeps the status and the approved caps.
        assert_eq!(reg.apply(app(1, 900.0), 15).unwrap(), Status::Approved);
        let r = reg.get(&pk).unwrap();
        assert_eq!(r.requested.cap_total_usd, 900.0);
        assert_eq!(r.approved, Some(Caps::new(150.0, &[("SOL", 50.0)])));

        // Suspend keeps the approved caps but enforces nothing; approve restores.
        let r = reg.suspend(&pk, 16).unwrap();
        assert_eq!(r.status, Status::Suspended);
        assert!(r.approved.is_some());
        assert!(reg.approved_name(&pk).is_none());
        assert!(reg.approved_caps(&pk).is_none());
        assert_eq!(reg.apply(app(1, 900.0), 17).unwrap(), Status::Suspended);
        assert_eq!(
            reg.approve(&pk, &json!({}), 18).unwrap().status,
            Status::Approved
        );

        // Reject clears the approved caps; re-applying goes back to pending.
        let r = reg.reject(&pk, 19).unwrap();
        assert_eq!((r.status, r.approved), (Status::Rejected, None));
        assert_eq!(reg.apply(app(1, 100.0), 20).unwrap(), Status::Pending);

        reg.delete(&pk).unwrap();
        assert!(reg.get(&pk).is_none());
        for result in [
            reg.delete(&pk).map(|_| ()),
            reg.approve(&pk, &json!({}), 1).map(|_| ()),
            reg.reject(&pk, 1).map(|_| ()),
            reg.suspend(&pk, 1).map(|_| ()),
        ] {
            assert_eq!(result, Err(RegError::NotFound));
        }
    }

    #[test]
    fn non_approved_records_are_bounded() {
        let reg = MmRegistry::in_memory(Vec::new());
        let application = |i: usize| {
            let mut seed = [0u8; 32];
            seed[..8].copy_from_slice(&(i as u64 + 1).to_le_bytes());
            Application {
                mm_pubkey: pubkey(&SigningKey::from_bytes(&seed)),
                name: "n".into(),
                contact: "c".into(),
                caps: Caps::new(1.0, &[("SOL", 1.0)]),
            }
        };
        for i in 0..MAX_NON_APPROVED {
            reg.apply(application(i), 1).unwrap();
        }
        assert_eq!(reg.apply(application(999), 1), Err(RegError::Full));
        // A known pubkey may still update its request, and approving one frees a slot.
        assert_eq!(reg.apply(application(0), 2).unwrap(), Status::Pending);
        reg.approve(&application(0).mm_pubkey, &json!({}), 3)
            .unwrap();
        assert_eq!(reg.apply(application(999), 4).unwrap(), Status::Pending);
        assert_eq!(reg.list().len(), MAX_NON_APPROVED + 1);
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("paytience-mmreg-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn persistence_round_trip() {
        let dir = temp_dir();
        // The parent directory does not exist yet.
        let path = dir.join("nested").join("mm-registry.json");
        let reg = MmRegistry::load(&path).unwrap();
        assert!(reg.list().is_empty(), "a missing file is an empty registry");
        assert!(!path.exists(), "nothing is written until something changes");

        reg.apply(app(1, 100.0), 10).unwrap();
        reg.apply(app(2, 300.0), 11).unwrap();
        reg.approve(
            &pubkey(&key(2)),
            &json!({ "cap_total_usd": 250, "asset_caps_usd": { "SOL": 200 } }),
            12,
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert!(!dir.join("nested").join("mm-registry.json.tmp").exists());

        let again = MmRegistry::load(&path).unwrap();
        assert_eq!(again.list(), reg.list());
        assert_eq!(
            again
                .list()
                .iter()
                .map(|r| r.name.as_str())
                .collect::<Vec<_>>(),
            ["mm-1", "mm-2"],
            "creation order survives"
        );
        assert_eq!(
            again.approved_caps(&pubkey(&key(2))),
            Some(Caps::new(250.0, &[("SOL", 200.0)]))
        );

        again.delete(&pubkey(&key(1))).unwrap();
        assert_eq!(MmRegistry::load(&path).unwrap().list().len(), 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_file_fails_to_load() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("mm-registry.json");
        std::fs::write(&path, b"{ not json").unwrap();
        assert!(MmRegistry::load(&path).err().unwrap().contains("malformed"));
        std::fs::write(&path, b"").unwrap();
        assert!(MmRegistry::load(&path).is_err());

        // The same pubkey twice.
        let record = MmRecord::approved(pubkey(&key(1)), "a", Caps::new(1.0, &[("SOL", 1.0)]));
        let twice = FileModel {
            version: FILE_VERSION,
            mms: vec![record.clone(), record],
        };
        std::fs::write(&path, serde_json::to_vec(&twice).unwrap()).unwrap();
        assert!(MmRegistry::load(&path).err().unwrap().contains("twice"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // The "parent directory" is a file, so the registry cannot be written.
        let blocker = dir.join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let reg = MmRegistry::load(&blocker.join("mm-registry.json")).unwrap_or_else(|_| {
            // Reading through a file fails on some platforms; fall back to a fresh path.
            MmRegistry {
                path: Some(blocker.join("mm-registry.json")),
                records: Mutex::new(Vec::new()),
            }
        });
        assert!(matches!(
            reg.apply(app(1, 1.0), 1),
            Err(RegError::Storage(_))
        ));
        assert!(reg.list().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn json_views() {
        let record = MmRecord::approved(
            pubkey(&key(1)),
            "alpha",
            Caps::new(1500.5, &[("sol", 1000.0)]),
        );
        assert_eq!(
            record.status_json(),
            json!({ "mm_pubkey": pubkey(&key(1)), "status": "approved",
                    "approved": { "cap_total_usd": 1500.5, "asset_caps_usd": { "SOL": 1000 } } })
        );
        let full = record.to_json();
        assert_eq!(full["name"], "alpha");
        assert_eq!(full["requested"], full["approved"]);
    }
}
