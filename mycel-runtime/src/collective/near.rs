//! NEAR Protocol JSON-RPC 2.0 Client.
//!
//! Provides real HTTP JSON-RPC 2.0 calls (`status`, `view_account`, `call_function`)
//! against a configurable NEAR RPC endpoint.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use std::time::Duration;

const DEFAULT_RPC_TIMEOUT_SECS: u64 = 10;

/// Status returned by NEAR JSON-RPC `status`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NearNodeStatus {
    pub chain_id: String,
    pub latest_block_height: u64,
}

/// Account view returned by NEAR JSON-RPC `query` (`view_account`).
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NearAccountView {
    pub amount: u128,
    pub storage_usage: u64,
}

/// NEAR JSON-RPC 2.0 client.
#[derive(Clone, Debug)]
pub struct NearClient {
    rpc_url: String,
    http_client: reqwest::Client,
}

impl NearClient {
    /// Create a new NEAR JSON-RPC 2.0 client with a 10-second request timeout.
    pub fn new(rpc_url: impl Into<String>) -> Result<Self> {
        let rpc_url = rpc_url.into().trim().trim_end_matches('/').to_string();
        if rpc_url.is_empty() {
            return Err(anyhow!("NEAR RPC URL cannot be empty"));
        }

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(DEFAULT_RPC_TIMEOUT_SECS))
            .build()
            .context("Failed to build NEAR HTTP client")?;

