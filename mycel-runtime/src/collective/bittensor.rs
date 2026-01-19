//! Bittensor Integration
//!
//! Handles communication with Bittensor network for:
//! - Distributed inference
//! - Pattern evaluation
//! - Federated learning
//! - TAO rewards

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use super::patterns::{Pattern, PatternId};

/// Bittensor client for Mycel OS
#[derive(Clone)]
pub struct BittensorClient {
    config: BittensorConfig,
    http_client: reqwest::Client,
    wallet: Option<BittensorWallet>,
}

impl BittensorClient {
    pub async fn new(config: &BittensorConfig) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(60))
            .build()?;
        
        // Load wallet if configured
        let wallet = if let Some(ref wallet_path) = config.wallet_path {
            Some(BittensorWallet::load(wallet_path)?)
        } else {
            None
        };
        
        let client = Self {
            config: config.clone(),
            http_client,
            wallet,
        };
        
        if config.verify_on_start {
            client.verify_connection().await?;
        }
        
        Ok(client)
    }
    
    async fn verify_connection(&self) -> Result<()> {
        // Check connection to Bittensor network
        let metagraph = self.get_metagraph().await?;
        info!(
            "Connected to Bittensor subnet {} with {} neurons",
            self.config.subnet_uid,
            metagraph.neurons.len()
        );
        Ok(())
    }
    
    /// Get the subnet metagraph
    pub async fn get_metagraph(&self) -> Result<Metagraph> {
        let response = self.http_client
            .get(format!(
                "{}/metagraph/{}",
                self.config.api_url,
                self.config.subnet_uid
            ))
            .send()
            .await?
            .json::<Metagraph>()
            .await?;
        
        Ok(response)
    }
    
    /// Submit a pattern for evaluation by the network
    pub async fn submit_pattern_for_evaluation(
        &self,
        pattern_id: &PatternId,
        pattern: &Pattern,
    ) -> Result<EvaluationResult> {
        debug!("Submitting pattern {} for evaluation", pattern_id);
        
        // Create evaluation request
        let request = EvaluationRequest {
            pattern_id: pattern_id.clone(),
            pattern_trigger: pattern.trigger.clone(),
            pattern_solution: pattern.solution_summary(),
            domain: pattern.domain.clone(),
        };
        
        // Query miners for evaluation
        let responses = self.query_miners(
            MycelSynapse::EvaluatePattern(request),
            self.config.evaluation_timeout_secs,
        ).await?;
        
        // Aggregate evaluations
        let result = self.aggregate_evaluations(responses)?;
        
        info!(
            "Pattern {} evaluated: score={:.2}, confidence={:.2}",
            pattern_id, result.score, result.confidence
        );
        
        Ok(result)
    }
    
    /// Query the network for inference (when local + cloud isn't enough)
    pub async fn distributed_inference(
        &self,
        query: &str,
        context: &str,
        patterns: Vec<PatternId>,
    ) -> Result<InferenceResult> {
        debug!("Requesting distributed inference");
        
        let request = InferenceRequest {
            query: query.to_string(),
            context: context.to_string(),
            available_patterns: patterns,
            max_tokens: self.config.max_inference_tokens,
        };
        
        let responses = self.query_miners(
            MycelSynapse::Inference(request),
            self.config.inference_timeout_secs,
        ).await?;
        
        // Select best response based on quality scoring
        let best = self.select_best_response(responses)?;
        
        Ok(best)
    }
    
    /// Perform semantic search for patterns across the network
    pub async fn semantic_pattern_search(
        &self,
        query_embedding: Vec<f32>,
        k: usize,
    ) -> Result<Vec<SemanticMatch>> {
        let request = SemanticSearchRequest {
            embedding: query_embedding,
            k,
            min_similarity: self.config.min_semantic_similarity,
        };
        
        let responses = self.query_miners(
            MycelSynapse::SemanticSearch(request),
            self.config.search_timeout_secs,
        ).await?;
        
        // Merge and deduplicate results
        let matches = self.merge_semantic_results(responses, k)?;
        
        Ok(matches)
    }
    
    /// Report pattern outcome for reputation/training
    pub async fn report_pattern_outcome(
        &self,
        pattern_id: &PatternId,
        success: bool,
    ) -> Result<()> {
        // This contributes to the collective training signal
        let report = OutcomeReport {
            pattern_id: pattern_id.clone(),
            success,
            timestamp: chrono::Utc::now(),
        };
        
        // Broadcast to subnet validators
        self.broadcast_to_validators(
            ValidatorMessage::PatternOutcome(report),
        ).await?;
        
        Ok(())
    }
    
    /// Submit gradients for federated learning
    pub async fn submit_gradients(&self, gradients: &PrivateGradients) -> Result<()> {
        debug!("Submitting gradients for federated learning");
        
        let submission = GradientSubmission {
            model_id: gradients.model_id.clone(),
            gradient_hash: gradients.hash.clone(),
            compressed_gradients: gradients.compressed.clone(),
            sample_count: gradients.sample_count,
            privacy_budget_used: gradients.epsilon,
        };
        
        self.broadcast_to_validators(
            ValidatorMessage::GradientSubmission(submission),
        ).await?;
        
        info!("Gradients submitted for model {}", gradients.model_id);
        Ok(())
    }
    
    /// Get current model weights from the network
    pub async fn get_model_weights(&self, model_id: &str) -> Result<ModelWeights> {
        let response = self.http_client
            .get(format!(
                "{}/models/{}/weights",
                self.config.api_url,
                model_id
            ))
            .send()
            .await?
            .json::<ModelWeights>()
            .await?;
        
        Ok(response)
    }
    
    /// Get TAO balance
    pub async fn get_balance(&self) -> Result<f64> {
        let wallet = self.wallet.as_ref()
            .ok_or_else(|| anyhow!("Wallet not configured"))?;
        
        let response = self.http_client
            .get(format!(
                "{}/balance/{}",
                self.config.api_url,
                wallet.hotkey
            ))
            .send()
            .await?
            .json::<BalanceResponse>()
            .await?;
        
        Ok(response.balance)
    }
    
    /// Get rewards earned
    pub async fn get_rewards(&self) -> Result<RewardsSummary> {
        let wallet = self.wallet.as_ref()
            .ok_or_else(|| anyhow!("Wallet not configured"))?;
        
        let response = self.http_client
            .get(format!(
                "{}/rewards/{}/{}",
                self.config.api_url,
                self.config.subnet_uid,
                wallet.hotkey
            ))
            .send()
            .await?
            .json::<RewardsSummary>()
            .await?;
        
        Ok(response)
    }
    
    // Helper methods
    
    async fn query_miners(
        &self,
        synapse: MycelSynapse,
        timeout_secs: u64,
    ) -> Result<Vec<MinerResponse>> {
        let metagraph = self.get_metagraph().await?;
        
        // Select top miners by stake/incentive
        let top_miners: Vec<_> = metagraph.neurons
            .iter()
            .filter(|n| n.is_active && n.axon_info.is_some())
            .take(self.config.max_miners_to_query)
            .collect();
        
        if top_miners.is_empty() {
            return Err(anyhow!("No active miners available"));
        }
        
        // Query miners in parallel
        let mut responses = Vec::new();
        let client = self.http_client.clone();
        
        for neuron in top_miners {
            let axon = neuron.axon_info.as_ref().unwrap();
            let url = format!("http://{}:{}/forward", axon.ip, axon.port);
            
            let request = MinerRequest {
                synapse: synapse.clone(),
                timeout: timeout_secs,
            };
            
            match client
                .post(&url)
                .timeout(std::time::Duration::from_secs(timeout_secs))
                .json(&request)
                .send()
                .await
            {
                Ok(resp) => {
                    if let Ok(miner_resp) = resp.json::<MinerResponse>().await {
                        responses.push(miner_resp);
                    }
                }
                Err(e) => {
                    debug!("Miner {} failed: {}", neuron.uid, e);
                }
            }
        }
        
        if responses.is_empty() {
            return Err(anyhow!("All miners failed to respond"));
        }
        
        Ok(responses)
    }
    
    async fn broadcast_to_validators(&self, message: ValidatorMessage) -> Result<()> {
        let metagraph = self.get_metagraph().await?;
        
        // Find validators
        let validators: Vec<_> = metagraph.neurons
            .iter()
            .filter(|n| n.is_validator && n.axon_info.is_some())
            .collect();
        
        for validator in validators {
            let axon = validator.axon_info.as_ref().unwrap();
            let url = format!("http://{}:{}/validator", axon.ip, axon.port);
            
            // Fire and forget
            let _ = self.http_client
                .post(&url)
                .json(&message)
                .send()
                .await;
        }
        
        Ok(())
    }
    
    fn aggregate_evaluations(&self, responses: Vec<MinerResponse>) -> Result<EvaluationResult> {
        let mut total_score = 0.0;
        let mut total_weight = 0.0;
        
        for resp in &responses {
            if let MycelSynapseResult::Evaluation(eval) = &resp.result {
                let weight = resp.stake_weight;
                total_score += eval.score * weight;
                total_weight += weight;
            }
        }
        
        if total_weight == 0.0 {
            return Err(anyhow!("No valid evaluations"));
        }
        
        Ok(EvaluationResult {
            score: total_score / total_weight,
            confidence: (responses.len() as f64 / self.config.max_miners_to_query as f64).min(1.0),
            evaluator_count: responses.len(),
        })
    }
    
    fn select_best_response(&self, responses: Vec<MinerResponse>) -> Result<InferenceResult> {
        let mut best: Option<(f64, InferenceResult)> = None;
        
        for resp in responses {
            if let MycelSynapseResult::Inference(result) = resp.result {
                let score = result.quality_score * resp.stake_weight;
                
                if best.is_none() || score > best.as_ref().unwrap().0 {
                    best = Some((score, result));
                }
            }
        }
        
        best.map(|(_, r)| r)
            .ok_or_else(|| anyhow!("No valid inference results"))
    }
    
    fn merge_semantic_results(
        &self,
        responses: Vec<MinerResponse>,
        k: usize,
    ) -> Result<Vec<SemanticMatch>> {
        let mut all_matches: Vec<SemanticMatch> = Vec::new();
        
        for resp in responses {
            if let MycelSynapseResult::SemanticSearch(matches) = resp.result {
                all_matches.extend(matches);
            }
        }
        
        // Sort by similarity and deduplicate
        all_matches.sort_by(|a, b| b.similarity.partial_cmp(&a.similarity).unwrap());
        all_matches.dedup_by(|a, b| a.pattern_id == b.pattern_id);
        all_matches.truncate(k);
        
        Ok(all_matches)
    }
}

