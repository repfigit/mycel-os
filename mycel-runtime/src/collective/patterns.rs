//! Patterns - IPFS HTTP client, atomic 0600 pattern cache, and pattern types.
//!
//! Implements content-addressed IPFS storage (`/api/v0/add` and `/api/v0/cat`)
//! with SHA-256 verification, a 2 MiB response cap, and atomic `0600` local
//! persistence at `<context_path>/collective_patterns.json`.

use anyhow::{anyhow, Context as AnyhowContext, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::context::Context;

const MAX_IPFS_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
const IPFS_TIMEOUT_SECS: u64 = 15;
const COLLECTIVE_PATTERNS_FILENAME: &str = "collective_patterns.json";

/// Result of uploading bytes to IPFS via `/api/v0/add`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IpfsAddResult {
    pub cid: String,
    pub size: u64,
    pub sha256: String,
}

/// Pure-Rust HTTP client for an IPFS Kubo-compatible `/api/v0` API.
#[derive(Clone, Debug)]
pub struct IpfsClient {
    api_url: String,
    http_client: reqwest::Client,
}

impl IpfsClient {
    /// Construct a new IPFS HTTP client targeting `api_url` (e.g. `http://127.0.0.1:5001`).
    pub fn new(api_url: String) -> Result<Self> {
        let trimmed = api_url.trim().trim_end_matches('/').to_string();
        if trimmed.is_empty()
            || (!trimmed.starts_with("http://") && !trimmed.starts_with("https://"))
        {
            return Err(anyhow!("Invalid IPFS API URL: '{}'", api_url));
        }

        let http_client = reqwest::Client::builder()
            .timeout(Duration::from_secs(IPFS_TIMEOUT_SECS))
            .build()
            .context("Failed to build IPFS HTTP client")?;

        Ok(Self {
            api_url: trimmed,
            http_client,
        })
    }

    /// Upload bytes to `{api_url}/api/v0/add` using a pure-Rust `multipart/form-data` body.
    pub async fn add_bytes(&self, filename: &str, data: &[u8]) -> Result<IpfsAddResult> {
        let sha256 = sha256::digest(data);
        let safe_filename: String = filename
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let safe_filename = if safe_filename.is_empty() {
            "pattern.json".to_string()
        } else {
            safe_filename
        };

        let boundary = format!("----mycel-ipfs-{}", uuid::Uuid::new_v4().simple());
        let header = format!(
            "--{}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{}\"\r\nContent-Type: application/octet-stream\r\n\r\n",
            boundary, safe_filename
        );
        let trailer = format!("\r\n--{}--\r\n", boundary);

        let mut body = Vec::with_capacity(header.len() + data.len() + trailer.len());
        body.extend_from_slice(header.as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(trailer.as_bytes());

        let url = format!("{}/api/v0/add", self.api_url);
        let response = self
            .http_client
            .post(&url)
            .header(
                reqwest::header::CONTENT_TYPE,
                format!("multipart/form-data; boundary={}", boundary),
            )
            .body(body)
            .send()
            .await
            .with_context(|| format!("IPFS /api/v0/add request to {} failed", url))?;

        let status = response.status();
        if !status.is_success() {
            let err_body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "IPFS /api/v0/add failed ({}): {}",
                status,
                err_body
            ));
        }

        let parsed: serde_json::Value = response
            .json()
            .await
            .context("Invalid JSON response from IPFS /api/v0/add")?;