        Ok(Self {
            rpc_url,
            http_client,
        })
    }

    /// Return the configured RPC endpoint URL.
    #[allow(dead_code)]
    pub fn rpc_url(&self) -> &str {
        &self.rpc_url
    }

    /// Query node status via `method: "status"`.
    pub async fn status(&self) -> Result<NearNodeStatus> {
        let result = self
            .rpc_call("mycel-status", "status", serde_json::json!([]))
            .await?;

        let chain_id = result
            .get("chain_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("NEAR status response missing 'chain_id'"))?
            .to_string();

        let latest_block_height = result
            .get("sync_info")
            .and_then(|s| s.get("latest_block_height"))
            .or_else(|| result.get("latest_block_height"))
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("NEAR status response missing 'latest_block_height'"))?;

        Ok(NearNodeStatus {
            chain_id,
            latest_block_height,
        })
    }

    /// Query account balance and storage via `request_type: "view_account"`.
    #[allow(dead_code)]
    pub async fn view_account(&self, account_id: &str) -> Result<NearAccountView> {
        let account_id = account_id.trim();
        if account_id.is_empty() {
            return Err(anyhow!("NEAR account_id cannot be empty"));
        }

        let result = self
            .rpc_call(
                "mycel-view-account",
                "query",
                serde_json::json!({
                    "request_type": "view_account",
                    "finality": "final",
                    "account_id": account_id,
                }),
            )
            .await?;

        let amount = match result.get("amount") {
            Some(serde_json::Value::String(s)) => s
                .parse::<u128>()
                .with_context(|| format!("Invalid NEAR account amount '{}'", s))?,
            Some(serde_json::Value::Number(n)) => n
                .as_u64()
                .map(u128::from)
                .ok_or_else(|| anyhow!("Invalid numeric NEAR account amount"))?,
            _ => return Err(anyhow!("NEAR view_account response missing 'amount'")),
        };

        let storage_usage = result
            .get("storage_usage")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("NEAR view_account response missing 'storage_usage'"))?;

        Ok(NearAccountView {
            amount,
            storage_usage,
        })
    }

    /// Call a read-only contract view method via `request_type: "call_function"`.
    ///
    /// Base64-encodes `args` and decodes both NEAR's standard `[u8]` byte-array
    /// `result.result` and base64-encoded string `result.result` into JSON.
    #[allow(dead_code)]
    pub async fn call_function(
        &self,
        contract_id: &str,
        method_name: &str,
        args: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let contract_id = contract_id.trim();
        let method_name = method_name.trim();
        if contract_id.is_empty() || method_name.is_empty() {
            return Err(anyhow!(
                "NEAR contract_id and method_name must be non-empty"
            ));
        }

        let args_bytes = serde_json::to_vec(&args)?;
        let args_base64 = base64::engine::general_purpose::STANDARD.encode(args_bytes);

        let result = self
            .rpc_call(
                "mycel-call-function",
                "query",
                serde_json::json!({
                    "request_type": "call_function",
                    "finality": "final",
                    "account_id": contract_id,
                    "method_name": method_name,
                    "args_base64": args_base64,
                }),
            )
            .await?;

        let raw_result = result
            .get("result")
            .ok_or_else(|| anyhow!("NEAR call_function response missing 'result'"))?;

        let decoded_bytes = match raw_result {
            serde_json::Value::Array(arr) => {
                let mut bytes = Vec::with_capacity(arr.len());
                for item in arr {
                    let num = item
                        .as_u64()
                        .ok_or_else(|| anyhow!("Non-integer element in NEAR result byte array"))?;
                    let byte = u8::try_from(num)
                        .map_err(|_| anyhow!("Byte value out of range in NEAR result: {}", num))?;
                    bytes.push(byte);
                }
                bytes
            }
            serde_json::Value::String(b64) => base64::engine::general_purpose::STANDARD
                .decode(b64.as_bytes())
                .context("Failed to decode base64 NEAR call_function result")?,
            _ => {
                return Err(anyhow!(
                    "Unsupported NEAR call_function 'result' format (expected byte array or base64 string)"
                ))
            }
        };

        let parsed: serde_json::Value = serde_json::from_slice(&decoded_bytes)
            .context("Failed to parse decoded NEAR call_function result as JSON")?;
        Ok(parsed)
    }

    async fn rpc_call(
        &self,
        id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let payload = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });

        let response = self
            .http_client
            .post(&self.rpc_url)
            .json(&payload)
            .send()
            .await
            .with_context(|| format!("NEAR RPC request '{}' failed", method))?;

        let status = response.status();
        if !status.is_success() {
            let body_text = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "NEAR RPC '{}' returned HTTP {}: {}",
                method,
                status,
                body_text
            ));
        }

        let body: serde_json::Value = response
            .json()
            .await
            .with_context(|| format!("Invalid JSON in NEAR RPC '{}' response", method))?;

        if let Some(error) = body.get("error") {
            if !error.is_null() {
                return Err(anyhow!("NEAR RPC '{}' returned error: {}", method, error));
            }
        }

        body.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("NEAR RPC '{}' response missing 'result'", method))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn read_http_request(stream: &mut tokio::net::TcpStream) -> String {
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        String::from_utf8_lossy(&buf[..n]).to_string()
    }

    async fn spawn_mock_rpc<F>(handler: F) -> String
    where
        F: Fn(serde_json::Value) -> (u16, serde_json::Value) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock rpc");
        let addr = listener.local_addr().expect("local addr");
        let handler = std::sync::Arc::new(handler);

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let handler = std::sync::Arc::clone(&handler);
                tokio::spawn(async move {
                    let raw = read_http_request(&mut stream).await;
                    let body_str = raw.split("\r\n\r\n").nth(1).unwrap_or("{}");
                    let req_json: serde_json::Value =
                        serde_json::from_str(body_str).unwrap_or(serde_json::Value::Null);
                    let (status_code, resp_json) = handler(req_json);
                    let resp_body = serde_json::to_string(&resp_json).unwrap_or_default();
                    let status_text = if status_code == 200 {
                        "OK"
                    } else {
                        "Bad Request"
                    };
                    let http = format!(
                        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        status_code,
                        status_text,
                        resp_body.len(),
                        resp_body
                    );
                    let _ = stream.write_all(http.as_bytes()).await;
                });
            }
        });

        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn near_client_status_view_account_and_call_function() {
        let url = spawn_mock_rpc(|req| {
            let method = req["method"].as_str().unwrap_or("");
            match method {
                "status" => (
                    200,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": req["id"],
                        "result": {
                            "chain_id": "testnet",
                            "sync_info": { "latest_block_height": 123456789u64 }
                        }
                    }),
                ),
                "query" => {
                    let req_type = req["params"]["request_type"].as_str().unwrap_or("");
                    if req_type == "view_account" {
                        (
                            200,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": req["id"],
                                "result": {
                                    "amount": "1500000000000000000000000",
                                    "storage_usage": 4096u64
                                }
                            }),
                        )
                    } else if req_type == "call_function" {
                        let args_b64 = req["params"]["args_base64"].as_str().unwrap_or("");
                        let decoded = base64::engine::general_purpose::STANDARD
                            .decode(args_b64)
                            .expect("valid base64 args");
                        let args_val: serde_json::Value =
                            serde_json::from_slice(&decoded).expect("valid json args");
                        let mode = args_val["mode"].as_str().unwrap_or("bytes");
                        let payload = serde_json::json!({ "ok": true, "echo": mode });
                        let payload_bytes = serde_json::to_vec(&payload).expect("serialize");

                        if mode == "base64" {
                            let encoded =
                                base64::engine::general_purpose::STANDARD.encode(payload_bytes);
                            (
                                200,
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": req["id"],
                                    "result": { "result": encoded }
                                }),
                            )
                        } else {
                            (
                                200,
                                serde_json::json!({
                                    "jsonrpc": "2.0",
                                    "id": req["id"],
                                    "result": { "result": payload_bytes }
                                }),
                            )
                        }
                    } else {
                        (
                            200,
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": req["id"],
                                "error": { "code": -32601, "message": "unknown request_type" }
                            }),
                        )
                    }
                }
                _ => (
                    400,
                    serde_json::json!({
                        "jsonrpc": "2.0",
                        "error": { "code": -32601, "message": "method not found" }
                    }),
                ),
            }
        })
        .await;

        let client = NearClient::new(url).expect("near client");

        let st = client.status().await.expect("status");
        assert_eq!(st.chain_id, "testnet");
        assert_eq!(st.latest_block_height, 123_456_789);

        let acct = client
            .view_account("alice.testnet")
            .await
            .expect("view_account");
        assert_eq!(acct.amount, 1_500_000_000_000_000_000_000_000u128);
        assert_eq!(acct.storage_usage, 4096);

        let res_bytes = client
            .call_function(
                "patterns.mycel.testnet",
                "get_pattern",
                serde_json::json!({ "mode": "bytes" }),
            )
            .await
            .expect("call_function bytes");
        assert_eq!(res_bytes["ok"], true);
        assert_eq!(res_bytes["echo"], "bytes");

        let res_b64 = client
            .call_function(
                "patterns.mycel.testnet",
                "get_pattern",
                serde_json::json!({ "mode": "base64" }),
            )
            .await
            .expect("call_function base64");
        assert_eq!(res_b64["ok"], true);
        assert_eq!(res_b64["echo"], "base64");
    }

    #[tokio::test]
    async fn near_client_propagates_rpc_error() {
        let url = spawn_mock_rpc(|req| {
            (
                200,
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": req["id"],
                    "error": { "code": -32000, "message": "account does not exist" }
                }),
            )
        })
        .await;

        let client = NearClient::new(url).expect("near client");
        let err = client
            .view_account("missing.testnet")
            .await
            .expect_err("should fail on jsonrpc error");
        assert!(err.to_string().contains("account does not exist"));
    }
}
