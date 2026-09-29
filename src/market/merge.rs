//! Pure merge rules for market data across MMs (contract §2.1-2.5). Inputs are the
//! desks' JSON bodies in MM order (oldest connection first), so every "first" and
//! "ties: first" rule below is deterministic.

use std::cmp::Ordering;
use std::collections::HashMap;

use serde_json::{Map, Value, json};

use super::MarketError;
use crate::protocol::{as_f64, num, parse_u64};

/// Median; an even count averages the two middle values. None when empty.
pub fn median(values: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = values.iter().copied().filter(|f| f.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let mid = v.len() / 2;
    Some(if v.len().is_multiple_of(2) {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    })
}

fn opt_num(v: Option<f64>) -> Value {
    v.map(num).unwrap_or(Value::Null)
}

fn numbers<'a>(items: impl IntoIterator<Item = &'a Value>, key: &str) -> Vec<f64> {
    items
        .into_iter()
        .filter_map(|v| v.get(key).and_then(as_f64))
        .collect()
}

fn max_of(values: &[f64]) -> Option<f64> {
    values.iter().copied().max_by(f64::total_cmp)
}

fn min_of(values: &[f64]) -> Option<f64> {
    values.iter().copied().min_by(f64::total_cmp)
}

fn is_true(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool) == Some(true)
}

fn field(v: &Value, key: &str) -> Value {
    v.get(key).cloned().unwrap_or(Value::Null)
}

fn array<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// The aggregated parts of `/health` (§2.1); the caller adds the backend fields.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthMerge {
    pub ok: bool,
    pub dry_run: bool,
    pub price: Value,
    pub assets: Vec<Value>,
    pub exposure: Value,
}

/// Merges fresh eligible MMs' `health` bodies. An empty slice gives the offline shape.
pub fn merge_health(snapshots: &[&Value]) -> HealthMerge {
    let prices: Vec<&Value> = snapshots.iter().filter_map(|s| s.get("price")).collect();
    let price = json!({
        "source": "aggregate",
        "spot": opt_num(median(&numbers(prices.iter().copied(), "spot"))),
        "spotAgeMs": opt_num(min_of(&numbers(prices.iter().copied(), "spotAgeMs"))),
        "vol": opt_num(median(&numbers(prices.iter().copied(), "vol"))),
        "expiries": opt_num(max_of(&numbers(prices.iter().copied(), "expiries"))),
    });

    let asset_lists: Vec<&[Value]> = snapshots.iter().map(|s| array(s, "assets")).collect();

    let (mut total, mut cap_total, mut cap_per) = (0.0, 0.0, 0.0);
    let mut per_expiry: Map<String, Value> = Map::new();
    let mut sums: HashMap<String, (f64, f64)> = HashMap::new();
    for s in snapshots {
        let Some(e) = s.get("exposure") else { continue };
        total += e.get("totalUsd").and_then(as_f64).unwrap_or(0.0);
        cap_total += e.get("capTotalUsd").and_then(as_f64).unwrap_or(0.0);
        cap_per += e.get("capPerExpiryUsd").and_then(as_f64).unwrap_or(0.0);
        if let Some(map) = e.get("perExpiry").and_then(Value::as_object) {
            for (ts, row) in map {
                let entry = sums.entry(ts.clone()).or_insert((0.0, 0.0));
                entry.0 += row.get("onChainUsd").and_then(as_f64).unwrap_or(0.0);
                entry.1 += row.get("reservedUsd").and_then(as_f64).unwrap_or(0.0);
            }
        }
    }
    for (ts, (on_chain, reserved)) in sums {
        per_expiry.insert(
            ts,
            json!({ "onChainUsd": num(on_chain), "reservedUsd": num(reserved) }),
        );
    }

    HealthMerge {
        ok: snapshots.iter().any(|s| is_true(s, "ok")),
        dry_run: !snapshots.is_empty() && snapshots.iter().all(|s| is_true(s, "dry_run")),
        price,
        assets: merge_assets(&asset_lists),
        exposure: json!({
            "totalUsd": num(total),
            "capTotalUsd": num(cap_total),
            "capPerExpiryUsd": num(cap_per),
            "perExpiry": per_expiry,
        }),
    }
}

