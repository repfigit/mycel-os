//! Discovery - Find relevant patterns from local cache and network
//!
//! Implements multi-source pattern discovery with intelligent ranking.

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

use super::bittensor::BittensorClient;
use super::near::NearClient;
use super::patterns::{Pattern, PatternSource, PatternStore, RankedPattern};
use crate::context::Context;

/// Pattern discovery across local and network sources
pub struct PatternDiscovery {
    near_client: Option<NearClient>,
    bittensor_client: Option<BittensorClient>,
    local_store: Arc<RwLock<PatternStore>>,
    cache: DiscoveryCache,
}

impl PatternDiscovery {
    pub fn new(
        near_client: Option<NearClient>,
        bittensor_client: Option<BittensorClient>,
        local_store: Arc<RwLock<PatternStore>>,
    ) -> Self {
        Self {
            near_client,
            bittensor_client,
            local_store,
            cache: DiscoveryCache::new(),
        }
    }

    /// Discover patterns relevant to the current context
    pub async fn discover(&self, context: &Context) -> Result<Vec<RankedPattern>> {
        debug!("Discovering patterns for context");

        // Check cache first
        let cache_key = self.compute_cache_key(context);
        if let Some(cached) = self.cache.get(&cache_key).await {
            debug!("Cache hit for pattern discovery");
            return Ok(cached);
        }

        // Gather patterns from all sources
        let mut all_patterns = Vec::new();

        // 1. Local patterns (fastest)
        let local_patterns = self.search_local(context).await?;
        debug!("Found {} local patterns", local_patterns.len());
        all_patterns.extend(local_patterns);

        // 2. NEAR registry patterns
        if let Some(ref near) = self.near_client {
            match self.search_near(near, context).await {
                Ok(patterns) => {
                    debug!("Found {} NEAR patterns", patterns.len());
                    all_patterns.extend(patterns);
                }
                Err(e) => {
                    debug!("NEAR search failed: {}", e);
                }
            }
        }

        // 3. Bittensor semantic search
        if let Some(ref bt) = self.bittensor_client {
            match self.search_bittensor(bt, context).await {
                Ok(patterns) => {
                    debug!("Found {} Bittensor patterns", patterns.len());
                    all_patterns.extend(patterns);
                }
                Err(e) => {
                    debug!("Bittensor search failed: {}", e);
                }
            }
        }

        // Deduplicate
        let deduped = self.deduplicate(all_patterns);

        // Rank patterns
        let ranked = self.rank_patterns(deduped, context);

        // Cache results
        self.cache.set(&cache_key, ranked.clone()).await;

        Ok(ranked)
    }

    async fn search_local(&self, context: &Context) -> Result<Vec<DiscoveredPattern>> {
        let store = self.local_store.read().await;

        // Infer domain from context
        let domain = self.infer_domain(context);

        // Search by domain and recent activity
        let patterns = store.search(Some(&domain), "");

        Ok(patterns
            .into_iter()
            .map(|p| DiscoveredPattern {
                pattern: p.clone(),
                source: PatternSource::Local,
                source_score: 1.0, // Local patterns get a boost
                fetch_time_ms: 0,
            })
            .collect())
    }