        let cid = parsed
            .get("Hash")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("IPFS /api/v0/add response missing 'Hash'"))?
            .to_string();

        validate_cid(&cid)?;

        let size = match parsed.get("Size") {
            Some(serde_json::Value::Number(n)) => n.as_u64().unwrap_or(data.len() as u64),
            Some(serde_json::Value::String(s)) => s.parse::<u64>().unwrap_or(data.len() as u64),
            _ => data.len() as u64,
        };

        Ok(IpfsAddResult { cid, size, sha256 })
    }

    /// Fetch bytes from `{api_url}/api/v0/cat?arg={cid}` (capped at 2 MiB) and
    /// optionally verify the SHA-256 hex digest.
    pub async fn cat_bytes(&self, cid: &str, expected_sha256: Option<&str>) -> Result<Vec<u8>> {
        validate_cid(cid)?;

        let url = format!("{}/api/v0/cat?arg={}", self.api_url, cid);
        let response = self
            .http_client
            .post(&url)
            .send()
            .await
            .with_context(|| format!("IPFS /api/v0/cat request to {} failed", url))?;

        let status = response.status();
        if !status.is_success() {
            let err_body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "IPFS /api/v0/cat failed ({}): {}",
                status,
                err_body
            ));
        }

        if let Some(len) = response.content_length() {
            if len as usize > MAX_IPFS_RESPONSE_BYTES {
                return Err(anyhow!(
                    "IPFS response exceeds maximum size of {} bytes",
                    MAX_IPFS_RESPONSE_BYTES
                ));
            }
        }

        let mut bytes = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk_res) = stream.next().await {
            let chunk = chunk_res.context("Failed reading IPFS response stream")?;
            if bytes.len() + chunk.len() > MAX_IPFS_RESPONSE_BYTES {
                return Err(anyhow!(
                    "IPFS response exceeded maximum size of {} bytes",
                    MAX_IPFS_RESPONSE_BYTES
                ));
            }
            bytes.extend_from_slice(&chunk);
        }

        if let Some(expected) = expected_sha256 {
            let expected_norm = expected.trim().to_ascii_lowercase();
            let actual = sha256::digest(bytes.as_slice());
            if actual != expected_norm {
                return Err(anyhow!(
                    "IPFS content SHA-256 mismatch for {}: expected {}, got {}",
                    cid,
                    expected_norm,
                    actual
                ));
            }
        }

        Ok(bytes)
    }
}

fn validate_cid(cid: &str) -> Result<()> {
    if cid.is_empty()
        || cid.len() > 128
        || !cid
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        return Err(anyhow!("Invalid IPFS CID '{}'", cid));
    }
    Ok(())
}

/// Stored collective pattern record (never auto-executed: `approved_for_execution` is
/// always `false` on unapproved or downloaded patterns).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CollectivePatternRecord {
    pub id: String,
    pub name: String,
    pub description: String,
    pub category: String,
    pub sha256: String,
    pub ipfs_cid: Option<String>,
    pub quality_score: f64,
    pub shared: bool,
    pub approved_for_execution: bool,
    pub source: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// Atomic `0600` JSON store for `CollectivePatternRecord`s at
/// `<context_path>/collective_patterns.json`.
#[derive(Debug, Clone)]
pub struct CollectivePatternCache {
    path: PathBuf,
    records: Vec<CollectivePatternRecord>,
}

#[allow(dead_code)]
impl CollectivePatternCache {
    /// Create an empty cache pointing at `<context_path>/collective_patterns.json`.
    #[allow(dead_code)]
    pub fn new(context_path: &Path) -> Self {
        Self {
            path: context_path.join(COLLECTIVE_PATTERNS_FILENAME),
            records: Vec::new(),
        }
    }

    /// Return the path to the backing JSON file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Load `<context_path>/collective_patterns.json` if it exists, or return an empty cache.
    pub async fn load_or_create(context_path: &Path) -> Result<Self> {
        let path = context_path.join(COLLECTIVE_PATTERNS_FILENAME);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
            let content = tokio::fs::read_to_string(&path)
                .await
                .with_context(|| format!("Failed to read {}", path.display()))?;
            let records: Vec<CollectivePatternRecord> = if content.trim().is_empty() {
                Vec::new()
            } else {
                serde_json::from_str(&content)
                    .with_context(|| format!("Failed to parse {}", path.display()))?
            };
            Ok(Self { path, records })
        } else {
            Ok(Self {
                path,
                records: Vec::new(),
            })
        }
    }

    /// Atomically persist records (`temp file + mode 0600 + sync_all + rename`).
    pub async fn save(&self) -> Result<()> {
        let path = self.path.clone();
        let data = serde_json::to_vec_pretty(&self.records)?;

        tokio::task::spawn_blocking(move || write_atomic_0600(&path, &data))
            .await
            .map_err(|e| anyhow!("Pattern cache save task failed: {}", e))?
    }

    /// Return all cached records.
    pub fn records(&self) -> &[CollectivePatternRecord] {
        &self.records
    }

    /// Insert or replace a record by `id`.
    pub fn upsert(&mut self, record: CollectivePatternRecord) {
        if let Some(existing) = self.records.iter_mut().find(|r| r.id == record.id) {
            *existing = record;
        } else {
            self.records.push(record);
        }
    }

    /// Number of cached patterns.
    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Number of patterns marked as shared.
    pub fn shared_count(&self) -> usize {
        self.records.iter().filter(|r| r.shared).count()
    }
}

