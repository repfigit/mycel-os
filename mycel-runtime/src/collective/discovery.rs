//! Discovery - Find relevant patterns from local cache.
//!
//! Provides local pattern discovery and ranking against the current context.
#![allow(dead_code)]

use anyhow::Result;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

use super::patterns::{Pattern, PatternSource, PatternStore, RankedPattern};
use crate::context::Context;

/// Pattern discovery across local sources.
pub struct PatternDiscovery {
    local_store: Arc<RwLock<PatternStore>>,
    cache: DiscoveryCache,
}

impl PatternDiscovery {
    pub fn new(local_store: Arc<RwLock<PatternStore>>) -> Self {
        Self {
            local_store,
            cache: DiscoveryCache::new(),
        }
    }

    /// Discover patterns relevant to the current context.
    pub async fn discover(&self, context: &Context) -> Result<Vec<RankedPattern>> {
        debug!("Discovering patterns for context");

        let cache_key = self.compute_cache_key(context);
        if let Some(cached) = self.cache.get(&cache_key).await {
            debug!("Cache hit for pattern discovery");
            return Ok(cached);
        }

        let local_patterns = self.search_local(context).await?;
        let ranked = self.rank_patterns(local_patterns, context);
        self.cache.set(&cache_key, ranked.clone()).await;

        Ok(ranked)
    }

    async fn search_local(&self, context: &Context) -> Result<Vec<DiscoveredPattern>> {
        let store = self.local_store.read().await;
        let domain = self.infer_domain(context);
        let patterns = store.search(Some(&domain), "");

        Ok(patterns
            .into_iter()
            .map(|p| DiscoveredPattern {
                pattern: p.clone(),
                source: PatternSource::Local,
            })
            .collect())
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
                let source_bonus = match dp.source {
                    PatternSource::Local => 0.15,
                    PatternSource::Network => 0.0,
                    PatternSource::Builtin => 0.1,
                };
                let combined_score = relevance_score * 0.4
                    + f64::from(dp.pattern.quality_score) * 0.3
                    + source_bonus
                    + f64::from(dp.pattern.success_rate) * 0.15;

                RankedPattern {
                    pattern: dp.pattern,
                    relevance_score,
                    combined_score,
                }
            })
            .collect();

        ranked.sort_by(|a, b| {
            b.combined_score
                .partial_cmp(&a.combined_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        ranked
    }

    fn compute_relevance(&self, pattern: &Pattern, context: &Context) -> f64 {
        let mut score = 0.0;

        let context_domain = self.infer_domain(context);
        if pattern.domain == context_domain {
            score += 0.4;
        }

        let trigger_lower = pattern.trigger.to_lowercase();
        let trigger_words: HashSet<_> = trigger_lower.split_whitespace().collect();

        let context_words: HashSet<String> = context
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
            .filter(|w| context_words.iter().any(|cw| cw.contains(**w)))
            .count();

        if !trigger_words.is_empty() {
            score += (overlap as f64 / trigger_words.len() as f64) * 0.3;
        }

        if let Some(pref_domain) = context.user_preferences.get("preferred_domain") {
            if &pattern.domain == pref_domain {
                score += 0.2;
            }
        }

        score += 0.1;
        score
    }

    fn infer_domain(&self, context: &Context) -> String {
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

        for file in &context.recent_files {
            let ext = file.rsplit('.').next().unwrap_or("");
            match ext {
                "py" | "rs" | "js" | "ts" => return "coding".to_string(),
                "md" | "txt" | "doc" => return "writing".to_string(),
                "csv" | "json" | "xlsx" => return "analysis".to_string(),
                _ => {}
            }
        }

        "general".to_string()
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

struct DiscoveredPattern {
    pattern: Pattern,
    source: PatternSource,
}

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
            ttl_secs: 300,
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
        cache.retain(|_, v| v.timestamp.elapsed().as_secs() < self.ttl_secs * 2);
    }
}
