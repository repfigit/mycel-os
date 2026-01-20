//! Context - Session and user context management
//!
//! The context manager maintains state about:
//! - Current session (what the user is working on)
//! - User preferences and history
//! - System state (files, apps, connections)
//!
//! Memory management:
//! - Sessions are cleaned up after configurable TTL (default: 24 hours)
//! - Call cleanup_stale_sessions() periodically to reclaim memory

use anyhow::Result;
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::config::MycelConfig;

/// Default session TTL in hours
const DEFAULT_SESSION_TTL_HOURS: i64 = 24;

/// Main context manager
#[derive(Clone)]
pub struct ContextManager {
    config: MycelConfig,
    sessions: Arc<RwLock<HashMap<String, SessionContext>>>,
    user_context: Arc<RwLock<UserContext>>,
}

impl ContextManager {
    pub async fn new(config: &MycelConfig) -> Result<Self> {
        // Load user context from disk if it exists
        let user_context = UserContext::load_or_default(&config.context_path).await?;

        Ok(Self {
            config: config.clone(),
            sessions: Arc::new(RwLock::new(HashMap::new())),
            user_context: Arc::new(RwLock::new(user_context)),
        })
    }

    /// Get the context for a session (creates if doesn't exist)
    pub async fn get_context(&self, session_id: &str) -> Result<Context> {
        let mut sessions = self.sessions.write().await;
        let user_ctx = self.user_context.read().await;

        let session = sessions
            .entry(session_id.to_string())
            .or_insert_with(|| SessionContext::new(session_id));

        // Update last accessed time
        session.touch();

        Ok(Context {
            session_id: session_id.to_string(),
            working_directory: session.working_directory.clone(),
            recent_files: session.recent_files.clone(),
            conversation_history: session.conversation_history.clone(),
            timestamp: Utc::now(),
            user_name: user_ctx.name.clone(),
            user_preferences: user_ctx.preferences.clone(),
        })
    }

    /// Update session context after an interaction
    pub async fn update_session(
        &self,
        session_id: &str,
        user_input: &str,
        ai_response: &str,
    ) -> Result<()> {
        let mut sessions = self.sessions.write().await;

        if let Some(session) = sessions.get_mut(session_id) {
            session.touch();
            session.conversation_history.push(ConversationTurn {
                timestamp: Utc::now(),
                user: user_input.to_string(),
                assistant: ai_response.to_string(),
            });

            // Keep only last N turns
            if session.conversation_history.len() > 50 {
                session.conversation_history.remove(0);
            }
        }

        Ok(())
    }

    /// Record that a file was accessed
    pub async fn record_file_access(&self, session_id: &str, file_path: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;

        if let Some(session) = sessions.get_mut(session_id) {
            session.touch();
            // Remove if already present, then add to front
            session.recent_files.retain(|f| f != file_path);
            session.recent_files.insert(0, file_path.to_string());

            // Keep only last 20 files
            session.recent_files.truncate(20);
        }

        Ok(())
    }

    /// Change working directory for a session
    pub async fn set_working_directory(&self, session_id: &str, path: &str) -> Result<()> {
        let mut sessions = self.sessions.write().await;

        if let Some(session) = sessions.get_mut(session_id) {
            session.touch();
            session.working_directory = path.to_string();
        }

        Ok(())
    }

    /// Update user preferences
    pub async fn set_user_preference(&self, key: &str, value: &str) -> Result<()> {
        let mut user_ctx = self.user_context.write().await;
        user_ctx.preferences.insert(key.to_string(), value.to_string());
        user_ctx.save(&self.config.context_path).await?;
        Ok(())
    }

    /// Clean up sessions that haven't been accessed within the TTL
    ///
    /// This prevents unbounded memory growth from accumulated sessions.
    /// Should be called periodically (e.g., every hour).
    pub async fn cleanup_stale_sessions(&self, max_age_hours: Option<i64>) -> usize {
        let ttl = Duration::hours(max_age_hours.unwrap_or(DEFAULT_SESSION_TTL_HOURS));
        let cutoff = Utc::now() - ttl;

        let mut sessions = self.sessions.write().await;
        let before_count = sessions.len();

        sessions.retain(|_id, session| session.last_accessed > cutoff);

        let removed = before_count - sessions.len();
        if removed > 0 {
            info!(
                removed_sessions = removed,
                remaining_sessions = sessions.len(),
                "Cleaned up stale sessions"
            );
        }

        removed
    }

    /// Get the number of active sessions
    pub async fn session_count(&self) -> usize {
        self.sessions.read().await.len()
    }
}

/// The context passed to AI for each interaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Context {
    pub session_id: String,
    pub working_directory: String,
    pub recent_files: Vec<String>,
    pub conversation_history: Vec<ConversationTurn>,
    pub timestamp: DateTime<Utc>,
    pub user_name: Option<String>,
    pub user_preferences: HashMap<String, String>,
}

/// A single conversation turn
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationTurn {
    pub timestamp: DateTime<Utc>,
    pub user: String,
    pub assistant: String,
}

/// Per-session context
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionContext {
    pub id: String,
    pub created_at: DateTime<Utc>,
    pub last_accessed: DateTime<Utc>,
    pub working_directory: String,
    pub recent_files: Vec<String>,
    pub conversation_history: Vec<ConversationTurn>,
    pub metadata: HashMap<String, String>,
}

impl SessionContext {
    pub fn new(id: &str) -> Self {
        let now = Utc::now();
        Self {
            id: id.to_string(),
            created_at: now,
            last_accessed: now,
            working_directory: dirs::home_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| "/home".to_string()),
            recent_files: Vec::new(),
            conversation_history: Vec::new(),
            metadata: HashMap::new(),
        }
    }

    /// Update the last accessed timestamp
    pub fn touch(&mut self) {
        self.last_accessed = Utc::now();
    }
}

/// Persistent user context
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserContext {
    pub name: Option<String>,
    pub preferences: HashMap<String, String>,
    pub learned_patterns: Vec<LearnedPattern>,
    pub frequently_used: Vec<String>,
}

impl UserContext {
    pub async fn load_or_default(path: &str) -> Result<Self> {
        let context_file = format!("{}/user_context.json", path);
        
        if std::path::Path::new(&context_file).exists() {
            let content = tokio::fs::read_to_string(&context_file).await?;
            Ok(serde_json::from_str(&content)?)
        } else {
            Ok(Self::default())
        }
    }

    pub async fn save(&self, path: &str) -> Result<()> {
        tokio::fs::create_dir_all(path).await?;
        let context_file = format!("{}/user_context.json", path);
        let content = serde_json::to_string_pretty(self)?;
        tokio::fs::write(&context_file, content).await?;
        Ok(())
    }
}

/// A pattern learned from user behavior
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LearnedPattern {
    pub trigger: String,
    pub action: String,
    pub confidence: f32,
    pub times_used: u32,
}
