//! Collective Intelligence Module
//!
//! Opt-in decentralized pattern evaluation, content-addressed IPFS storage,
//! NEAR RPC status/queries, and privacy-preserving PII redaction + DP noise.
//! Disabled by default (`enabled = false`).

pub mod bittensor;
pub mod discovery;
pub mod near;
pub mod patterns;
pub mod privacy;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::MycelConfig;
use crate::context::Context;

use self::bittensor::BittensorClient;
use self::near::NearClient;
use self::patterns::{CollectivePatternCache, CollectivePatternRecord, IpfsClient};
use self::privacy::{add_dp_noise_score, redact_pii, validate_category_allowed, PrivacyConfig};

fn default_near_network() -> String {
    "testnet".to_string()
}

fn default_near_contract() -> String {
    "patterns.mycel.testnet".to_string()
}

fn default_ipfs_api() -> String {
    "http://127.0.0.1:5001".to_string()
}

fn default_bittensor_network() -> String {
    "finney".to_string()
}

fn default_dp_epsilon() -> f64 {
    1.0
}

/// Configuration for opt-in collective intelligence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveConfig {
    #[serde(default)]
    pub enabled: bool,

    #[serde(default = "default_near_network")]
    pub near_network: String,

    #[serde(default)]
    pub near_rpc_url: String,

    #[serde(default)]
    pub near_account: String,

    #[serde(default = "default_near_contract")]
    pub near_contract: String,

    #[serde(default)]
    pub ipfs_enabled: bool,

    #[serde(default = "default_ipfs_api")]
    pub ipfs_api: String,

    #[serde(default)]
    pub bittensor_enabled: bool,

    #[serde(default)]
    pub bittensor_api_url: Option<String>,

    #[serde(default = "default_bittensor_network")]
    pub bittensor_network: String,

    #[serde(default)]
    pub bittensor_subnet: u16,

    #[serde(default)]
    pub share_patterns: bool,

    #[serde(default = "default_dp_epsilon")]
    pub differential_privacy_epsilon: f64,
}

impl CollectiveConfig {
    /// Return `near_rpc_url` if non-empty, otherwise the default RPC URL for `near_network`.
    pub fn effective_near_rpc_url(&self) -> &str {
        if !self.near_rpc_url.trim().is_empty() {
            &self.near_rpc_url
        } else if self.near_network.eq_ignore_ascii_case("mainnet") {
            "https://rpc.mainnet.near.org"
        } else {
            "https://rpc.testnet.near.org"
        }
    }
}

impl Default for CollectiveConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            near_network: default_near_network(),
            near_rpc_url: String::new(),
            near_account: String::new(),
            near_contract: default_near_contract(),
            ipfs_enabled: false,
            ipfs_api: default_ipfs_api(),
            bittensor_enabled: false,
            bittensor_api_url: None,
            bittensor_network: default_bittensor_network(),
            bittensor_subnet: 0,
            share_patterns: false,
            differential_privacy_epsilon: default_dp_epsilon(),
        }
    }
}

/// Runtime status snapshot for the collective subsystem.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CollectiveStatusInfo {
    pub enabled: bool,
    pub near_network: String,
    pub near_rpc_url: String,
    pub near_account: String,
    pub near_connected: bool,
    pub ipfs_enabled: bool,
    pub ipfs_api: String,
    pub bittensor_enabled: bool,
    pub local_patterns: usize,
    pub patterns_shared: usize,
    pub share_patterns_enabled: bool,
}

/// Opt-in collective intelligence service.
#[derive(Clone)]
pub struct CollectiveService {
    enabled: bool,
    config: CollectiveConfig,
    near_client: Option<NearClient>,
    ipfs_client: Option<IpfsClient>,
    bittensor_client: BittensorClient,
    cache: Arc<RwLock<CollectivePatternCache>>,
}

#[allow(dead_code)]
impl CollectiveService {
    /// Construct a `CollectiveService` from `MycelConfig`.
    ///
    /// When disabled (`!config.collective.enabled || force_disabled`), makes zero
    /// network calls and loads the local cache cleanly.
    pub async fn new(config: &MycelConfig, force_disabled: bool) -> Result<Self> {
        Self::from_collective_config(
            config.collective.clone(),
            Path::new(&config.context_path),
            force_disabled,
        )
        .await
    }