fn write_atomic_0600(target: &Path, data: &[u8]) -> Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| anyhow!("Pattern cache path '{}' has no parent", target.display()))?;
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Failed to create directory {}", parent.display()))?;

    let tmp_name = format!(
        ".{}.tmp.{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("collective_patterns.json"),
        uuid::Uuid::new_v4().simple()
    );
    let tmp_path = parent.join(tmp_name);

    let write_res = (|| -> Result<()> {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut file = opts
            .open(&tmp_path)
            .with_context(|| format!("Failed to create temp file {}", tmp_path.display()))?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp_path, target)
            .with_context(|| format!("Failed to rename temp file to {}", target.display()))?;
        Ok(())
    })();

    if write_res.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_res
}

// ---------------------------------------------------------------------------
// Legacy / internal pattern structs retained for privacy & discovery helpers
// ---------------------------------------------------------------------------

/// Unique identifier for a pattern.
#[allow(dead_code)]
pub type PatternId = String;

/// A learned pattern that can be shared.
#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    pub id: PatternId,
    pub trigger: String,
    pub context_requirements: Vec<String>,
    pub solution: PatternSolution,
    pub domain: String,
    pub description: String,
    pub quality_score: f32,
    pub success_rate: f32,
    pub usage_count: u64,
    pub suggested_price: Option<u128>,
    pub source: PatternSource,
    pub creator: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[allow(dead_code)]