    async fn search_near(
        &self,
        near: &NearClient,
        context: &Context,
    ) -> Result<Vec<DiscoveredPattern>> {
        let domain = self.infer_domain(context);

        let query = super::near::PatternQuery {
            domain: Some(domain),
            min_reputation: 0.6,
            max_price: Some(1_000_000_000_000_000_000_000_000), // 1 NEAR
            limit: 20,
        };

        let start = std::time::Instant::now();
        let entries = near.query_patterns(query).await?;
        let fetch_time = start.elapsed().as_millis() as u64;

        // Convert entries to patterns (would need to fetch full data)
        let patterns: Vec<DiscoveredPattern> = entries
            .into_iter()
            .map(|e| DiscoveredPattern {
                pattern: Pattern {
                    id: e.id,
                    trigger: "".to_string(), // Would fetch from IPFS
                    context_requirements: Vec::new(),
                    solution: super::patterns::PatternSolution::PromptTemplate {
                        template: "".to_string(),
                        variables: Vec::new(),
                    },
                    domain: e.domain,
                    description: "".to_string(),
                    quality_score: e.reputation_score as f32,
                    success_rate: 0.0,
                    usage_count: e.usage_count,
                    suggested_price: Some(e.price_per_use),
                    source: PatternSource::Network,
                    creator: Some(e.creator),
                    created_at: chrono::Utc::now(),
                },
                source: PatternSource::Network,
                source_score: e.reputation_score,
                fetch_time_ms: fetch_time,
            })
            .collect();

        Ok(patterns)
    }

    async fn search_bittensor(
        &self,
        bt: &BittensorClient,
        context: &Context,
    ) -> Result<Vec<DiscoveredPattern>> {
        // Compute embedding for semantic search
        let embedding = self.compute_context_embedding(context)?;

        let start = std::time::Instant::now();
        let matches = bt.semantic_pattern_search(embedding, 10).await?;
        let fetch_time = start.elapsed().as_millis() as u64;

        let patterns: Vec<DiscoveredPattern> = matches
            .into_iter()
            .map(|m| DiscoveredPattern {
                pattern: Pattern {
                    id: m.pattern_id,
                    trigger: "".to_string(),
                    context_requirements: Vec::new(),
                    solution: super::patterns::PatternSolution::PromptTemplate {
                        template: "".to_string(),
                        variables: Vec::new(),
                    },
                    domain: "".to_string(),
                    description: "".to_string(),
                    quality_score: m.similarity as f32,
                    success_rate: 0.0,
                    usage_count: 0,
                    suggested_price: None,
                    source: PatternSource::Network,
                    creator: None,
                    created_at: chrono::Utc::now(),
                },
                source: PatternSource::Network,
                source_score: m.similarity,
                fetch_time_ms: fetch_time,
            })
            .collect();

        Ok(patterns)
    }

    fn deduplicate(&self, patterns: Vec<DiscoveredPattern>) -> Vec<DiscoveredPattern> {
        use std::collections::HashMap;

        let mut seen: HashMap<String, DiscoveredPattern> = HashMap::new();

        for pattern in patterns {
            let key = pattern.pattern.id.clone();

            if let Some(existing) = seen.get(&key) {
                // Keep the one with higher source score
                if pattern.source_score > existing.source_score {
                    seen.insert(key, pattern);
                }
            } else {
                seen.insert(key, pattern);
            }
        }

        seen.into_values().collect()
    }

