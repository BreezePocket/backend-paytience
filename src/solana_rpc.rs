//! Minimal Solana JSON-RPC client. Every method goes through `call`, which separates
//! a JSON-RPC error response (the node answered "no") from transport failures,
//! throttling and unparseable replies (we don't know what happened), because §2.7
//! treats them differently.
//!
//! The RPC URL often carries a provider API key in its path or query, so no error
//! from here ever contains it: reqwest errors are stripped of their URL.

use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RpcError {
    /// The node returned a JSON-RPC `error` object.
    #[error("{message}")]
    Rpc {
        code: i64,
        message: String,
        data: Option<Value>,
    },
    /// HTTP 429: a rate limiter in front of the node refused the request, so the node
    /// never saw it (devnet answers 429 with a JSON-RPC error body).
    #[error("rpc throttled: {0}")]
    Throttled(String),
    /// Connect / timeout failure, or HTTP 5xx whatever the body says: the outcome is
    /// unknown.
    #[error("rpc transport error: {0}")]
    Transport(String),
    /// A body that is not a JSON-RPC response, or whose `result` has the wrong shape.
    #[error("invalid rpc response: {0}")]
    InvalidResponse(String),
}

pub struct SolanaRpc {
    http: reqwest::Client,
    url: String,
}

impl SolanaRpc {
    pub fn new(url: &str, timeout: Duration) -> Result<Self, RpcError> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout.min(Duration::from_secs(5)))
            .build()
            .map_err(transport)?;
        Ok(Self {
            http,
            url: url.to_string(),
        })
    }

    /// One JSON-RPC 2.0 call, deserializing `result` into `T`.
    pub async fn call<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
    ) -> Result<T, RpcError> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let resp = self
            .http
            .post(&self.url)
            .json(&body)
            .send()
            .await
            .map_err(transport)?;
        let status = resp.status();
        // Throttled or failing upstream: the node did not judge the request, even when
        // the body is a JSON-RPC error (devnet answers 429 with one).
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(RpcError::Throttled(format!("HTTP {status}")));
        }
        if status.is_server_error() {
            return Err(RpcError::Transport(format!("HTTP {status}")));
        }
        let bytes = resp.bytes().await.map_err(transport)?;
        let reply: Value = serde_json::from_slice(&bytes)
            .map_err(|_| RpcError::InvalidResponse(format!("HTTP {status}: body is not JSON")))?;
        if let Some(err) = reply.get("error").filter(|e| !e.is_null()) {
            return Err(RpcError::Rpc {
                code: err.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: err
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown rpc error")
                    .to_string(),
                data: err.get("data").cloned(),
            });
        }
        let result = reply
            .get("result")
            .cloned()
            .ok_or_else(|| RpcError::InvalidResponse(format!("HTTP {status}: no result")))?;
        serde_json::from_value(result).map_err(|e| RpcError::InvalidResponse(e.to_string()))
    }

    /// `sendTransaction` of a base64 wire transaction with preflight at `confirmed`;
    /// returns the signature.
    pub async fn send_transaction(&self, tx_base64: &str) -> Result<String, RpcError> {
        self.call(
            "sendTransaction",
            json!([tx_base64, { "encoding": "base64", "skipPreflight": false, "preflightCommitment": "confirmed" }]),
        )
        .await
    }

    /// `getLatestBlockhash` at `confirmed`; returns the base58 blockhash.
    pub async fn get_latest_blockhash(&self) -> Result<String, RpcError> {
        let r: WithContext<LatestBlockhash> = self
            .call("getLatestBlockhash", json!([{ "commitment": "confirmed" }]))
            .await?;
        Ok(r.value.blockhash)
    }

    /// `getSignatureStatuses` for one signature; None until the cluster has seen it.
    pub async fn get_signature_status(
        &self,
        signature: &str,
    ) -> Result<Option<SignatureStatus>, RpcError> {
        let r: WithContext<Vec<Option<SignatureStatus>>> = self
            .call(
                "getSignatureStatuses",
                json!([[signature], { "searchTransactionHistory": false }]),
            )
            .await?;
        Ok(r.value.into_iter().next().flatten())
    }

    /// `requestAirdrop` of `lamports` to `pubkey`; returns the signature.
    pub async fn request_airdrop(&self, pubkey: &str, lamports: u64) -> Result<String, RpcError> {
        self.call("requestAirdrop", json!([pubkey, lamports])).await
    }
}

/// A transport error without the request URL, plus its causes (none of which carry it).
fn transport(e: reqwest::Error) -> RpcError {
    let e = e.without_url();
    let mut msg = e.to_string();
    let mut source = std::error::Error::source(&e);
    while let Some(s) = source {
        msg.push_str(": ");
        msg.push_str(&s.to_string());
        source = s.source();
    }
    RpcError::Transport(msg)
}

/// The host (and port) of an RPC URL, for logs: no scheme, credentials, path or query.
pub fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
}

/// The `{ context, value }` envelope of most read methods.
#[derive(Debug, Deserialize)]
struct WithContext<T> {
    value: T,
}

#[derive(Debug, Deserialize)]
struct LatestBlockhash {
    blockhash: String,
}

/// One entry of `getSignatureStatuses.value`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignatureStatus {
    /// The transaction's error, if it failed on chain.
    #[serde(default)]
    pub err: Option<Value>,
    /// `processed`, `confirmed` or `finalized`.
    #[serde(default)]
    pub confirmation_status: Option<String>,
}

impl SignatureStatus {
    /// Reached `confirmed` (or `finalized`).
    pub fn is_confirmed(&self) -> bool {
        matches!(
            self.confirmation_status.as_deref(),
            Some("confirmed" | "finalized")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_of_drops_everything_secret() {
        assert_eq!(
            host_of("https://solana-devnet.g.alchemy.com/v2/KEY"),
            "solana-devnet.g.alchemy.com"
        );
        assert_eq!(
            host_of("https://mainnet.helius-rpc.com/?api-key=KEY"),
            "mainnet.helius-rpc.com"
        );
        assert_eq!(host_of("http://user:pass@127.0.0.1:8899"), "127.0.0.1:8899");
        assert_eq!(host_of("https://h.example#frag"), "h.example");
        assert_eq!(host_of("h.example/KEY"), "h.example");
    }

    #[tokio::test]
    async fn transport_errors_carry_no_part_of_the_url() {
        // Port 1 on localhost: refused at once, nothing leaves the machine.
        let rpc = SolanaRpc::new(
            "http://127.0.0.1:1/v2/PATHKEY?api-key=QUERYKEY",
            Duration::from_secs(2),
        )
        .unwrap();
        let e = rpc.get_latest_blockhash().await.unwrap_err();
        assert!(matches!(e, RpcError::Transport(_)), "{e:?}");
        let msg = e.to_string();
        for secret in ["PATHKEY", "QUERYKEY", "api-key", "/v2", "for url"] {
            assert!(!msg.contains(secret), "{secret} in {msg}");
        }
    }
}
