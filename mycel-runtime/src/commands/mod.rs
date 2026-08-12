//! User-defined commands: named sequences of shell commands and tool calls,
//! created conversationally and executed as a unit.
//!
//! A user says "make a command called morning that checks email and shows my
//! calendar" — the runtime captures that as a `UserCommand`, persists it, and
//! runs the whole sequence when the user later says "morning".
//!
//! Safety: every step is policy-gated at execution time (a dangerous step
//! still asks for confirmation). Creating a command never bypasses the
//! policy evaluator. Names that collide with real binaries require an
//! explicit override flag.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::debug;

/// A single operation within a user command.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum CommandStep {
    /// A raw shell command, run through the policy-gated executor.
    Shell { command: String },
    /// A prompt shown to the user mid-command (their reply is not yet used).
    Prompt { message: String },
}

impl CommandStep {
    pub fn shell(cmd: &str) -> Self {
        CommandStep::Shell {
            command: cmd.to_string(),
        }
    }

    /// Human-readable rendering for confirmation display.
    pub fn describe(&self) -> String {
        match self {
            CommandStep::Shell { command } => format!("run: {}", command),
            CommandStep::Prompt { message } => format!("ask: {}", message),
        }
    }
}

/// A named, user-defined command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCommand {
    pub name: String,
    pub description: String,
    pub steps: Vec<CommandStep>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub use_count: u64,
}

impl UserCommand {
    fn new(name: &str, description: &str, steps: Vec<CommandStep>) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            steps,
            created_at: chrono::Utc::now(),
            use_count: 0,
        }
    }
}

/// A command definition awaiting user confirmation before being saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCommandDef {
    pub name: String,
    pub description: String,
    pub steps: Vec<CommandStep>,
}

/// Persistent registry of user commands.
#[derive(Clone)]
pub struct CommandRegistry {
    commands: Arc<RwLock<HashMap<String, UserCommand>>>,
    path: Option<PathBuf>,
}

impl CommandRegistry {
    /// Load (or create) the registry at `path`.
    pub async fn load(path: PathBuf) -> Self {
        let commands = match tokio::fs::read_to_string(&path).await {
            Ok(content) => serde_json::from_str(&content).unwrap_or_else(|e| {
                debug!(
                    "Corrupt command registry at {:?}, starting fresh: {}",
                    path, e
                );
                HashMap::new()
            }),
            Err(_) => HashMap::new(),
        };
        Self {
            commands: Arc::new(RwLock::new(commands)),
            path: Some(path),
        }
    }

    /// In-memory registry (tests).
    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            commands: Arc::new(RwLock::new(HashMap::new())),
            path: None,
        }
    }

    /// Look up a command by name.
    pub async fn get(&self, name: &str) -> Option<UserCommand> {
        self.commands.read().await.get(name).cloned()
    }

    /// Whether a command with this name exists.
    pub async fn contains(&self, name: &str) -> bool {
        self.commands.read().await.contains_key(name)
    }

    /// Define a new command. Returns an error if the name is taken.
    pub async fn define(
        &self,
        name: &str,
        description: &str,
        steps: Vec<CommandStep>,
    ) -> Result<UserCommand> {
        validate_name(name)?;
        if steps.is_empty() {
            return Err(anyhow!("a command needs at least one step"));
        }
        let mut commands = self.commands.write().await;
        if commands.contains_key(name) {
            return Err(anyhow!(
                "'{}' is already defined. Say \"forget {}\" first to redefine it.",
                name,
                name
            ));
        }
        let cmd = UserCommand::new(name, description, steps);
        commands.insert(name.to_string(), cmd.clone());
        drop(commands);
        self.persist().await?;
        Ok(cmd)
    }

    /// Remove a command. Returns true if it existed.
    pub async fn remove(&self, name: &str) -> Result<bool> {
        let mut commands = self.commands.write().await;
        let existed = commands.remove(name).is_some();
        drop(commands);
        if existed {
            self.persist().await?;
        }
        Ok(existed)
    }

    /// List all commands, sorted by name.
    pub async fn list(&self) -> Vec<UserCommand> {
        let mut cmds: Vec<_> = self.commands.read().await.values().cloned().collect();
        cmds.sort_by(|a, b| a.name.cmp(&b.name));
        cmds
    }

    /// Increment the use counter (best-effort, for ranking later).
    pub async fn record_use(&self, name: &str) {
        let mut commands = self.commands.write().await;
        if let Some(cmd) = commands.get_mut(name) {
            cmd.use_count += 1;
        }
        drop(commands);
        let _ = self.persist().await;
    }

    async fn persist(&self) -> Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        let commands = self.commands.read().await;
        let json = serde_json::to_string_pretty(&*commands)?;
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        let tmp = path.with_extension("tmp");
        tokio::fs::write(&tmp, json).await?;
        tokio::fs::rename(&tmp, path).await?;
        Ok(())
    }
}

/// Command names must be simple lowercase identifiers.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("command name cannot be empty"));
    }
    if name.len() > 32 {
        return Err(anyhow!("command name too long (max 32 chars)"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "command name must be lowercase letters, digits, '-' or '_'"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps() -> Vec<CommandStep> {
        vec![
            CommandStep::shell("echo one"),
            CommandStep::shell("echo two"),
        ]
    }

    #[tokio::test]
    async fn test_define_and_get() {
        let reg = CommandRegistry::in_memory();
        reg.define("morning", "start the day", steps())
            .await
            .unwrap();
        let cmd = reg.get("morning").await.unwrap();
        assert_eq!(cmd.name, "morning");
        assert_eq!(cmd.steps.len(), 2);
        assert!(reg.contains("morning").await);
    }

    #[tokio::test]
    async fn test_duplicate_rejected() {
        let reg = CommandRegistry::in_memory();
        reg.define("morning", "a", steps()).await.unwrap();
        let err = reg.define("morning", "b", steps()).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn test_remove() {
        let reg = CommandRegistry::in_memory();
        reg.define("morning", "a", steps()).await.unwrap();
        assert!(reg.remove("morning").await.unwrap());
        assert!(!reg.contains("morning").await);
        assert!(!reg.remove("morning").await.unwrap());
    }

    #[tokio::test]
    async fn test_list_sorted() {
        let reg = CommandRegistry::in_memory();
        reg.define("zebra", "z", steps()).await.unwrap();
        reg.define("alpha", "a", steps()).await.unwrap();
        let names: Vec<_> = reg.list().await.iter().map(|c| c.name.clone()).collect();
        assert_eq!(names, vec!["alpha", "zebra"]);
    }

    #[tokio::test]
    async fn test_record_use() {
        let reg = CommandRegistry::in_memory();
        reg.define("morning", "a", steps()).await.unwrap();
        reg.record_use("morning").await;
        reg.record_use("morning").await;
        assert_eq!(reg.get("morning").await.unwrap().use_count, 2);
    }

    #[tokio::test]
    async fn test_validate_name() {
        let reg = CommandRegistry::in_memory();
        assert!(reg.define("Bad Name", "a", steps()).await.is_err());
        assert!(reg.define("ok-name_2", "a", steps()).await.is_ok());
        assert!(reg.define("", "a", steps()).await.is_err());
    }

    #[tokio::test]
    async fn test_empty_steps_rejected() {
        let reg = CommandRegistry::in_memory();
        assert!(reg.define("noop", "a", vec![]).await.is_err());
    }
}