impl Pattern {
    pub fn new(
        trigger: String,
        solution: PatternSolution,
        domain: String,
        description: String,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            trigger,
            context_requirements: Vec::new(),
            solution,
            domain,
            description,
            quality_score: 0.0,
            success_rate: 0.0,
            usage_count: 0,
            suggested_price: None,
            source: PatternSource::Local,
            creator: None,
            created_at: chrono::Utc::now(),
        }
    }

    pub async fn apply(&self, context: &Context) -> Result<PatternResult> {
        match &self.solution {
            PatternSolution::PromptTemplate {
                template,
                variables,
            } => {
                let mut result = template.clone();
                for var in variables {
                    let value = match var.as_str() {
                        "working_directory" => context.working_directory.clone(),
                        "user_name" => context.user_name.clone().unwrap_or_default(),
                        "timestamp" => context.timestamp.to_rfc3339(),
                        _ => format!("{{{{{}}}}}", var),
                    };
                    result = result.replace(&format!("{{{{{}}}}}", var), &value);
                }
                Ok(PatternResult::Prompt(result))
            }
            PatternSolution::CodeTemplate { language, code, .. } => Ok(PatternResult::Code {
                language: language.clone(),
                code: code.replace("{{working_dir}}", &context.working_directory),
            }),
            PatternSolution::Workflow { steps } => Ok(PatternResult::Workflow(steps.clone())),
            PatternSolution::ModelAdapter {
                base_model,
                adapter_cid,
                ..
            } => Ok(PatternResult::Adapter {
                base_model: base_model.clone(),
                adapter_cid: adapter_cid.clone(),
            }),
        }
    }

    pub fn solution_summary(&self) -> String {
        match &self.solution {
            PatternSolution::PromptTemplate { template, .. } => {
                let prefix: String = template.chars().take(100).collect();
                format!("Prompt template: {}...", prefix)
            }
            PatternSolution::CodeTemplate { language, .. } => {
                format!("Code template ({})", language)
            }
            PatternSolution::Workflow { steps } => {
                format!("Workflow with {} steps", steps.len())
            }
            PatternSolution::ModelAdapter { base_model, .. } => {
                format!("Model adapter for {}", base_model)
            }
        }
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PatternSolution {
    PromptTemplate {
        template: String,
        variables: Vec<String>,
    },
    CodeTemplate {
        language: String,
        code: String,
        dependencies: Vec<String>,
    },
    Workflow {
        steps: Vec<WorkflowStep>,
    },
    ModelAdapter {
        base_model: String,
        adapter_cid: String,
        adapter_hash: String,
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    pub action: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum PatternResult {
    Prompt(String),
    Code {
        language: String,
        code: String,
    },
    Workflow(Vec<WorkflowStep>),
    Adapter {
        base_model: String,
        adapter_cid: String,
    },
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PatternSource {
    Local,
    Network,
    Builtin,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RankedPattern {
    pub pattern: Pattern,
    pub relevance_score: f64,
    pub combined_score: f64,
}

#[allow(dead_code)]
pub struct PatternStore {
    patterns: HashMap<PatternId, Pattern>,
    store_path: String,
}

#[allow(dead_code)]
impl PatternStore {
    pub async fn load_or_create(path: &str) -> Result<Self> {
        let patterns_file = Path::new(path).join("patterns.json");
        if patterns_file.exists() {
            let content = tokio::fs::read_to_string(&patterns_file).await?;
            let patterns: HashMap<PatternId, Pattern> = serde_json::from_str(&content)?;
            Ok(Self {
                patterns,
                store_path: path.to_string(),
            })
        } else {
            Ok(Self {
                patterns: HashMap::new(),
                store_path: path.to_string(),
            })
        }
    }

    pub async fn save(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.store_path).await?;
        let patterns_file = Path::new(&self.store_path).join("patterns.json");
        let content = serde_json::to_string_pretty(&self.patterns)?;
        tokio::fs::write(&patterns_file, content).await?;
        Ok(())
    }

    pub fn search(&self, domain: Option<&str>, query: &str) -> Vec<&Pattern> {
        let q = query.to_lowercase();
        self.patterns
            .values()
            .filter(|p| {
                let domain_match = domain.is_none_or(|d| p.domain == d);
                let query_match = p.trigger.to_lowercase().contains(&q)
                    || p.description.to_lowercase().contains(&q);
                domain_match && query_match
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_mock_ipfs(stored: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock ipfs");
        let addr = listener.local_addr().expect("local addr");
        let stored = std::sync::Arc::new(tokio::sync::RwLock::new(stored));

        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let stored = std::sync::Arc::clone(&stored);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 65536];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let first_line = req.lines().next().unwrap_or("");

                    if first_line.starts_with("POST /api/v0/add") {
                        let cid = "QmTestPatternCid123456789";
                        let body = format!(
                            r#"{{"Name":"pattern.json","Hash":"{}","Size":"{}"}}"#,
                            cid,
                            stored.read().await.len()
                        );
                        let resp = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        );
                        let _ = stream.write_all(resp.as_bytes()).await;
                    } else if first_line
                        .starts_with("POST /api/v0/cat?arg=QmTestPatternCid123456789")
                    {
                        let data = stored.read().await.clone();
                        let header = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            data.len()
                        );
                        let _ = stream.write_all(header.as_bytes()).await;
                        let _ = stream.write_all(&data).await;
                    } else {
                        let resp = "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                        let _ = stream.write_all(resp.as_bytes()).await;
                    }
                });
            }
        });

        format!("http://{}", addr)
    }

    #[tokio::test]
    async fn ipfs_add_and_cat_verifies_sha256_and_rejects_mismatch() {
        let payload =
            br#"{"name":"safe-pattern","description":"test","category":"general"}"#.to_vec();
        let expected_sha = sha256::digest(payload.as_slice());
        let url = spawn_mock_ipfs(payload.clone()).await;

        let client = IpfsClient::new(url).expect("ipfs client");
        let add_res = client
            .add_bytes("pattern.json", &payload)
            .await
            .expect("add_bytes");
        assert_eq!(add_res.cid, "QmTestPatternCid123456789");
        assert_eq!(add_res.sha256, expected_sha);

        let fetched = client
            .cat_bytes(&add_res.cid, Some(&expected_sha))
            .await
            .expect("cat_bytes match");
        assert_eq!(fetched, payload);

        let mismatch = client
            .cat_bytes(
                &add_res.cid,
                Some("0000000000000000000000000000000000000000000000000000000000000000"),
            )
            .await
            .expect_err("must reject sha256 mismatch");
        assert!(mismatch.to_string().contains("SHA-256 mismatch"));

        let bad_cid = client
            .cat_bytes("../etc/passwd", None)
            .await
            .expect_err("must reject invalid cid");
        assert!(bad_cid.to_string().contains("Invalid IPFS CID"));
    }

    #[tokio::test]
    async fn atomic_0600_pattern_cache_roundtrip() {
        let dir = std::env::temp_dir().join(format!("mycel-cache-test-{}", uuid::Uuid::new_v4()));
        let mut cache = CollectivePatternCache::load_or_create(&dir)
            .await
            .expect("create cache");
        assert!(cache.is_empty());

        let record = CollectivePatternRecord {
            id: "pat-1".to_string(),
            name: "Safe Pattern".to_string(),
            description: "Clean description".to_string(),
            category: "general".to_string(),
            sha256: "abc123".to_string(),
            ipfs_cid: None,
            quality_score: 0.8,
            shared: false,
            approved_for_execution: false,
            source: "local".to_string(),
            created_at: chrono::Utc::now(),
        };
        cache.upsert(record.clone());
        cache.save().await.expect("save cache");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(cache.path()).expect("metadata");
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        }

        let reloaded = CollectivePatternCache::load_or_create(&dir)
            .await
            .expect("reload cache");
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded.records()[0], record);
        assert!(!reloaded.records()[0].approved_for_execution);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
