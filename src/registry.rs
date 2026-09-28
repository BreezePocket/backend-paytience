//! Every connected MM instance, keyed by `(mm_pubkey, instance_id)`, with its `ws/rfq`
//! and `ws/sign` connections, its latest `health` snapshot, and per-pubkey counters.
//! An instance is eligible for RFQs only while both channels are authenticated (§1.2).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::Value;

use crate::conn::Conn;
use crate::protocol::{Channel, now_ms};

#[derive(Debug, Clone)]
pub struct HealthSnapshot {
    pub body: Value,
    pub at: Instant,
    pub at_ms: u64,
}

pub struct Instance {
    pub mm_pubkey: String,
    pub instance_id: String,
    pub name: String,
    pub rfq: Option<Arc<Conn>>,
    pub sign: Option<Arc<Conn>>,
    pub health: Option<HealthSnapshot>,
    pub connected_since_ms: u64,
}

impl Instance {
    pub fn eligible(&self) -> bool {
        self.rfq.is_some() && self.sign.is_some()
    }

    fn slot(&mut self, channel: Channel) -> &mut Option<Arc<Conn>> {
        match channel {
            Channel::Rfq => &mut self.rfq,
            Channel::Sign => &mut self.sign,
        }
    }
}

/// An eligible instance, detached from the registry lock.
#[derive(Clone)]
pub struct EligibleInstance {
    pub mm_pubkey: String,
    pub instance_id: String,
    pub name: String,
    pub rfq: Arc<Conn>,
    pub sign: Arc<Conn>,
    pub health: Option<HealthSnapshot>,
}

impl EligibleInstance {
    pub fn fresh_health(&self, window: Duration) -> Option<&HealthSnapshot> {
        self.health.as_ref().filter(|h| h.at.elapsed() <= window)
    }
}

/// Per-pubkey counters (for `GET /admin/mms`).
#[derive(Debug, Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub struct MmCounters {
    pub rfqs_asked: u64,
    pub quotes: u64,
    pub declines: u64,
    pub timeouts: u64,
    pub wins: u64,
    pub signs: u64,
    pub sign_rejections: u64,
    pub sign_timeouts: u64,
}

#[derive(Default)]
struct Inner {
    instances: HashMap<(String, String), Instance>,
    counters: HashMap<String, MmCounters>,
}