    /// Construct a `CollectiveService` directly from a `CollectiveConfig` and `context_path`.
    pub async fn from_collective_config(
        collective_config: CollectiveConfig,
        context_path: &Path,
        force_disabled: bool,
    ) -> Result<Self> {
        let enabled = collective_config.enabled && !force_disabled;

        let near_client = if enabled {
            Some(NearClient::new(collective_config.effective_near_rpc_url())?)
        } else {
            None
        };

        let ipfs_client = if enabled && collective_config.ipfs_enabled {
            Some(IpfsClient::new(collective_config.ipfs_api.clone())?)
        } else {
            None
        };

        let bittensor_api = if enabled && collective_config.bittensor_enabled {
            collective_config.bittensor_api_url.clone()
        } else {
            None
        };
        let bittensor_client = BittensorClient::new(
            bittensor_api,
            collective_config.bittensor_network.clone(),
            collective_config.bittensor_subnet,
        )?;

        let cache = CollectivePatternCache::load_or_create(context_path).await?;

        Ok(Self {
            enabled,
            config: collective_config,
            near_client,
            ipfs_client,
            bittensor_client,
            cache: Arc::new(RwLock::new(cache)),
        })
    }

    /// Return whether collective intelligence is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Return collective status; probes NEAR `status()` only when enabled.
    pub async fn status(&self) -> CollectiveStatusInfo {
        let near_connected = if self.enabled {
            match self.near_client.as_ref() {
                Some(client) => client.status().await.is_ok(),
                None => false,
            }
        } else {
            false
        };

        let cache = self.cache.read().await;
        CollectiveStatusInfo {
            enabled: self.enabled,
            near_network: self.config.near_network.clone(),
            near_rpc_url: self.config.effective_near_rpc_url().to_string(),
            near_account: self.config.near_account.clone(),
            near_connected,
            ipfs_enabled: self.config.ipfs_enabled,
            ipfs_api: self.config.ipfs_api.clone(),
            bittensor_enabled: self.config.bittensor_enabled,
            local_patterns: cache.len(),
            patterns_shared: cache.shared_count(),
            share_patterns_enabled: self.config.share_patterns,
        }
    }

    /// List all locally cached collective pattern records.
    pub async fn list_patterns(&self) -> Vec<CollectivePatternRecord> {
        let cache = self.cache.read().await;
        cache.records().to_vec()
    }

    /// Validate, redact PII, score with DP noise, optionally upload to IPFS,
    /// and atomically persist a pattern record.
    pub async fn share_pattern(
        &self,
        name: &str,
        description: &str,
        category: &str,
    ) -> Result<CollectivePatternRecord> {
        if !self.enabled {
            return Err(anyhow!(
                "Collective intelligence is disabled; enable [collective] in config.toml"
            ));
        }
        if !self.config.share_patterns {
            return Err(anyhow!(
                "Pattern sharing is disabled; set share_patterns = true in [collective] to share patterns"
            ));
        }

        let privacy_cfg = PrivacyConfig {
            epsilon: self.config.differential_privacy_epsilon,
            ..PrivacyConfig::default()
        };
        let combined_raw = format!("{} {}", name, description);
        validate_category_allowed(&combined_raw, category, &privacy_cfg)?;

        let (redacted_name, _) = redact_pii(name.trim());
        let (redacted_description, _) = redact_pii(description.trim());
        let normalized_category = if category.trim().is_empty() {
            "general".to_string()
        } else {
            category.trim().to_string()
        };

        if redacted_name.is_empty() || redacted_description.is_empty() {
            return Err(anyhow!("Pattern name and description must be non-empty"));
        }

        let eval = self
            .bittensor_client
            .evaluate_pattern_record(&redacted_name, &redacted_description, &normalized_category)
            .await;
        let quality_score =
            add_dp_noise_score(eval.score, self.config.differential_privacy_epsilon);

        let id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now();

        let payload_json = serde_json::to_vec(&serde_json::json!({
            "id": id,
            "name": redacted_name,
            "description": redacted_description,
            "category": normalized_category,
            "quality_score": quality_score,
            "created_at": created_at,
        }))?;
        let mut sha256_hex = sha256::digest(payload_json.as_slice());

        let (ipfs_cid, shared) = if let Some(ref ipfs) = self.ipfs_client {
            let add_res = ipfs
                .add_bytes(&format!("{}.json", id), &payload_json)
                .await?;
            sha256_hex = add_res.sha256;
            (Some(add_res.cid), true)
        } else {
            (None, self.config.share_patterns)
        };

        let record = CollectivePatternRecord {
            id,
            name: redacted_name,
            description: redacted_description,
            category: normalized_category,
            sha256: sha256_hex,
            ipfs_cid,
            quality_score,
            shared,
            approved_for_execution: false,
            source: "local".to_string(),
            created_at,
        };

        let mut cache = self.cache.write().await;
        cache.upsert(record.clone());
        cache.save().await?;

        Ok(record)
    }