/// §2.2: one list per MM, merged by case-insensitive `asset`; SOL first, then first seen.
pub fn merge_assets(lists: &[&[Value]]) -> Vec<Value> {
    // (lowercase key, one entry per MM)
    let mut groups: Vec<(String, Vec<&Value>)> = Vec::new();
    for list in lists {
        let mut seen_here: Vec<String> = Vec::new();
        for entry in *list {
            let Some(asset) = entry.get("asset").and_then(Value::as_str) else {
                continue;
            };
            let key = asset.to_lowercase();
            if seen_here.contains(&key) {
                continue;
            }
            seen_here.push(key.clone());
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, entries)) => entries.push(entry),
                None => groups.push((key, vec![entry])),
            }
        }
    }
    if let Some(pos) = groups.iter().position(|(k, _)| k == "sol") {
        let sol = groups.remove(pos);
        groups.insert(0, sol);
    }
    groups
        .iter()
        .map(|(_, entries)| merge_asset(entries))
        .collect()
}

fn merge_asset(entries: &[&Value]) -> Value {
    let first = entries[0];
    let tradable = entries.iter().copied().find(|e| is_true(e, "tradable"));
    let base = tradable.unwrap_or(first);
    let from_tradable = |key: &str| tradable.map(|e| field(e, key)).unwrap_or(Value::Null);
    let fresh: Vec<&Value> = entries
        .iter()
        .copied()
        .filter(|e| is_true(e, "fresh"))
        .collect();
    let priced = if fresh.is_empty() {
        entries.to_vec()
    } else {
        fresh.clone()
    };
    json!({
        "asset": field(first, "asset"),
        "underlying": field(base, "underlying"),
        "venue": field(base, "venue"),
        "synthetic": field(base, "synthetic"),
        "tradable": tradable.is_some(),
        "mint": from_tradable("mint"),
        "decimals": from_tradable("decimals"),
        "expiry_time_of_day": from_tradable("expiry_time_of_day"),
        "fresh": !fresh.is_empty(),
        "spot": opt_num(median(&numbers(priced.iter().copied(), "spot"))),
        "atm_vol": opt_num(median(&numbers(priced.iter().copied(), "atm_vol"))),
        "expiries": opt_num(max_of(&numbers(entries.iter().copied(), "expiries"))),
    })
}

/// Merges expiry rows by `expiry_ts`: `days` first seen, `forward_price` median,
/// `atm_vol` median of non-null. Returns (row, contributing rows) sorted by expiry_ts.
fn group_rows<'a>(lists: &[&'a [Value]]) -> Vec<(i64, Vec<&'a Value>)> {
    let mut groups: Vec<(i64, Vec<&Value>)> = Vec::new();
    for list in lists {
        for row in *list {
            let Some(ts) = row.get("expiry_ts").and_then(Value::as_i64) else {
                continue;
            };
            match groups.iter_mut().find(|(t, _)| *t == ts) {
                Some((_, rows)) => rows.push(row),
                None => groups.push((ts, vec![row])),
            }
        }
    }
    groups.sort_by_key(|(ts, _)| *ts);
    groups
}

fn merged_row_header(ts: i64, rows: &[&Value]) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("expiry_ts".into(), json!(ts));
    out.insert("days".into(), field(rows[0], "days"));
    out.insert(
        "forward_price".into(),
        opt_num(median(&numbers(rows.iter().copied(), "forward_price"))),
    );
    out.insert(
        "atm_vol".into(),
        opt_num(median(&numbers(rows.iter().copied(), "atm_vol"))),
    );
    out
}

/// §2.4: one `expiries` list per MM; strikes are the sorted, de-duplicated union.
pub fn merge_expiries(lists: &[&[Value]]) -> Vec<Value> {
    group_rows(lists)
        .into_iter()
        .map(|(ts, rows)| {
            let mut strikes: Vec<f64> = rows
                .iter()
                .flat_map(|r| array(r, "strikes"))
                .filter_map(as_f64)
                .collect();
            strikes.sort_by(f64::total_cmp);
            strikes.dedup();
            let mut out = merged_row_header(ts, &rows);
            out.insert(
                "strikes".into(),
                Value::Array(strikes.into_iter().map(num).collect()),
            );
            Value::Object(out)
        })
        .collect()
}

