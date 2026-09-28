//! Backend-wide counters reported in `/health.stats` (contract §2.1).

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

#[derive(Debug, Default)]
pub struct Stats {
    /// Valid RFQs received.
    pub rfqs: AtomicU64,
    /// RFQs answered with a winning quote.
    pub quoted: AtomicU64,
    /// RFQs answered with a decline (every MM declined, or none online).
    pub declined: AtomicU64,
    /// RFQs no MM answered in time.
    pub timed_out: AtomicU64,
    /// Co-signed transactions returned to the client.
    pub signs: AtomicU64,
    /// Sign requests rejected by the MM, or whose broadcast failed.
    pub sign_rejections: AtomicU64,
    /// Sign requests the MM did not answer in time (or disconnected during).
    pub sign_timeouts: AtomicU64,
    /// Transactions the backend broadcast successfully.
    pub broadcasts: AtomicU64,
}

impl Stats {
    pub fn inc(counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub fn snapshot(&self) -> Value {
        let g = |c: &AtomicU64| c.load(Ordering::Relaxed);
        json!({
            "rfqs": g(&self.rfqs),
            "quoted": g(&self.quoted),
            "declined": g(&self.declined),
            "timed_out": g(&self.timed_out),
            "signs": g(&self.signs),
            "sign_rejections": g(&self.sign_rejections),
            "sign_timeouts": g(&self.sign_timeouts),
            "broadcasts": g(&self.broadcasts),
        })
    }
}
