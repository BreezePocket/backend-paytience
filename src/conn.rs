//! One authenticated MM WebSocket. Outbound frames go through an mpsc channel to the
//! connection's writer task; request/response pairs are matched through a pending map
//! so callers get a plain `request(..).await` with a timeout. Closing the connection
//! fails every pending request at once (contract §1.3).

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use serde_json::Value;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};

use crate::protocol::Channel;

/// What the writer task sends on the socket.
#[derive(Debug)]
pub enum Outbound {
    Text(String),
    /// Send a close frame, then stop writing.
    Close(u16, String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum RequestError {
    /// The connection was already gone; nothing was sent, so another one may be tried.
    #[error("market maker disconnected")]
    NotSent,
    #[error("market maker disconnected")]
    Disconnected,
    #[error("market maker did not respond in time")]
    Timeout,
}

#[derive(Default)]
struct Pending {
    closed: bool,
    waiters: HashMap<String, oneshot::Sender<Value>>,
}

pub struct Conn {
    /// Monotonic across all connections, assigned at authentication, so it also orders
    /// connections by when they authenticated.
    pub id: u64,
    pub channel: Channel,
    pub mm_pubkey: String,
    pub instance_id: String,
    pub name: String,
    pub authenticated_at: Instant,
    pub authenticated_at_ms: u64,
    out: mpsc::UnboundedSender<Outbound>,
    pending: Mutex<Pending>,
    closed_tx: watch::Sender<bool>,
}

pub struct ConnInfo {
    pub id: u64,
    pub channel: Channel,
    pub mm_pubkey: String,
    pub instance_id: String,
    pub name: String,
    pub authenticated_at_ms: u64,
}

impl Conn {
    pub fn new(info: ConnInfo, out: mpsc::UnboundedSender<Outbound>) -> Self {
        Self {
            id: info.id,
            channel: info.channel,
            mm_pubkey: info.mm_pubkey,
            instance_id: info.instance_id,
            name: info.name,
            authenticated_at: Instant::now(),
            authenticated_at_ms: info.authenticated_at_ms,
            out,
            pending: Mutex::new(Pending::default()),
            closed_tx: watch::channel(false).0,
        }
    }

    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Fire-and-forget; false when the connection is closed.
    pub fn send(&self, frame: &Value) -> bool {
        !self.is_closed() && self.out.send(Outbound::Text(frame.to_string())).is_ok()
    }

    /// Sends `frame` and waits for the reply matched on `key` (its rfq_id / request_id).
    pub async fn request(
        &self,
        key: &str,
        frame: &Value,
        timeout: Duration,
    ) -> Result<Value, RequestError> {
        let rx = {
            let mut p = self.pending();
            if p.closed {
                return Err(RequestError::NotSent);
            }
            let (tx, rx) = oneshot::channel();
            p.waiters.insert(key.to_string(), tx);
            rx
        };
        // Removes the waiter however this future ends: reply, timeout or cancellation.
        let _guard = WaiterGuard { conn: self, key };
        if self.out.send(Outbound::Text(frame.to_string())).is_err() {
            return Err(RequestError::NotSent);
        }
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(reply)) => Ok(reply),
            Ok(Err(_)) => Err(RequestError::Disconnected),
            Err(_) => Err(RequestError::Timeout),
        }
    }

    /// Hands an inbound reply to its waiter; false when nobody is waiting for `key`.
    pub fn resolve(&self, key: &str, frame: Value) -> bool {
        match self.pending().waiters.remove(key) {
            Some(tx) => tx.send(frame).is_ok(),
            None => false,
        }
    }

    /// Fails every pending request now and refuses new ones.
    pub fn mark_closed(&self) {
        let waiters = {
            let mut p = self.pending();
            p.closed = true;
            std::mem::take(&mut p.waiters)
        };
        drop(waiters);
        self.closed_tx.send_replace(true);
    }

    /// Marks the connection closed and has the writer send a close frame.
    pub fn close(&self, code: u16, reason: &str) {
        let already = self.is_closed();
        self.mark_closed();
        if !already {
            let _ = self.out.send(Outbound::Close(code, reason.to_string()));
        }
    }

    pub fn is_closed(&self) -> bool {
        *self.closed_tx.borrow()
    }

    /// Resolves once the connection is closed.
    pub async fn closed(&self) {
        let mut rx = self.closed_tx.subscribe();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    pub fn pending_count(&self) -> usize {
        self.pending().waiters.len()
    }
}

struct WaiterGuard<'a> {
    conn: &'a Conn,
    key: &'a str,
}

impl Drop for WaiterGuard<'_> {
    fn drop(&mut self) {
        self.conn.pending().waiters.remove(self.key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    fn conn() -> (Arc<Conn>, mpsc::UnboundedReceiver<Outbound>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let info = ConnInfo {
            id: 1,
            channel: Channel::Rfq,
            mm_pubkey: "pk".into(),
            instance_id: "i".into(),
            name: "n".into(),
            authenticated_at_ms: 0,
        };
        (Arc::new(Conn::new(info, tx)), rx)
    }

    #[tokio::test]
    async fn request_resolves_on_matching_reply() {
        let (c, mut rx) = conn();
        let c2 = c.clone();
        let task = tokio::spawn(async move {
            c2.request("k1", &json!({"a": 1}), Duration::from_secs(5))
                .await
        });
        assert!(matches!(rx.recv().await, Some(Outbound::Text(t)) if t.contains("\"a\":1")));
        assert!(!c.resolve("other", json!({})));
        assert!(c.resolve("k1", json!({"ok": true})));
        assert_eq!(task.await.unwrap().unwrap(), json!({"ok": true}));
        assert_eq!(c.pending_count(), 0);
    }

    #[tokio::test]
    async fn timeout_removes_the_waiter() {
        let (c, _rx) = conn();
        let r = c.request("k", &json!({}), Duration::from_millis(20)).await;
        assert_eq!(r, Err(RequestError::Timeout));
        assert_eq!(c.pending_count(), 0);
    }

    #[tokio::test]
    async fn close_fails_pending_requests_immediately() {
        let (c, mut rx) = conn();
        let c2 = c.clone();
        let started = Instant::now();
        let task =
            tokio::spawn(async move { c2.request("k", &json!({}), Duration::from_secs(30)).await });
        let _ = rx.recv().await;
        c.close(4000, "replaced");
        assert_eq!(task.await.unwrap(), Err(RequestError::Disconnected));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(matches!(rx.recv().await, Some(Outbound::Close(4000, _))));
        assert_eq!(
            c.request("k2", &json!({}), Duration::from_secs(1)).await,
            Err(RequestError::NotSent)
        );
        assert!(!c.send(&json!({})));
        c.closed().await;
    }
}
