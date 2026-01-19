//! Executor - Sandboxed code execution
//!
//! Runs AI-generated code in a secure sandbox to prevent
//! unintended system modifications or security issues.

use anyhow::{anyhow, Result};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tracing::{debug, warn};

use crate::config::MycelConfig;

/// Code executor with sandboxing
#[derive(Clone)]
pub struct CodeExecutor {
    config: MycelConfig,
    sandbox_enabled: bool,
}

impl CodeExecutor {
    pub fn new(config: &MycelConfig) -> Result<Self> {
        // Check if we have sandboxing capabilities
        let sandbox_enabled = Self::check_sandbox_available();
        
        if !sandbox_enabled {
            warn!("Sandboxing not available - running in restricted mode");
        }

        Ok(Self {
            config: config.clone(),
            sandbox_enabled,
        })
    }

    fn check_sandbox_available() -> bool {
        // Check for various sandboxing options
        // In production, we'd use gVisor, Firecracker, or Linux namespaces
        // For now, we'll use a simple approach
        
        // Check if firejail is available
        std::process::Command::new("which")
            .arg("firejail")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Execute code and return output
    pub async fn run(&self, code: &str) -> Result<String> {
        // Detect language (for now, assume Python)
        let language = self.detect_language(code);

        match language {
            Language::Python => self.run_python(code).await,
            Language::JavaScript => self.run_javascript(code).await,
            Language::Shell => self.run_shell(code).await,
            Language::Unknown => Err(anyhow!("Unknown code language")),
        }
    }

    fn detect_language(&self, code: &str) -> Language {
        let code_lower = code.to_lowercase();
        
        // Simple heuristics
        if code_lower.contains("import ") || code_lower.contains("def ") || code_lower.contains("print(") {
            return Language::Python;
        }
        if code_lower.contains("const ") || code_lower.contains("function ") || code_lower.contains("console.log") {
            return Language::JavaScript;
        }
        if code_lower.starts_with("#!/bin/bash") || code_lower.starts_with("#!/bin/sh") {
            return Language::Shell;
        }
        
        // Default to Python
        Language::Python
    }

    async fn run_python(&self, code: &str) -> Result<String> {
        debug!("Executing Python code");

        // Validate code before execution
        self.validate_python_code(code)?;

        let mut cmd = if self.sandbox_enabled {
            let mut c = Command::new("firejail");
            c.args([
                "--quiet",
                "--private",
                "--net=none",
                "--nosound",
                "--no3d",
                "python3",
                "-c",
            ]);
            c.arg(code);
            c
        } else {
            let mut c = Command::new("python3");
            c.arg("-c").arg(code);
            c
        };

        let output = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("Error:\n{}\n\nOutput:\n{}", stderr, stdout))
        }
    }

    async fn run_javascript(&self, code: &str) -> Result<String> {
        debug!("Executing JavaScript code");

        self.validate_js_code(code)?;

        let mut cmd = if self.sandbox_enabled {
            let mut c = Command::new("firejail");
            c.args([
                "--quiet",
                "--private",
                "--net=none",
                "node",
                "-e",
            ]);
            c.arg(code);
            c
        } else {
            let mut c = Command::new("node");
            c.arg("-e").arg(code);
            c
        };

        let output = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("Error:\n{}\n\nOutput:\n{}", stderr, stdout))
        }
    }

    async fn run_shell(&self, code: &str) -> Result<String> {
        debug!("Executing shell code");

        // Shell is dangerous - extra validation
        self.validate_shell_code(code)?;

        let mut cmd = if self.sandbox_enabled {
            let mut c = Command::new("firejail");
            c.args([
                "--quiet",
                "--private",
                "--net=none",
                "--read-only=/",
                "bash",
                "-c",
            ]);
            c.arg(code);
            c
        } else {
            // Without sandbox, refuse to run shell
            return Err(anyhow!("Shell execution requires sandboxing"));
        };

        let output = cmd
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await?;

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("Error:\n{}\n\nOutput:\n{}", stderr, stdout))
        }
    }

    fn validate_python_code(&self, code: &str) -> Result<()> {
        let dangerous_patterns = [
            "os.system",
            "subprocess",
            "eval(",
            "exec(",
            "__import__",
            "open(",
            "shutil.rmtree",
            "os.remove",
            "os.unlink",
        ];

        for pattern in dangerous_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains potentially dangerous pattern: {}",
                    pattern
                ));
            }
        }

        Ok(())
    }

    fn validate_js_code(&self, code: &str) -> Result<()> {
        let dangerous_patterns = [
            "require('child_process')",
            "require('fs')",
            "process.exit",
            "eval(",
        ];

        for pattern in dangerous_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains potentially dangerous pattern: {}",
                    pattern
                ));
            }
        }

        Ok(())
    }

    fn validate_shell_code(&self, code: &str) -> Result<()> {
        let dangerous_patterns = [
            "rm -rf",
            "rm -r",
            "mkfs",
            "dd if=",
            "> /dev/",
            "chmod 777",
            "curl | bash",
            "wget | bash",
        ];

        for pattern in dangerous_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains dangerous pattern: {}",
                    pattern
                ));
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
enum Language {
    Python,
    JavaScript,
    Shell,
    Unknown,
}

/// Result of code execution
#[derive(Debug, Clone)]
pub struct ExecutionResult {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub duration_ms: u64,
}