    /// Fetch a pattern from IPFS, verify its SHA-256 digest, force
    /// `approved_for_execution = false`, and store it locally.
    pub async fn fetch_verified_pattern(
        &self,
        cid: &str,
        expected_sha256: &str,
    ) -> Result<CollectivePatternRecord> {
        if !self.enabled {
            return Err(anyhow!(
                "Collective intelligence is disabled; enable [collective] in config.toml"
            ));
        }
        let ipfs = self.ipfs_client.as_ref().ok_or_else(|| {
            anyhow!("IPFS is disabled; set ipfs_enabled = true in [collective] to fetch patterns")
        })?;

        let bytes = ipfs.cat_bytes(cid, Some(expected_sha256)).await?;
        let val: serde_json::Value = serde_json::from_slice(&bytes)?;

        let raw_name = val
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Fetched pattern JSON missing 'name'"))?;
        let raw_desc = val
            .get("description")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Fetched pattern JSON missing 'description'"))?;
        let category = val
            .get("category")
            .and_then(|v| v.as_str())
            .unwrap_or("general")
            .to_string();

        let (name, _) = redact_pii(raw_name);
        let (description, _) = redact_pii(raw_desc);

        let id = val
            .get("id")
            .and_then(|v| v.as_str())
            .map(ToString::to_string)
            .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());

        let quality_score = val
            .get("quality_score")
            .and_then(|v| v.as_f64())
            .filter(|s| s.is_finite())
            .map(|s| s.clamp(0.0, 1.0))
            .unwrap_or(0.5);

        let created_at = val
            .get("created_at")
            .and_then(|v| v.as_str())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|dt| dt.with_timezone(&chrono::Utc))
            .unwrap_or_else(chrono::Utc::now);

        let record = CollectivePatternRecord {
            id,
            name,
            description,
            category,
            sha256: expected_sha256.trim().to_ascii_lowercase(),
            ipfs_cid: Some(cid.to_string()),
            quality_score,
            shared: false,
            approved_for_execution: false,
            source: "ipfs".to_string(),
            created_at,
        };

        let mut cache = self.cache.write().await;
        cache.upsert(record.clone());
        cache.save().await?;

        Ok(record)
    }
}