    fn rank_patterns(
        &self,
        patterns: Vec<DiscoveredPattern>,
        context: &Context,
    ) -> Vec<RankedPattern> {
        let mut ranked: Vec<RankedPattern> = patterns
            .into_iter()
            .map(|dp| {
                let relevance_score = self.compute_relevance(&dp.pattern, context);

                // Combined score factors:
                // - Relevance to context (40%)
                // - Quality/reputation score (30%)
                // - Source preference (local > network) (15%)
                // - Success rate (15%)

                let source_bonus = match dp.source {
                    PatternSource::Local => 0.15,
                    PatternSource::Network => 0.0,
                    PatternSource::Builtin => 0.1,
                };

                let combined_score = relevance_score * 0.4
                    + (dp.pattern.quality_score as f64) * 0.3
                    + source_bonus
                    + (dp.pattern.success_rate as f64) * 0.15;

                RankedPattern {
                    pattern: dp.pattern,
                    relevance_score,
                    combined_score,
                }
            })
            .collect();

        // Sort by combined score descending
        ranked.sort_by(|a, b| {
            b.combined_score
                .partial_cmp(&a.combined_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        ranked
    }

    fn compute_relevance(&self, pattern: &Pattern, context: &Context) -> f64 {
        let mut score = 0.0;

        // Domain match
        let context_domain = self.infer_domain(context);
        if pattern.domain == context_domain {
            score += 0.4;
        }

        // Keyword overlap between trigger and recent activity
        let trigger_lower = pattern.trigger.to_lowercase();
        let trigger_words: std::collections::HashSet<_> =
            trigger_lower.split_whitespace().collect();

        let context_words: std::collections::HashSet<_> = context
            .recent_files
            .iter()
            .flat_map(|f| {
                f.to_lowercase()
                    .split('/')
                    .map(String::from)
                    .collect::<Vec<_>>()
            })
            .collect();

        let overlap = trigger_words
            .iter()
            .filter(|w| context_words.iter().any(|cw| cw.contains(*w)))
            .count();

        if !trigger_words.is_empty() {
            score += (overlap as f64 / trigger_words.len() as f64) * 0.3;
        }

        // User preference match
        if let Some(pref_domain) = context.user_preferences.get("preferred_domain") {
            if &pattern.domain == pref_domain {
                score += 0.2;
            }
        }

        // Recency bonus for recently successful patterns
        // (Would need access to usage history)
        score += 0.1; // Default

        score
    }

    fn infer_domain(&self, context: &Context) -> String {
        // Infer domain from working directory and recent files
        let wd = context.working_directory.to_lowercase();

        if wd.contains("code") || wd.contains("src") || wd.contains("dev") {
            return "coding".to_string();
        }
        if wd.contains("doc") || wd.contains("writing") {
            return "writing".to_string();
        }
        if wd.contains("data") || wd.contains("analytics") {
            return "analysis".to_string();
        }

        // Check recent files
        for file in &context.recent_files {
            let ext = file.split('.').last().unwrap_or("");
            match ext {
                "py" | "rs" | "js" | "ts" => return "coding".to_string(),
                "md" | "txt" | "doc" => return "writing".to_string(),
                "csv" | "json" | "xlsx" => return "analysis".to_string(),
                _ => {}
            }
        }

        "general".to_string()
    }

    fn compute_context_embedding(&self, _context: &Context) -> Result<Vec<f32>> {
        // Compute embedding for semantic search
        // Simplified - would use actual embedding model

        Ok(vec![0.0; 128])
    }

    fn compute_cache_key(&self, context: &Context) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        context.working_directory.hash(&mut hasher);
        context.recent_files.hash(&mut hasher);

        format!("{:x}", hasher.finish())
    }
}

/// A pattern discovered from any source
struct DiscoveredPattern {
    pattern: Pattern,
    source: PatternSource,
    source_score: f64,
    fetch_time_ms: u64,
}

/// Cache for discovery results
struct DiscoveryCache {
    cache: Arc<RwLock<std::collections::HashMap<String, CacheEntry>>>,
    ttl_secs: u64,
}

struct CacheEntry {
    patterns: Vec<RankedPattern>,
    timestamp: std::time::Instant,
}

impl DiscoveryCache {
    fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(std::collections::HashMap::new())),
            ttl_secs: 300, // 5 minutes
        }
    }

    async fn get(&self, key: &str) -> Option<Vec<RankedPattern>> {
        let cache = self.cache.read().await;

        if let Some(entry) = cache.get(key) {
            if entry.timestamp.elapsed().as_secs() < self.ttl_secs {
                return Some(entry.patterns.clone());
            }
        }

        None
    }

    async fn set(&self, key: &str, patterns: Vec<RankedPattern>) {
        let mut cache = self.cache.write().await;

        cache.insert(
            key.to_string(),
            CacheEntry {
                patterns,
                timestamp: std::time::Instant::now(),
            },
        );

        // Clean old entries
        cache.retain(|_, v| v.timestamp.elapsed().as_secs() < self.ttl_secs * 2);
    }
}
