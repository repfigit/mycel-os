//! Collective Intelligence Module
//!
//! Integrates Clay OS with decentralized networks (NEAR Protocol + Bittensor)
//! to enable instances to learn from each other.
//!
//! Note: This module is scaffolded - blockchain integration is deferred.
#![allow(dead_code)]

pub mod bittensor;
pub mod discovery;
pub mod near;
pub mod patterns;
pub mod privacy;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::MycelConfig;
use crate::context::Context;

/// Main collective intelligence coordinator
pub struct CollectiveIntelligence {
    config: CollectiveConfig,
    near_client: Option<near::NearClient>,
    bittensor_client: Option<bittensor::BittensorClient>,
    pattern_store: Arc<RwLock<patterns::PatternStore>>,
    discovery: discovery::PatternDiscovery,
}

impl CollectiveIntelligence {
    pub async fn new(config: &MycelConfig) -> Result<Self> {
        let collective_config = CollectiveConfig::from_mycel_config(config);

        // Initialize NEAR client if configured
        let near_client = if collective_config.near_enabled {
            Some(near::NearClient::new(&collective_config.near_config).await?)
        } else {
            None
        };

        // Initialize Bittensor client if configured
        let bittensor_client = if collective_config.bittensor_enabled {
            Some(bittensor::BittensorClient::new(&collective_config.bittensor_config).await?)
        } else {
            None
        };

        // Initialize local pattern store
        let pattern_store = Arc::new(RwLock::new(
            patterns::PatternStore::load_or_create(&collective_config.pattern_store_path).await?,
        ));

        // Initialize discovery system
        let discovery = discovery::PatternDiscovery::new(
            near_client.clone(),
            bittensor_client.clone(),
            Arc::clone(&pattern_store),
        );

        Ok(Self {
            config: collective_config,
            near_client,
            bittensor_client,
            pattern_store,
            discovery,
        })
    }

    /// Find patterns relevant to the current context
    pub async fn find_patterns(&self, context: &Context) -> Result<Vec<patterns::RankedPattern>> {
        self.discovery.discover(context).await
    }

    /// Apply a pattern to the current context
    pub async fn apply_pattern(
        &self,
        pattern: &patterns::Pattern,
        context: &Context,
    ) -> Result<patterns::PatternResult> {
        // Record usage attempt
        let mut store = self.pattern_store.write().await;
        store.record_usage(&pattern.id);

        // If pattern is from network, handle payment
        if let Some(ref near) = self.near_client {
            if pattern.source == patterns::PatternSource::Network {
                near.use_pattern(&pattern.id, pattern.suggested_price.unwrap_or(0))
                    .await?;
            }
        }

        // Apply the pattern
        let result = pattern.apply(context).await?;

        Ok(result)
    }

    /// Learn from a successful interaction
    pub async fn learn_from_interaction(
        &self,
        interaction: &Interaction,
        _context: &Context,
    ) -> Result<Option<patterns::Pattern>> {
        // Extract generalizable pattern
        let maybe_pattern =
            privacy::extract_shareable_pattern(interaction, &self.config.privacy_config)?;

        if let Some(pattern) = maybe_pattern {
            // Store locally
            {
                let mut store = self.pattern_store.write().await;
                store.add_pattern(pattern.clone()).await?;
            }

            // Optionally share to network
            if self.config.auto_share_patterns
                && pattern.quality_score >= self.config.min_share_quality
            {
                self.share_pattern(&pattern).await?;
            }

            Ok(Some(pattern))
        } else {
            Ok(None)
        }
    }

    /// Share a pattern to the network
    pub async fn share_pattern(&self, pattern: &patterns::Pattern) -> Result<patterns::PatternId> {
        // Validate pattern before sharing
        privacy::validate_for_sharing(pattern, &self.config.privacy_config)?;

        // Register on NEAR
        let pattern_id = if let Some(ref near) = self.near_client {
            near.register_pattern(pattern).await?
        } else {
            pattern.id.clone()
        };

        // Submit to Bittensor for evaluation
        if let Some(ref bt) = self.bittensor_client {
            bt.submit_pattern_for_evaluation(&pattern_id, pattern)
                .await?;
        }

        Ok(pattern_id)
    }

    /// Report pattern success/failure for reputation
    pub async fn report_pattern_outcome(
        &self,
        pattern_id: &patterns::PatternId,
        success: bool,
        rating: u8,
    ) -> Result<()> {
        // Update local stats
        {
            let mut store = self.pattern_store.write().await;
            store.record_outcome(pattern_id, success, rating);
        }

        // Report to NEAR
        if let Some(ref near) = self.near_client {
            near.rate_pattern(pattern_id, success, rating).await?;
        }

        // Report to Bittensor
        if let Some(ref bt) = self.bittensor_client {
            bt.report_pattern_outcome(pattern_id, success).await?;
        }

        Ok(())
    }

    /// Contribute to federated learning
    pub async fn contribute_to_collective_learning(&self) -> Result<()> {
        if !self.config.federated_learning_enabled {
            return Ok(());
        }

        let store = self.pattern_store.read().await;
        let interactions = store.get_recent_successful_interactions(100);

        if interactions.is_empty() {
            return Ok(());
        }

        // Compute private gradients
        let gradients =
            privacy::compute_private_gradients(&interactions, &self.config.privacy_config)?;

        // Submit to Bittensor
        if let Some(ref bt) = self.bittensor_client {
            bt.submit_gradients(&gradients).await?;
        }

        Ok(())
    }

    /// Get collective intelligence stats
    pub async fn get_stats(&self) -> CollectiveStats {
        let store = self.pattern_store.read().await;

        CollectiveStats {
            local_patterns: store.pattern_count(),
            network_patterns_used: store.network_patterns_used(),
            patterns_shared: store.patterns_shared(),
            total_earnings: store.total_earnings(),
            reputation_score: self.get_reputation().await.unwrap_or(0.0),
        }
    }

    async fn get_reputation(&self) -> Result<f64> {
        if let Some(ref near) = self.near_client {
            near.get_reputation().await
        } else {
            Ok(0.0)
        }
    }
}

/// Configuration for collective intelligence
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveConfig {
    pub near_enabled: bool,
    pub near_config: near::NearConfig,

    pub bittensor_enabled: bool,
    pub bittensor_config: bittensor::BittensorConfig,

    pub pattern_store_path: String,
    pub auto_share_patterns: bool,
    pub min_share_quality: f32,

    pub federated_learning_enabled: bool,
    pub privacy_config: privacy::PrivacyConfig,
}

impl CollectiveConfig {
    pub fn from_mycel_config(_config: &MycelConfig) -> Self {
        // Extract collective config from main config
        // For now, use defaults
        Self::default()
    }
}

impl Default for CollectiveConfig {
    fn default() -> Self {
        Self {
            near_enabled: false,
            near_config: near::NearConfig::default(),
            bittensor_enabled: false,
            bittensor_config: bittensor::BittensorConfig::default(),
            pattern_store_path: "./patterns".to_string(),
            auto_share_patterns: false,
            min_share_quality: 0.8,
            federated_learning_enabled: false,
            privacy_config: privacy::PrivacyConfig::default(),
        }
    }
}

/// An interaction that can be learned from
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

/// Stats about collective intelligence participation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveStats {
    pub local_patterns: usize,
    pub network_patterns_used: usize,
    pub patterns_shared: usize,
    pub total_earnings: u128,
    pub reputation_score: f64,
}