/// Bittensor configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BittensorConfig {
    /// Subnet UID for Mycel
    pub subnet_uid: u16,
    
    /// API endpoint URL
    pub api_url: String,
    
    /// Wallet path
    pub wallet_path: Option<String>,
    
    /// Maximum miners to query
    pub max_miners_to_query: usize,
    
    /// Timeouts
    pub evaluation_timeout_secs: u64,
    pub inference_timeout_secs: u64,
    pub search_timeout_secs: u64,
    
    /// Inference settings
    pub max_inference_tokens: u32,
    pub min_semantic_similarity: f32,
    
    /// Verify connection on start
    pub verify_on_start: bool,
}

impl Default for BittensorConfig {
    fn default() -> Self {
        Self {
            subnet_uid: 32,  // Hypothetical Clay subnet
            api_url: "https://api.bittensor.com".to_string(),
            wallet_path: None,
            max_miners_to_query: 10,
            evaluation_timeout_secs: 30,
            inference_timeout_secs: 60,
            search_timeout_secs: 10,
            max_inference_tokens: 2048,
            min_semantic_similarity: 0.7,
            verify_on_start: true,
        }
    }
}

/// Bittensor wallet
#[derive(Debug, Clone)]
pub struct BittensorWallet {
    pub name: String,
    pub hotkey: String,
    pub coldkey: String,
}

