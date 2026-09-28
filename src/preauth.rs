//! Caps on WebSocket connections that have not authenticated yet (contract §1.2): one
//! global limit and one per client IP. A socket holds a [`PreauthPermit`] from before
//! its upgrade until its `hello` is verified or it closes, whichever comes first.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

type PerIp = Arc<Mutex<HashMap<String, usize>>>;

fn lock(per_ip: &PerIp) -> MutexGuard<'_, HashMap<String, usize>> {
    per_ip.lock().unwrap_or_else(PoisonError::into_inner)
}

pub struct PreauthGate {
    global: Arc<Semaphore>,
    max_global: usize,
    per_ip: PerIp,
    max_per_ip: usize,
}

/// Why an upgrade was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Global,
    PerIp,
}

impl PreauthGate {
    pub fn new(max_global: usize, max_per_ip: usize) -> Self {
        Self {
            global: Arc::new(Semaphore::new(max_global)),
            max_global,
            per_ip: PerIp::default(),
            max_per_ip,
        }
    }

    /// Takes a slot for one unauthenticated socket from `client`.
    pub fn admit(&self, client: &str) -> Result<PreauthPermit, Refused> {
        let global = self
            .global
            .clone()
            .try_acquire_owned()
            .map_err(|_| Refused::Global)?;
        let mut counts = lock(&self.per_ip);
        let n = counts.entry(client.to_string()).or_insert(0);
        if *n >= self.max_per_ip {
            if *n == 0 {
                counts.remove(client);
            }
            return Err(Refused::PerIp);
        }
        *n += 1;
        Ok(PreauthPermit {
            _global: global,
            per_ip: self.per_ip.clone(),
            client: client.to_string(),
        })
    }

    /// Unauthenticated sockets currently holding a slot.
    pub fn open(&self) -> usize {
        self.max_global
            .saturating_sub(self.global.available_permits())
    }

    /// Slots held by `client`.
    pub fn open_for(&self, client: &str) -> usize {
        lock(&self.per_ip).get(client).copied().unwrap_or(0)
    }
}

/// Releases its global and per-IP slot on drop.
pub struct PreauthPermit {
    _global: OwnedSemaphorePermit,
    per_ip: PerIp,
    client: String,
}

impl Drop for PreauthPermit {
    fn drop(&mut self) {
        let mut counts = lock(&self.per_ip);
        if let Some(n) = counts.get_mut(&self.client) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                counts.remove(&self.client);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn global_and_per_ip_caps_free_on_drop() {
        let gate = PreauthGate::new(3, 2);
        let a1 = gate.admit("a").unwrap();
        let a2 = gate.admit("a").unwrap();
        assert_eq!(gate.admit("a").err(), Some(Refused::PerIp));
        let b1 = gate.admit("b").unwrap();
        assert_eq!(gate.admit("c").err(), Some(Refused::Global));
        assert_eq!((gate.open(), gate.open_for("a")), (3, 2));
        assert_eq!(gate.open_for("c"), 0, "a refused client leaves no entry");

        drop(a1);
        assert_eq!((gate.open(), gate.open_for("a")), (2, 1));
        let c1 = gate.admit("c").unwrap();
        drop((a2, b1, c1));
        assert_eq!(gate.open(), 0);
        assert!(lock(&gate.per_ip).is_empty());
    }
}