/// An interaction that can be learned from.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Interaction {
    pub id: String,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub user_input: String,
    pub ai_response: String,
    pub context_snapshot: Context,
    pub success: bool,
    pub user_rating: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn collective_config_defaults_are_safe_and_disabled() {
        let cfg = CollectiveConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.near_network, "testnet");
        assert_eq!(cfg.near_rpc_url, "");
        assert_eq!(cfg.effective_near_rpc_url(), "https://rpc.testnet.near.org");
        assert_eq!(cfg.near_contract, "patterns.mycel.testnet");
        assert!(!cfg.ipfs_enabled);
        assert_eq!(cfg.ipfs_api, "http://127.0.0.1:5001");
        assert!(!cfg.bittensor_enabled);
        assert!(cfg.bittensor_api_url.is_none());
        assert_eq!(cfg.bittensor_network, "finney");
        assert_eq!(cfg.bittensor_subnet, 0);
        assert!(!cfg.share_patterns);
        assert!((cfg.differential_privacy_epsilon - 1.0).abs() < 1e-9);

        let mainnet_cfg = CollectiveConfig {
            near_network: "mainnet".to_string(),
            ..Default::default()
        };
        assert_eq!(
            mainnet_cfg.effective_near_rpc_url(),
            "https://rpc.mainnet.near.org"
        );

        let custom_cfg = CollectiveConfig {
            near_rpc_url: "http://127.0.0.1:3030".to_string(),
            ..Default::default()
        };
        assert_eq!(custom_cfg.effective_near_rpc_url(), "http://127.0.0.1:3030");
    }

    #[tokio::test]
    async fn disabled_collective_service_makes_no_network_calls_and_refuses_sharing() {
        let dir =
            std::env::temp_dir().join(format!("mycel-collective-off-{}", uuid::Uuid::new_v4()));
        let cfg = CollectiveConfig {
            enabled: false,
            near_rpc_url: "http://127.0.0.1:1".to_string(),
            ..Default::default()
        };

        let svc = CollectiveService::from_collective_config(cfg, &dir, false)
            .await
            .expect("service init");
        assert!(!svc.is_enabled());

        let status = svc.status().await;
        assert!(!status.enabled);
        assert!(!status.near_connected);
        assert_eq!(status.local_patterns, 0);

        let err = svc
            .share_pattern("name", "desc", "general")
            .await
            .expect_err("disabled service must refuse sharing");
        assert!(err.to_string().contains("disabled"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn enabled_collective_service_shares_fetches_and_enforces_t6() {
        let dir =
            std::env::temp_dir().join(format!("mycel-collective-on-{}", uuid::Uuid::new_v4()));

        // Mock NEAR RPC server
        let near_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind near");
        let near_addr = near_listener.local_addr().expect("near addr");
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = near_listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let body = r#"{"jsonrpc":"2.0","id":"mycel-status","result":{"chain_id":"testnet","sync_info":{"latest_block_height":42}}}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes()).await;
            }
        });

        // Mock IPFS server
        let fetched_payload = serde_json::to_vec(&serde_json::json!({
            "id": "remote-1",
            "name": "Remote Pattern",
            "description": "Fetched from IPFS safely",
            "category": "coding",
            "quality_score": 0.9
        }))
        .expect("serialize");
        let fetched_sha = sha256::digest(fetched_payload.as_slice());

        let ipfs_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ipfs");
        let ipfs_addr = ipfs_listener.local_addr().expect("ipfs addr");
        let fetched_payload_clone = fetched_payload.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = ipfs_listener.accept().await {
                let mut buf = vec![0u8; 32768];
                let n = stream.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let first_line = req.lines().next().unwrap_or("");
                if first_line.starts_with("POST /api/v0/add") {
                    let body = r#"{"Name":"p.json","Hash":"QmUploadedCid999","Size":"128"}"#;
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                } else if first_line.starts_with("POST /api/v0/cat?arg=QmRemoteCid123") {
                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        fetched_payload_clone.len()
                    );
                    let _ = stream.write_all(header.as_bytes()).await;
                    let _ = stream.write_all(&fetched_payload_clone).await;
                } else {
                    let _ = stream
                        .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                        .await;
                }
            }
        });

        let cfg = CollectiveConfig {
            enabled: true,
            near_rpc_url: format!("http://{}", near_addr),
            ipfs_enabled: true,
            ipfs_api: format!("http://{}", ipfs_addr),
            share_patterns: true,
            differential_privacy_epsilon: 1.0,
            ..Default::default()
        };

        let svc = CollectiveService::from_collective_config(cfg, &dir, false)
            .await
            .expect("service init");
        assert!(svc.is_enabled());

        let status = svc.status().await;
        assert!(status.near_connected);

        // Share pattern with PII -> redacted, DP-scored, uploaded, approved_for_execution == false
        let shared = svc
            .share_pattern(
                "Debug alice@example.com",
                "Check /home/alice/project/log and sk-secret12345678",
                "coding",
            )
            .await
            .expect("share_pattern");
        assert!(!shared.name.contains("alice@example.com"));
        assert!(!shared.description.contains("/home/alice/project/log"));
        assert!(!shared.description.contains("sk-secret12345678"));
        assert_eq!(shared.ipfs_cid.as_deref(), Some("QmUploadedCid999"));
        assert!(!shared.approved_for_execution);

        // Blocked category rejected
        let blocked = svc
            .share_pattern("Bank transfer", "Send money", "financial")
            .await
            .expect_err("financial must be rejected");
        assert!(blocked.to_string().contains("blocked"));

        // Fetch verified pattern -> approved_for_execution == false
        let fetched = svc
            .fetch_verified_pattern("QmRemoteCid123", &fetched_sha)
            .await
            .expect("fetch_verified_pattern");
        assert_eq!(fetched.id, "remote-1");
        assert!(!fetched.approved_for_execution);

        let all = svc.list_patterns().await;
        assert_eq!(all.len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn share_pattern_refused_and_no_ipfs_upload_when_share_patterns_false() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let dir =
            std::env::temp_dir().join(format!("mycel-collective-noshare-{}", uuid::Uuid::new_v4()));

        let ipfs_listener = TcpListener::bind("127.0.0.1:0").await.expect("bind ipfs");
        let ipfs_addr = ipfs_listener.local_addr().expect("ipfs addr");
        let request_count = Arc::new(AtomicUsize::new(0));
        let request_count_clone = Arc::clone(&request_count);
        let server_task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = ipfs_listener.accept().await {
                request_count_clone.fetch_add(1, Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let _ = stream
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                    .await;
            }
        });

        let cfg = CollectiveConfig {
            enabled: true,
            ipfs_enabled: true,
            ipfs_api: format!("http://{}", ipfs_addr),
            share_patterns: false,
            ..Default::default()
        };

        let svc = CollectiveService::from_collective_config(cfg, &dir, false)
            .await
            .expect("service init");
        assert!(svc.is_enabled());

        let err = svc
            .share_pattern("Safe pattern", "Should not upload", "coding")
            .await
            .expect_err("share_pattern must refuse when share_patterns = false");
        assert!(err.to_string().contains("share_patterns"));
        assert_eq!(
            request_count.load(Ordering::SeqCst),
            0,
            "mock IPFS server must receive 0 requests when share_patterns = false"
        );
        assert!(svc.list_patterns().await.is_empty());

        server_task.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
