//! Minimal Solana JSON-RPC client. Every method goes through `call`, which separates
//! a JSON-RPC error response (the node answered "no") from transport failures and
//! unparseable replies (we don't know what happened), because §2.7 treats them differently.

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
    /// Connect / timeout / HTTP failure without a JSON-RPC body.
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
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        Ok(Self {
            http,
            url: url.to_string(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
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
            .map_err(|e| RpcError::Transport(e.to_string()))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| RpcError::Transport(e.to_string()))?;
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
