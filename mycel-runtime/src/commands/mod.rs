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
    /// Named parameters for templates (Tier 2). Steps reference them as {param}.
    #[serde(default)]
    pub params: Vec<String>,
    /// Raw bash body for logic commands (Tier 3). When set, takes precedence
    /// over steps and is run as a single script through the policy-gated executor.
    #[serde(default)]
    pub script: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub use_count: u64,
}

impl UserCommand {
    fn new(name: &str, description: &str, steps: Vec<CommandStep>) -> Self {
        Self {
            name: name.to_string(),
            description: description.to_string(),
            steps,
            params: Vec::new(),
            script: None,
            created_at: chrono::Utc::now(),
            use_count: 0,
        }
    }

    /// Whether this is a raw-script (Tier 3) command.
    pub fn is_script(&self) -> bool {
        self.script.is_some()
    }

    /// Substitute {param} placeholders in a string with provided arg values.
    /// Args are positionally bound to self.params. Values are shell-escaped
    /// (single-quoted) so they are treated as data, not code.
    pub fn substitute(&self, text: &str, args: &[String]) -> String {
        let mut out = text.to_string();
        for (i, param) in self.params.iter().enumerate() {
            let placeholder = format!("{{{}}}", param);
            let value = args.get(i).map(|s| s.as_str()).unwrap_or("");
            // Single-quote and escape embedded single quotes: safe shell data.
            let escaped = shell_escape(value);
            out = out.replace(&placeholder, &escaped);
        }
        out
    }
}

/// Escape a value for safe embedding in a shell command as a single-quoted
/// string. Prevents parameter injection: the value can never break out into
/// new commands. 'foo' -> 'foo'; o'brien -> 'o'\''brien'
pub fn shell_escape(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// A command definition awaiting user confirmation before being saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingCommandDef {
    pub name: String,
    pub description: String,
    pub steps: Vec<CommandStep>,
    #[serde(default)]
    pub params: Vec<String>,
    #[serde(default)]
    pub script: Option<String>,
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
        self.define_full(name, description, steps, Vec::new(), None)
            .await
    }

    /// Define a command with parameters and/or a raw script body.
    pub async fn define_full(
        &self,
        name: &str,
        description: &str,
        steps: Vec<CommandStep>,
        params: Vec<String>,
        script: Option<String>,
    ) -> Result<UserCommand> {
        validate_name(name)?;
        if script.is_none() && steps.is_empty() {
            return Err(anyhow!("a command needs at least one step"));
        }
        for p in &params {
            validate_param(p)?;
        }
        let mut commands = self.commands.write().await;
        if commands.contains_key(name) {
            return Err(anyhow!(
                "'{}' is already defined. Say \"forget {}\" first to redefine it.",
                name,
                name
            ));
        }
        let mut cmd = UserCommand::new(name, description, steps);
        cmd.params = params;
        cmd.script = script;
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

/// Parameter names must be simple identifiers (used as {name} placeholders).
fn validate_param(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("parameter name cannot be empty"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(anyhow!("parameter names must be letters, digits, or '_'"));
    }
    Ok(())
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

    #[tokio::test]
    async fn test_define_full_with_params() {
        let reg = CommandRegistry::in_memory();
        let cmd = reg
            .define_full(
                "backup",
                "tar a folder",
                vec![CommandStep::shell("tar czf {folder}.tgz {folder}")],
                vec!["folder".to_string()],
                None,
            )
            .await
            .unwrap();
        assert_eq!(cmd.params, vec!["folder"]);
        assert!(!cmd.is_script());
    }

    #[tokio::test]
    async fn test_define_script() {
        let reg = CommandRegistry::in_memory();
        let cmd = reg
            .define_full(
                "cleanup",
                "remove old files",
                vec![],
                vec![],
                Some("for f in *; do rm -i \"$f\"; done".to_string()),
            )
            .await
            .unwrap();
        assert!(cmd.is_script());
    }

    #[tokio::test]
    async fn test_script_allows_empty_steps() {
        let reg = CommandRegistry::in_memory();
        // A script command with no steps is valid (script takes precedence).
        assert!(reg
            .define_full("s", "d", vec![], vec![], Some("echo hi".to_string()))
            .await
            .is_ok());
        // But no steps AND no script is invalid.
        assert!(reg
            .define_full("s2", "d", vec![], vec![], None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn test_invalid_param_rejected() {
        let reg = CommandRegistry::in_memory();
        assert!(reg
            .define_full("bad", "d", steps(), vec!["bad param!".to_string()], None)
            .await
            .is_err());
    }

    #[test]
    fn test_substitute() {
        let mut cmd = UserCommand::new("backup", "d", vec![]);
        cmd.params = vec!["folder".to_string()];
        let out = cmd.substitute("tar czf {folder}.tgz {folder}", &["docs".to_string()]);
        assert_eq!(out, "tar czf 'docs'.tgz 'docs'");
    }

    #[test]
    fn test_substitute_missing_arg_becomes_empty() {
        let mut cmd = UserCommand::new("backup", "d", vec![]);
        cmd.params = vec!["folder".to_string()];
        let out = cmd.substitute("echo {folder}", &[]);
        assert_eq!(out, "echo ''");
    }

    #[test]
    fn test_shell_escape_prevents_injection() {
        // A malicious value must not break out into a new command.
        let evil = "$(rm -rf /)";
        let escaped = shell_escape(evil);
        assert_eq!(escaped, "'$(rm -rf /)'"); // inert single-quoted string
                                              // Single quotes are escaped.
        assert_eq!(shell_escape("o'brien"), "'o'\\''brien'");
    }
}
