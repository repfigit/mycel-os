//! Executor - Code execution for Mycel OS
//!
//! The AI IS the kernel. Its generated code runs with full system access.
//! This is not a sandbox for untrusted code - this is the OS executing its own instructions.
//!
//! Security model: The AI is trusted. Users interact through natural language,
//! and the AI decides what code to run. The AI is responsible for safety.

use anyhow::{anyhow, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info};

use crate::config::MycelConfig;

/// Code executor - runs AI-generated code with full system access
#[derive(Clone)]
pub struct CodeExecutor {
    config: MycelConfig,
}

impl CodeExecutor {
    pub fn new(config: &MycelConfig) -> Result<Self> {
        info!("🔧 Code executor initialized - AI has full system access");
        Ok(Self {
            config: config.clone(),
        })
    }

    /// Execute code and return output
    pub async fn run(&self, code: &str) -> Result<String> {
        let language = self.detect_language(code);

        info!(language = ?language, "Executing kernel-generated code");

        match language {
            Language::Python => self.run_python(code).await,
            Language::JavaScript => self.run_javascript(code).await,
            Language::Shell => self.run_shell(code).await,
        }
    }

    fn detect_language(&self, code: &str) -> Language {
        let code_lower = code.to_lowercase();
        let first_line = code.lines().next().unwrap_or("");

        // Check shebang first
        if first_line.starts_with("#!/usr/bin/python") || first_line.starts_with("#!/usr/bin/env python") {
            return Language::Python;
        }
        if first_line.starts_with("#!/bin/bash") || first_line.starts_with("#!/bin/sh") {
            return Language::Shell;
        }
        if first_line.starts_with("#!/usr/bin/node") || first_line.starts_with("#!/usr/bin/env node") {
            return Language::JavaScript;
        }

        // Python indicators
        if code_lower.contains("import ") || code_lower.contains("def ") || code_lower.contains("print(") {
            return Language::Python;
        }

        // JavaScript indicators
        if code_lower.contains("const ") || code_lower.contains("function ") || code_lower.contains("console.log") {
            return Language::JavaScript;
        }

        // Default to shell - the AI can run any command
        Language::Shell
    }

    async fn write_to_temp_file(&self, code: &str, extension: &str) -> Result<std::path::PathBuf> {
        let filename = format!("{}.{}", uuid::Uuid::new_v4(), extension);
        let path = std::path::Path::new(&self.config.code_path).join(&filename);

        // Ensure directory exists
        tokio::fs::create_dir_all(&self.config.code_path).await?;
        
        // Write code to file
        tokio::fs::write(&path, code).await?;
        
        Ok(path)
    }

    async fn run_python(&self, code: &str) -> Result<String> {
        debug!("Executing Python code as kernel");

        let path = self.write_to_temp_file(code, "py").await?;
        let path_str = path.to_string_lossy().to_string();

        let mut cmd = Command::new("python3");
        cmd.arg(&path_str);

        let result = self.execute_with_timeout(cmd).await;
        
        // Cleanup
        let _ = tokio::fs::remove_file(path).await;
        
        result
    }

    async fn run_javascript(&self, code: &str) -> Result<String> {
        debug!("Executing JavaScript code as kernel");

        let path = self.write_to_temp_file(code, "js").await?;
        let path_str = path.to_string_lossy().to_string();

        let mut cmd = Command::new("node");
        cmd.arg(&path_str);

        let result = self.execute_with_timeout(cmd).await;
        
        // Cleanup
        let _ = tokio::fs::remove_file(path).await;
        
        result
    }

    async fn run_shell(&self, code: &str) -> Result<String> {
        debug!("Executing shell code as kernel");

        // For shell, we still use -c because it's often simpler for one-liners
        // But for consistency we could write to .sh file
        // Let's stick to -c for shell as it usually doesn't hit arg limits for simple tasks
        // and setting +x permissions on a temp file is extra work
        
        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(code);

        self.execute_with_timeout(cmd).await
    }

    async fn execute_with_timeout(&self, mut cmd: Command) -> Result<String> {
        let timeout_duration = Duration::from_secs(self.config.execution_timeout_secs);

        let output = match timeout(
            timeout_duration,
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(anyhow!(
                    "Execution timed out after {} seconds",
                    self.config.execution_timeout_secs
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            if stdout.is_empty() && !stderr.is_empty() {
                // Some commands output to stderr even on success
                Ok(stderr.to_string())
            } else {
                Ok(stdout.to_string())
            }
        } else {
            // Include both stdout and stderr for debugging
            let mut result = String::new();
            if !stderr.is_empty() {
                result.push_str(&stderr);
            }
            if !stdout.is_empty() {
                if !result.is_empty() {
                    result.push_str("\n");
                }
                result.push_str(&stdout);
            }
            if result.is_empty() {
                result = format!("Command exited with code: {:?}", output.status.code());
            }
            Ok(result)
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Language {
    Python,
    JavaScript,
    Shell,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor() -> CodeExecutor {
        let config = crate::config::MycelConfig::default();
        CodeExecutor::new(&config).unwrap()
    }

    #[test]
    fn test_detect_python() {
        let executor = test_executor();
        assert!(matches!(
            executor.detect_language("import os\nprint('hello')"),
            Language::Python
        ));
        assert!(matches!(
            executor.detect_language("def foo():\n    pass"),
            Language::Python
        ));
    }

    #[test]
    fn test_detect_javascript() {
        let executor = test_executor();
        assert!(matches!(
            executor.detect_language("const x = 1;"),
            Language::JavaScript
        ));
        assert!(matches!(
            executor.detect_language("console.log('hi')"),
            Language::JavaScript
        ));
    }

    #[test]
    fn test_detect_shell() {
        let executor = test_executor();
        assert!(matches!(
            executor.detect_language("#!/bin/bash\necho hello"),
            Language::Shell
        ));
    }

    #[test]
    fn test_simple_command_is_shell() {
        let executor = test_executor();
        // Simple commands like "ls" default to shell
        assert!(matches!(
            executor.detect_language("ls -la"),
            Language::Shell
        ));
    }
}