/// Does `a` beat `b` for the same strike: larger yield_amount (u64), then larger apr_pct.
fn better_cell(a: &Value, b: &Value) -> bool {
    let y = |c: &Value| c.get("yield_amount").and_then(parse_u64).unwrap_or(0);
    let apr = |c: &Value| {
        c.get("apr_pct")
            .and_then(as_f64)
            .unwrap_or(f64::NEG_INFINITY)
    };
    match y(a).cmp(&y(b)) {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => apr(a) > apr(b),
    }
}

/// §2.5: `boards` are `(mm_pubkey, board)` pairs of the successful boards, in MM order.
/// Returns None when there is nothing to merge.
pub fn merge_boards(boards: &[(&str, &Value)]) -> Option<Value> {
    let (_, first) = boards.first()?;
    let index_price = median(&numbers(boards.iter().map(|(_, b)| *b), "index_price"));

    // Rows by expiry_ts, with each row's cells tagged by the board they came from.
    struct Row<'a> {
        ts: i64,
        rows: Vec<&'a Value>,
        cells: Vec<(usize, Value)>,
    }
    let mut rows: Vec<Row> = Vec::new();
    for (bi, (_, board)) in boards.iter().enumerate() {
        for row in array(board, "expiries") {
            let Some(ts) = row.get("expiry_ts").and_then(Value::as_i64) else {
                continue;
            };
            let idx = match rows.iter().position(|r| r.ts == ts) {
                Some(i) => i,
                None => {
                    rows.push(Row {
                        ts,
                        rows: Vec::new(),
                        cells: Vec::new(),
                    });
                    rows.len() - 1
                }
            };
            let r = &mut rows[idx];
            r.rows.push(row);
            for cell in array(row, "quotes") {
                let Some(price) = cell.get("fixed_price").and_then(as_f64) else {
                    continue;
                };
                let same = r
                    .cells
                    .iter()
                    .position(|(_, c)| c.get("fixed_price").and_then(as_f64) == Some(price));
                match same {
                    Some(i) if better_cell(cell, &r.cells[i].1) => r.cells[i] = (bi, cell.clone()),
                    Some(_) => {}
                    None => r.cells.push((bi, cell.clone())),
                }
            }
        }
    }
    rows.sort_by_key(|r| r.ts);

    let mut cells_per_board = vec![0usize; boards.len()];
    let merged_rows: Vec<Value> = rows
        .into_iter()
        .map(|mut r| {
            if let Some(index) = index_price {
                let dist = |c: &Value| {
                    c.get("fixed_price")
                        .and_then(as_f64)
                        .map_or(f64::INFINITY, |p| (p - index).abs())
                };
                r.cells.sort_by(|a, b| dist(&a.1).total_cmp(&dist(&b.1)));
            }
            let quotes: Vec<Value> = r
                .cells
                .into_iter()
                .map(|(bi, mut cell)| {
                    cells_per_board[bi] += 1;
                    let (pubkey, board) = boards[bi];
                    if let Some(obj) = cell.as_object_mut() {
                        obj.insert("mm_pubkey".into(), json!(pubkey));
                        obj.insert("fee_pct".into(), field(board, "fee_pct"));
                    }
                    cell
                })
                .collect();
            let mut out = merged_row_header(r.ts, &r.rows);
            out.insert("quotes".into(), Value::Array(quotes));
            Value::Object(out)
        })
        .collect();

    // Most cells wins; `max_by_key` keeps the last maximum, so scan for the first.
    let mut best = 0;
    for (i, n) in cells_per_board.iter().enumerate() {
        if *n > cells_per_board[best] {
            best = i;
        }
    }
    let fee_pct = min_of(&numbers(boards.iter().map(|(_, b)| *b), "fee_pct"))
        .map(num)
        .unwrap_or_else(|| field(first, "fee_pct"));
    let generated_at = max_of(&numbers(boards.iter().map(|(_, b)| *b), "generated_at"));

    let mut out = Map::new();
    for key in [
        "asset",
        "underlying",
        "venue",
        "mint",
        "decimals",
        "product",
        "token",
        "yield_token",
        "amount",
    ] {
        out.insert(key.into(), field(first, key));
    }
    out.insert(
        "tradable".into(),
        json!(boards.iter().any(|(_, b)| is_true(b, "tradable"))),
    );
    out.insert("index_price".into(), opt_num(index_price));
    out.insert("fee_pct".into(), fee_pct);
    out.insert("mm_pubkey".into(), json!(boards[best].0));
    out.insert("generated_at".into(), opt_num(generated_at));
    out.insert("expiries".into(), Value::Array(merged_rows));
    Some(Value::Object(out))
}