#[derive(Default)]
pub struct Registry {
    inner: Mutex<Inner>,
    next_conn_id: AtomicU64,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn next_conn_id(&self) -> u64 {
        self.next_conn_id.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Installs an authenticated connection. Returns the connection it replaced (same
    /// pubkey, instance and channel), which the caller closes with 4000.
    pub fn register(&self, conn: Arc<Conn>) -> Option<Arc<Conn>> {
        let mut inner = self.lock();
        let key = (conn.mm_pubkey.clone(), conn.instance_id.clone());
        let instance = inner.instances.entry(key).or_insert_with(|| Instance {
            mm_pubkey: conn.mm_pubkey.clone(),
            instance_id: conn.instance_id.clone(),
            name: conn.name.clone(),
            rfq: None,
            sign: None,
            health: None,
            connected_since_ms: now_ms(),
        });
        instance.slot(conn.channel).replace(conn)
    }

    /// Removes `conn` if it is still the registered one; a replaced connection's
    /// cleanup must not remove its replacement.
    pub fn unregister(&self, conn: &Conn) {
        let mut inner = self.lock();
        let key = (conn.mm_pubkey.clone(), conn.instance_id.clone());
        let Some(instance) = inner.instances.get_mut(&key) else {
            return;
        };
        let slot = instance.slot(conn.channel);
        if slot.as_ref().is_some_and(|c| c.id == conn.id) {
            *slot = None;
        }
        if instance.rfq.is_none() && instance.sign.is_none() {
            inner.instances.remove(&key);
        }
    }

    /// Stores a health result polled over `conn` (the instance's current rfq connection).
    pub fn set_health(&self, conn: &Conn, body: Value) {
        let mut inner = self.lock();
        let key = (conn.mm_pubkey.clone(), conn.instance_id.clone());
        if let Some(instance) = inner.instances.get_mut(&key)
            && instance.rfq.as_ref().is_some_and(|c| c.id == conn.id)
        {
            instance.health = Some(HealthSnapshot {
                body,
                at: Instant::now(),
                at_ms: now_ms(),
            });
        }
    }

    /// Eligible instances, in the order their rfq channel authenticated (oldest first).
    pub fn eligible(&self) -> Vec<EligibleInstance> {
        let inner = self.lock();
        let mut out: Vec<EligibleInstance> = inner
            .instances
            .values()
            .filter_map(|i| match (&i.rfq, &i.sign) {
                (Some(rfq), Some(sign)) => Some(EligibleInstance {
                    mm_pubkey: i.mm_pubkey.clone(),
                    instance_id: i.instance_id.clone(),
                    name: i.name.clone(),
                    rfq: rfq.clone(),
                    sign: sign.clone(),
                    health: i.health.clone(),
                }),
                _ => None,
            })
            .collect();
        out.sort_by_key(|i| i.rfq.id);
        out
    }

    /// One eligible instance per pubkey for market data, so one MM running two desks
    /// is not counted twice: the freshest health snapshot wins, else the newest
    /// connection. Pubkeys are ordered by their oldest eligible instance.
    pub fn market_representatives(&self) -> Vec<EligibleInstance> {
        let mut out: Vec<EligibleInstance> = Vec::new();
        for inst in self.eligible() {
            match out.iter_mut().find(|o| o.mm_pubkey == inst.mm_pubkey) {
                None => out.push(inst),
                Some(existing) => {
                    let better = match (&inst.health, &existing.health) {
                        (Some(a), Some(b)) => a.at > b.at,
                        (Some(_), None) => true,
                        (None, Some(_)) => false,
                        (None, None) => inst.rfq.id > existing.rfq.id,
                    };
                    if better {
                        *existing = inst;
                    }
                }
            }
        }
        out
    }

    /// The `ws/sign` connection of one instance, if connected.
    pub fn sign_conn(&self, mm_pubkey: &str, instance_id: &str) -> Option<Arc<Conn>> {
        let inner = self.lock();
        inner
            .instances
            .get(&(mm_pubkey.to_string(), instance_id.to_string()))
            .and_then(|i| i.sign.clone())
    }

    /// Every connected `ws/sign` of a pubkey, most recently authenticated first.
    pub fn sign_conns_for(&self, mm_pubkey: &str) -> Vec<Arc<Conn>> {
        let inner = self.lock();
        let mut out: Vec<Arc<Conn>> = inner
            .instances
            .values()
            .filter(|i| i.mm_pubkey == mm_pubkey)
            .filter_map(|i| i.sign.clone())
            .collect();
        out.sort_by_key(|c| std::cmp::Reverse(c.id));
        out
    }

    pub fn count(&self, mm_pubkey: &str, f: impl FnOnce(&mut MmCounters)) {
        let mut inner = self.lock();
        f(inner.counters.entry(mm_pubkey.to_string()).or_default());
    }

    pub fn counters(&self, mm_pubkey: &str) -> MmCounters {
        self.lock()
            .counters
            .get(mm_pubkey)
            .copied()
            .unwrap_or_default()
    }

    /// Runs `f` over every instance under the lock (for admin listings).
    pub fn with_instances<R>(&self, f: impl FnOnce(&mut dyn Iterator<Item = &Instance>) -> R) -> R {
        let inner = self.lock();
        f(&mut inner.instances.values())
    }

    /// Closes every connection (server shutdown).
    pub fn close_all(&self, code: u16, reason: &str) {
        let conns: Vec<Arc<Conn>> = {
            let inner = self.lock();
            inner
                .instances
                .values()
                .flat_map(|i| [i.rfq.clone(), i.sign.clone()])
                .flatten()
                .collect()
        };
        for c in conns {
            c.close(code, reason);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::ConnInfo;
    use serde_json::json;
    use tokio::sync::mpsc;

    fn conn(reg: &Registry, pk: &str, inst: &str, channel: Channel) -> Arc<Conn> {
        let (tx, rx) = mpsc::unbounded_channel();
        std::mem::forget(rx);
        let info = ConnInfo {
            id: reg.next_conn_id(),
            channel,
            mm_pubkey: pk.into(),
            instance_id: inst.into(),
            name: pk.into(),
            authenticated_at_ms: 0,
        };
        Arc::new(Conn::new(info, tx))
    }

    #[test]
    fn eligibility_needs_both_channels() {
        let reg = Registry::new();
        let rfq = conn(&reg, "a", "1", Channel::Rfq);
        assert!(reg.register(rfq.clone()).is_none());
        assert!(reg.eligible().is_empty());
        let sign = conn(&reg, "a", "1", Channel::Sign);
        reg.register(sign.clone());
        assert_eq!(reg.eligible().len(), 1);
        reg.unregister(&sign);
        assert!(reg.eligible().is_empty());
        assert!(reg.sign_conn("a", "1").is_none());
    }

    #[test]
    fn replacement_keeps_the_new_connection() {
        let reg = Registry::new();
        let old = conn(&reg, "a", "1", Channel::Rfq);
        reg.register(old.clone());
        reg.register(conn(&reg, "a", "1", Channel::Sign));
        let new = conn(&reg, "a", "1", Channel::Rfq);
        let displaced = reg.register(new.clone()).unwrap();
        assert_eq!(displaced.id, old.id);
        // The old socket's cleanup runs after the replacement registered.
        reg.unregister(&old);
        assert_eq!(reg.eligible()[0].rfq.id, new.id);
    }

    #[test]
    fn sign_conns_newest_first_and_representatives() {
        let reg = Registry::new();
        for inst in ["1", "2"] {
            reg.register(conn(&reg, "a", inst, Channel::Rfq));
            reg.register(conn(&reg, "a", inst, Channel::Sign));
        }
        reg.register(conn(&reg, "b", "1", Channel::Rfq));
        reg.register(conn(&reg, "b", "1", Channel::Sign));
        let signs = reg.sign_conns_for("a");
        assert_eq!(signs.len(), 2);
        assert_eq!(signs[0].instance_id, "2");
        assert!(reg.sign_conns_for("c").is_empty());

        let reps = reg.market_representatives();
        assert_eq!(
            reps.iter()
                .map(|r| r.mm_pubkey.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(reps[0].instance_id, "2", "newest connection without health");

        let inst1 = reg
            .eligible()
            .into_iter()
            .find(|i| i.mm_pubkey == "a" && i.instance_id == "1")
            .unwrap();
        reg.set_health(&inst1.rfq, json!({"ok": true}));
        assert_eq!(
            reg.market_representatives()[0].instance_id,
            "1",
            "the instance with health wins"
        );
        // Health polled over a sign connection (or a stale rfq one) is ignored.
        reg.set_health(&inst1.sign, json!({"bad": true}));
        assert_eq!(
            reg.eligible()
                .iter()
                .find(|i| i.instance_id == "1" && i.mm_pubkey == "a")
                .unwrap()
                .health
                .as_ref()
                .unwrap()
                .body,
            json!({"ok": true})
        );
    }

    #[test]
    fn counters_accumulate() {
        let reg = Registry::new();
        reg.count("a", |c| c.quotes += 2);
        reg.count("a", |c| c.wins += 1);
        assert_eq!(
            reg.counters("a"),
            MmCounters {
                quotes: 2,
                wins: 1,
                ..Default::default()
            }
        );
        assert_eq!(reg.counters("zzz"), MmCounters::default());
    }
}