impl BittensorWallet {
    pub fn load(path: &str) -> Result<Self> {
        // In production, load from encrypted wallet file
        warn!("Wallet loading not fully implemented");
        Ok(Self {
            name: "default".to_string(),
            hotkey: "mock_hotkey".to_string(),
            coldkey: "mock_coldkey".to_string(),
        })
    }
}

// Network data structures

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metagraph {
    pub neurons: Vec<Neuron>,
    pub block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Neuron {
    pub uid: u16,
    pub hotkey: String,
    pub stake: f64,
    pub incentive: f64,
    pub is_active: bool,
    pub is_validator: bool,
    pub axon_info: Option<AxonInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AxonInfo {
    pub ip: String,
    pub port: u16,
}

// Synapse types

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MycelSynapse {
    EvaluatePattern(EvaluationRequest),
    Inference(InferenceRequest),
    SemanticSearch(SemanticSearchRequest),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationRequest {
    pub pattern_id: PatternId,
    pub pattern_trigger: String,
    pub pattern_solution: String,
    pub domain: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub query: String,
    pub context: String,
    pub available_patterns: Vec<PatternId>,
    pub max_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticSearchRequest {
    pub embedding: Vec<f32>,
    pub k: usize,
    pub min_similarity: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerRequest {
    pub synapse: MycelSynapse,
    pub timeout: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MinerResponse {
    pub result: MycelSynapseResult,
    pub process_time: f64,
    pub stake_weight: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MycelSynapseResult {
    Evaluation(EvaluationResponse),
    Inference(InferenceResult),
    SemanticSearch(Vec<SemanticMatch>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResponse {
    pub score: f64,
    pub relevance: f64,
    pub quality: f64,
    pub safety: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationResult {
    pub score: f64,
    pub confidence: f64,
    pub evaluator_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceResult {
    pub response: String,
    pub quality_score: f64,
    pub patterns_used: Vec<PatternId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMatch {
    pub pattern_id: PatternId,
    pub similarity: f64,
    pub metadata_cid: String,
}

// Validator messages

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ValidatorMessage {
    PatternOutcome(OutcomeReport),
    GradientSubmission(GradientSubmission),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeReport {
    pub pattern_id: PatternId,
    pub success: bool,
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GradientSubmission {
    pub model_id: String,
    pub gradient_hash: String,
    pub compressed_gradients: Vec<u8>,
    pub sample_count: usize,
    pub privacy_budget_used: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrivateGradients {
    pub model_id: String,
    pub hash: String,
    pub compressed: Vec<u8>,
    pub sample_count: usize,
    pub epsilon: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelWeights {
    pub model_id: String,
    pub version: u64,
    pub weights_cid: String,
    pub updated_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceResponse {
    pub balance: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewardsSummary {
    pub total_earned: f64,
    pub last_day: f64,
    pub last_week: f64,
    pub rank: u32,
}
