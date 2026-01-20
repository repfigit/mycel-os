//! Patterns - Learned patterns that can be shared across Clay instances
//!
//! A pattern is a reusable solution to a class of problems, extracted
//! from successful interactions.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;

use crate::context::Context;

/// Unique identifier for a pattern
pub type PatternId = String;

/// A learned pattern that can be shared
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pattern {
    /// Unique identifier
    pub id: PatternId,

    /// What triggers this pattern (natural language description)
    pub trigger: String,

    /// Context requirements for the pattern to apply
    pub context_requirements: Vec<String>,

    /// The actual solution
    pub solution: PatternSolution,

    /// Domain/category
    pub domain: String,

    /// Human-readable description
    pub description: String,

    /// Quality metrics
    pub quality_score: f32,
    pub success_rate: f32,
    pub usage_count: u64,

    /// Economics
    pub suggested_price: Option<u128>,

    /// Source
    pub source: PatternSource,
    pub creator: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Pattern {
    /// Create a new pattern
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

    /// Apply this pattern to a context
    pub async fn apply(&self, context: &Context) -> Result<PatternResult> {
        match &self.solution {
            PatternSolution::PromptTemplate {
                template,
                variables,
            } => {
                let filled = self.fill_template(template, variables, context)?;
                Ok(PatternResult::Prompt(filled))
            }
            PatternSolution::CodeTemplate { language, code, .. } => {
                let filled = self.fill_code_template(code, context)?;
                Ok(PatternResult::Code {
                    language: language.clone(),
                    code: filled,
                })
            }
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

    /// Get a summary of the solution (for sharing)
    pub fn solution_summary(&self) -> String {
        match &self.solution {
            PatternSolution::PromptTemplate { template, .. } => {
                format!(
                    "Prompt template: {}...",
                    &template[..template.len().min(100)]
                )
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

    fn fill_template(
        &self,
        template: &str,
        variables: &[String],
        context: &Context,
    ) -> Result<String> {
        let mut result = template.to_string();

        for var in variables {
            let value = self.get_context_value(var, context);
            result = result.replace(&format!("{{{{{}}}}}", var), &value);
        }

        Ok(result)
    }

    fn fill_code_template(&self, code: &str, context: &Context) -> Result<String> {
        // Simple variable substitution
        let mut result = code.to_string();
        result = result.replace("{{working_dir}}", &context.working_directory);
        Ok(result)
    }

    fn get_context_value(&self, var: &str, context: &Context) -> String {
        match var {
            "working_directory" => context.working_directory.clone(),
            "user_name" => context.user_name.clone().unwrap_or_default(),
            "timestamp" => context.timestamp.to_rfc3339(),
            _ => format!("{{{{{}}}}}", var), // Leave unfilled
        }
    }
}

/// The actual solution in a pattern
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PatternSolution {
    /// A prompt template that works well
    PromptTemplate {
        template: String,
        variables: Vec<String>,
    },

    /// Generated code that solves a class of problems
    CodeTemplate {
        language: String,
        code: String,
        dependencies: Vec<String>,
    },

    /// A multi-step workflow
    Workflow { steps: Vec<WorkflowStep> },

    /// A fine-tuned LoRA adapter
    ModelAdapter {
        base_model: String,
        adapter_cid: String,
        adapter_hash: String,
    },
}

/// A step in a workflow pattern
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    pub name: String,
    pub action: String,
    pub inputs: Vec<String>,
    pub outputs: Vec<String>,
}

/// Result of applying a pattern
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

/// Where a pattern came from
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PatternSource {
    Local,
    Network,
    Builtin,
}

/// A pattern with ranking information
#[derive(Debug, Clone)]
pub struct RankedPattern {
    pub pattern: Pattern,
    pub relevance_score: f64,
    pub combined_score: f64,
}

/// Local storage for patterns
pub struct PatternStore {
    patterns: HashMap<PatternId, Pattern>,
    usage_stats: HashMap<PatternId, UsageStats>,
    store_path: String,

    // Aggregated stats
    network_patterns_used: usize,
    patterns_shared: usize,
    total_earnings: u128,
}

impl PatternStore {
    /// Load or create a pattern store
    pub async fn load_or_create(path: &str) -> Result<Self> {
        let store_path = Path::new(path);

        if store_path.join("patterns.json").exists() {
            Self::load(path).await
        } else {
            Ok(Self::new(path))
        }
    }

    fn new(path: &str) -> Self {
        Self {
            patterns: HashMap::new(),
            usage_stats: HashMap::new(),
            store_path: path.to_string(),
            network_patterns_used: 0,
            patterns_shared: 0,
            total_earnings: 0,
        }
    }

    async fn load(path: &str) -> Result<Self> {
        let patterns_file = Path::new(path).join("patterns.json");
        let content = tokio::fs::read_to_string(&patterns_file).await?;
        let patterns: HashMap<PatternId, Pattern> = serde_json::from_str(&content)?;

        let stats_file = Path::new(path).join("stats.json");
        let usage_stats = if stats_file.exists() {
            let content = tokio::fs::read_to_string(&stats_file).await?;
            serde_json::from_str(&content)?
        } else {
            HashMap::new()
        };

        Ok(Self {
            patterns,
            usage_stats,
            store_path: path.to_string(),
            network_patterns_used: 0,
            patterns_shared: 0,
            total_earnings: 0,
        })
    }

    /// Save the pattern store
    pub async fn save(&self) -> Result<()> {
        tokio::fs::create_dir_all(&self.store_path).await?;

        let patterns_file = Path::new(&self.store_path).join("patterns.json");
        let content = serde_json::to_string_pretty(&self.patterns)?;
        tokio::fs::write(&patterns_file, content).await?;

        let stats_file = Path::new(&self.store_path).join("stats.json");
        let content = serde_json::to_string_pretty(&self.usage_stats)?;
        tokio::fs::write(&stats_file, content).await?;

        Ok(())
    }

    /// Add a pattern
    pub async fn add_pattern(&mut self, pattern: Pattern) -> Result<()> {
        self.patterns.insert(pattern.id.clone(), pattern);
        self.save().await
    }

    /// Get a pattern by ID
    pub fn get(&self, id: &PatternId) -> Option<&Pattern> {
        self.patterns.get(id)
    }

    /// Search patterns by domain and trigger
    pub fn search(&self, domain: Option<&str>, query: &str) -> Vec<&Pattern> {
        self.patterns
            .values()
            .filter(|p| {
                let domain_match = domain.map_or(true, |d| p.domain == d);
                let query_match = p.trigger.to_lowercase().contains(&query.to_lowercase())
                    || p.description.to_lowercase().contains(&query.to_lowercase());
                domain_match && query_match
            })
            .collect()
    }

    /// Record pattern usage
    pub fn record_usage(&mut self, pattern_id: &PatternId) {
        let stats = self
            .usage_stats
            .entry(pattern_id.clone())
            .or_insert(UsageStats::default());
        stats.usage_count += 1;
        stats.last_used = Some(chrono::Utc::now());

        if let Some(pattern) = self.patterns.get(pattern_id) {
            if pattern.source == PatternSource::Network {
                self.network_patterns_used += 1;
            }
        }
    }

    /// Record outcome
    pub fn record_outcome(&mut self, pattern_id: &PatternId, success: bool, rating: u8) {
        let stats = self
            .usage_stats
            .entry(pattern_id.clone())
            .or_insert(UsageStats::default());

        if success {
            stats.success_count += 1;
        } else {
            stats.failure_count += 1;
        }

        stats.total_rating += rating as u64;
        stats.rating_count += 1;
    }

    /// Get recent successful interactions (for federated learning)
    pub fn get_recent_successful_interactions(&self, limit: usize) -> Vec<&Pattern> {
        let mut patterns: Vec<_> = self.patterns.values().collect();

        patterns.sort_by(|a, b| {
            let a_stats = self.usage_stats.get(&a.id);
            let b_stats = self.usage_stats.get(&b.id);

            let a_time = a_stats.and_then(|s| s.last_used).unwrap_or(a.created_at);
            let b_time = b_stats.and_then(|s| s.last_used).unwrap_or(b.created_at);

            b_time.cmp(&a_time)
        });

        patterns.into_iter().take(limit).collect()
    }

    pub fn pattern_count(&self) -> usize {
        self.patterns.len()
    }

    pub fn network_patterns_used(&self) -> usize {
        self.network_patterns_used
    }

    pub fn patterns_shared(&self) -> usize {
        self.patterns_shared
    }

    pub fn total_earnings(&self) -> u128 {
        self.total_earnings
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageStats {
    pub usage_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub total_rating: u64,
    pub rating_count: u64,
    pub last_used: Option<chrono::DateTime<chrono::Utc>>,
}