/// Why one MM gave no usable result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MmFailure {
    pub message: String,
    /// The desk's HTTP status; None for timeouts, disconnects and garbage.
    pub status: Option<u16>,
}

/// §2.4: the most common message (ties: first seen), with the status every failure
/// shared, else 503.
pub fn aggregate_failures(failures: &[MmFailure]) -> MarketError {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for f in failures {
        match counts.iter_mut().find(|(m, _)| *m == f.message) {
            Some((_, n)) => *n += 1,
            None => counts.push((&f.message, 1)),
        }
    }
    let mut message = "no market maker is online";
    let mut best = 0;
    for (m, n) in counts {
        if n > best {
            best = n;
            message = m;
        }
    }
    let first = failures.first().and_then(|f| f.status);
    let status = match first {
        Some(s) if failures.iter().all(|f| f.status == Some(s)) => s,
        _ => 503,
    };
    MarketError {
        status,
        message: message.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_rules() {
        assert_eq!(median(&[]), None);
        assert_eq!(median(&[3.0]), Some(3.0));
        assert_eq!(median(&[3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), Some(2.5));
        assert_eq!(median(&[200.0, 202.0]), Some(201.0));
        assert_eq!(median(&[f64::NAN, 1.0]), Some(1.0));
    }

    fn health(
        ok: bool,
        dry: bool,
        spot: f64,
        age: u64,
        vol: f64,
        exp: u64,
        exposure: Value,
    ) -> Value {
        json!({
            "ok": ok, "mm_pubkey": "x", "dry_run": dry,
            "price": { "source": "deribit", "spot": spot, "spotAgeMs": age, "vol": vol, "expiries": exp },
            "assets": [{ "asset": "SOL", "underlying": "SOL", "venue": "deribit", "tradable": true, "fresh": true, "spot": spot, "atm_vol": vol, "expiries": exp, "mint": null, "decimals": null, "expiry_time_of_day": null, "synthetic": false }],
            "exposure": exposure,
        })
    }

    #[test]
    fn health_aggregation() {
        let a = health(
            true,
            false,
            200.0,
            1200,
            0.7,
            12,
            json!({
                "totalUsd": 1000, "capTotalUsd": 20000, "capPerExpiryUsd": 5000,
                "perExpiry": { "1759824000": { "onChainUsd": 1000, "reservedUsd": 0 } }
            }),
        );
        let b = health(
            false,
            true,
            202.0,
            800,
            0.8,
            10,
            json!({
                "totalUsd": 234.5, "capTotalUsd": 20000, "capPerExpiryUsd": 5000,
                "perExpiry": { "1759824000": { "onChainUsd": 0, "reservedUsd": 234.5 }, "1760000000": { "onChainUsd": 5, "reservedUsd": 0 } }
            }),
        );
        let m = merge_health(&[&a, &b]);
        assert!(m.ok, "one MM ok is enough");
        assert!(!m.dry_run, "only when every MM is dry-run");
        assert_eq!(
            m.price,
            json!({ "source": "aggregate", "spot": 201, "spotAgeMs": 800, "vol": 0.75, "expiries": 12 })
        );
        assert_eq!(
            m.exposure,
            json!({
                "totalUsd": 1234.5, "capTotalUsd": 40000, "capPerExpiryUsd": 10000,
                "perExpiry": {
                    "1759824000": { "onChainUsd": 1000, "reservedUsd": 234.5 },
                    "1760000000": { "onChainUsd": 5, "reservedUsd": 0 }
                }
            })
        );
        assert_eq!(m.assets.len(), 1);
        assert!(
            m.assets[0].get("mm_count").is_none(),
            "the MM count is not public"
        );

        let dry = merge_health(&[&b]);
        assert!(!dry.ok);
        assert!(dry.dry_run);
    }

    #[test]
    fn health_offline_shape() {
        let m = merge_health(&[]);
        assert!(!m.ok);
        assert!(!m.dry_run);
        assert_eq!(
            m.price,
            json!({ "source": "aggregate", "spot": null, "spotAgeMs": null, "vol": null, "expiries": null })
        );
        assert!(m.assets.is_empty());
        assert_eq!(
            m.exposure,
            json!({ "totalUsd": 0, "capTotalUsd": 0, "capPerExpiryUsd": 0, "perExpiry": {} })
        );
    }

    #[test]
    fn assets_merge_rules() {
        let a = vec![
            json!({ "asset": "TSLAon", "underlying": "TSLA", "venue": "alpaca", "tradable": false, "fresh": true, "spot": 250.0, "atm_vol": 0.5, "expiries": 4, "mint": null, "decimals": null, "expiry_time_of_day": null, "synthetic": false }),
            json!({ "asset": "SOL", "underlying": "SOL", "venue": "deribit", "tradable": true, "fresh": true, "spot": 200.0, "atm_vol": 0.7, "expiries": 12, "mint": null, "decimals": null, "expiry_time_of_day": null, "synthetic": false }),
        ];
        let b = vec![
            json!({ "asset": "tslaON", "underlying": "TSLA-listed", "venue": "prestocks", "tradable": true, "fresh": false, "spot": 999.0, "atm_vol": null, "expiries": 6, "mint": "Mint111", "decimals": 9, "expiry_time_of_day": 72000, "synthetic": true }),
            json!({ "asset": "SPYon", "underlying": "SPY", "venue": "alpaca", "tradable": false, "fresh": false, "spot": 500.0, "atm_vol": null, "expiries": null, "mint": null, "decimals": null, "expiry_time_of_day": null, "synthetic": false }),
            json!({ "asset": "SOL", "underlying": "SOL", "venue": "deribit", "tradable": true, "fresh": true, "spot": 204.0, "atm_vol": 0.8, "expiries": 10, "mint": null, "decimals": null, "expiry_time_of_day": null, "synthetic": false }),
        ];
        let c = vec![
            json!({ "asset": "SPYon", "underlying": "SPY", "venue": "alpaca", "tradable": false, "fresh": false, "spot": 510.0, "atm_vol": 0.2, "expiries": 3 }),
        ];
        let out = merge_assets(&[&a, &b, &c]);
        let names: Vec<&str> = out.iter().map(|x| x["asset"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["SOL", "TSLAon", "SPYon"],
            "SOL first, then first-seen order and casing"
        );

        assert_eq!(out[0]["spot"], json!(202));
        assert_eq!(out[0]["atm_vol"], json!(0.75));
        assert_eq!(out[0]["expiries"], json!(12));
        assert!(
            out[0].get("mm_count").is_none(),
            "the MM count is not public"
        );

        let tsla = &out[1];
        assert_eq!(tsla["tradable"], json!(true));
        assert_eq!(
            tsla["underlying"],
            json!("TSLA-listed"),
            "from the tradable entry"
        );
        assert_eq!(tsla["venue"], json!("prestocks"));
        assert_eq!(tsla["synthetic"], json!(true));
        assert_eq!(tsla["mint"], json!("Mint111"));
        assert_eq!(tsla["decimals"], json!(9));
        assert_eq!(tsla["expiry_time_of_day"], json!(72000));
        assert_eq!(tsla["fresh"], json!(true));
        assert_eq!(tsla["spot"], json!(250), "only fresh entries price it");
        assert_eq!(tsla["atm_vol"], json!(0.5));
        assert_eq!(tsla["expiries"], json!(6));

        let spy = &out[2];
        assert_eq!(spy["tradable"], json!(false));
        assert_eq!(spy["mint"], Value::Null);
        assert_eq!(spy["fresh"], json!(false));
        assert_eq!(spy["spot"], json!(505), "no fresh entry: all entries");
        assert_eq!(spy["atm_vol"], json!(0.2), "median of non-null");
        assert_eq!(spy["expiries"], json!(3));
        assert!(spy.get("mm_count").is_none(), "the MM count is not public");
    }

    #[test]
    fn expiries_merge_rules() {
        let a = vec![
            json!({ "expiry_ts": 2000, "days": 2.0, "forward_price": 201.0, "atm_vol": null, "strikes": [190, 200, 210] }),
            json!({ "expiry_ts": 1000, "days": 1.0, "forward_price": 200.0, "atm_vol": 0.7, "strikes": [200] }),
        ];
        let b = vec![
            json!({ "expiry_ts": 1000, "days": 1.1, "forward_price": 202.0, "atm_vol": 0.9, "strikes": [195.5, 200] }),
            json!({ "expiry_ts": 2000, "days": 2.2, "forward_price": 203.0, "atm_vol": null, "strikes": [205, 190] }),
        ];
        let c = vec![
            json!({ "expiry_ts": 1000, "days": 1.2, "forward_price": 210.0, "atm_vol": null, "strikes": [] }),
        ];
        let out = merge_expiries(&[&a, &b, &c]);
        assert_eq!(
            out,
            vec![
                json!({ "expiry_ts": 1000, "days": 1.0, "forward_price": 202, "atm_vol": 0.8, "strikes": [195.5, 200] }),
                json!({ "expiry_ts": 2000, "days": 2.0, "forward_price": 202, "atm_vol": null, "strikes": [190, 200, 205, 210] }),
            ]
        );
        assert!(merge_expiries(&[]).is_empty());
    }

    fn cell(price: f64, yield_amount: &str, apr: f64, tag: &str) -> Value {
        json!({ "moneyness": 1.0, "fixed_price": price, "yield_amount": yield_amount, "yield_pct": 1.0, "apr_pct": apr, "instrument": tag })
    }

    fn board(tradable: bool, index: f64, fee: f64, generated: u64, rows: Value) -> Value {
        json!({
            "asset": "SOL", "underlying": "SOL", "venue": "deribit", "tradable": tradable, "mint": null, "decimals": 9,
            "product": "sell_sol", "token": "sol", "yield_token": "usdc", "amount": "1000000000",
            "index_price": index, "fee_pct": fee, "mm_pubkey": "ignored", "generated_at": generated, "expiries": rows,
        })
    }

    #[test]
    fn board_merge_rules() {
        let a = board(
            false,
            200.0,
            0.5,
            1000,
            json!([
                { "expiry_ts": 2000, "days": 2.0, "forward_price": 200.0, "atm_vol": 0.7, "quotes": [
                    cell(220.0, "900", 10.0, "a220"), cell(210.0, "1000", 12.0, "a210") ] },
            ]),
        );
        let b = board(
            true,
            204.0,
            0.25,
            3000,
            json!([
                { "expiry_ts": 2000, "days": 2.5, "forward_price": 202.0, "atm_vol": null, "quotes": [
                    cell(210.0, "1000", 13.0, "b210"), cell(220.0, "18446744073709551615", 1.0, "b220"), cell(205.0, "5", 1.0, "b205") ] },
                { "expiry_ts": 1000, "days": 1.0, "forward_price": 201.0, "atm_vol": 0.6, "quotes": [ cell(230.0, "7", 1.0, "b230") ] },
            ]),
        );
        let c = board(
            false,
            206.0,
            1.0,
            2000,
            json!([
                { "expiry_ts": 2000, "days": 3.0, "forward_price": 210.0, "atm_vol": 0.9, "quotes": [ cell(205.0, "5", 1.0, "c205") ] },
            ]),
        );
        let out = merge_boards(&[("A", &a), ("B", &b), ("C", &c)]).unwrap();

        assert_eq!(out["index_price"], json!(204));
        assert_eq!(out["generated_at"], json!(3000));
        assert_eq!(out["fee_pct"], json!(0.25));
        assert_eq!(out["tradable"], json!(true));
        assert!(out.get("mm_count").is_none(), "the MM count is not public");
        assert_eq!(out["amount"], json!("1000000000"));
        assert_eq!(out["mm_pubkey"], json!("B"), "B contributes the most cells");

        let rows = out["expiries"].as_array().unwrap();
        assert_eq!(
            rows.iter()
                .map(|r| r["expiry_ts"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [1000, 2000]
        );
        let row = &rows[1];
        assert_eq!(row["days"], json!(2.0), "first seen");
        assert_eq!(row["forward_price"], json!(202));
        assert_eq!(row["atm_vol"], json!(0.8));
        let tags: Vec<&str> = row["quotes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|q| q["instrument"].as_str().unwrap())
            .collect();
        // Sorted by distance from 204: 205 (1), 210 (6), 220 (16).
        // 210: equal yield, B's higher APR wins. 220: u64 compare, B wins. 205: equal, first seen (B).
        assert_eq!(tags, ["b205", "b210", "b220"]);
        let q = &row["quotes"][1];
        assert_eq!(q["mm_pubkey"], json!("B"));
        assert_eq!(q["fee_pct"], json!(0.25));
    }

    #[test]
    fn board_ties_keep_first_and_most_cells_ties_first() {
        let a = board(
            false,
            100.0,
            0.5,
            1,
            json!([{ "expiry_ts": 1, "days": 1, "forward_price": 100, "atm_vol": null, "quotes": [cell(110.0, "10", 5.0, "a")] }]),
        );
        let b = board(
            false,
            100.0,
            0.5,
            1,
            json!([{ "expiry_ts": 1, "days": 1, "forward_price": 100, "atm_vol": null, "quotes": [cell(110.0, "10", 5.0, "b")] }]),
        );
        let out = merge_boards(&[("A", &a), ("B", &b)]).unwrap();
        assert_eq!(out["expiries"][0]["quotes"][0]["instrument"], json!("a"));
        assert_eq!(out["mm_pubkey"], json!("A"));

        // B wins no cell, A wins one: A. With no cells at all the first board's MM.
        let empty = board(false, 100.0, 0.5, 1, json!([]));
        assert_eq!(
            merge_boards(&[("E", &empty), ("B", &b)]).unwrap()["mm_pubkey"],
            json!("B")
        );
        assert_eq!(
            merge_boards(&[("E", &empty)]).unwrap()["mm_pubkey"],
            json!("E")
        );
        assert!(merge_boards(&[]).is_none());
    }

    #[test]
    fn failure_aggregation() {
        let f = |m: &str, s: Option<u16>| MmFailure {
            message: m.into(),
            status: s,
        };
        let e = aggregate_failures(&[
            f("SOL reference price is stale", Some(400)),
            f("SOL reference price is stale", Some(400)),
        ]);
        assert_eq!(
            e,
            MarketError {
                status: 400,
                message: "SOL reference price is stale".into()
            }
        );

        let e = aggregate_failures(&[f("x", Some(400)), f("y", Some(400)), f("y", Some(400))]);
        assert_eq!(
            e,
            MarketError {
                status: 400,
                message: "y".into()
            }
        );

        // Tie: first seen. Mixed statuses: 503.
        let e = aggregate_failures(&[f("a", Some(400)), f("b", Some(404))]);
        assert_eq!(
            e,
            MarketError {
                status: 503,
                message: "a".into()
            }
        );

        // A timeout has no status.
        let e = aggregate_failures(&[
            f("market maker did not respond in time", None),
            f("x", Some(400)),
        ]);
        assert_eq!(e.status, 503);
        assert_eq!(e.message, "market maker did not respond in time");
    }
}
