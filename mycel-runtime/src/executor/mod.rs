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
    sandbox_type: SandboxType,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum SandboxType {
    Firejail,
    Bubblewrap,
    None,
}

impl CodeExecutor {
    pub fn new(config: &MycelConfig) -> Result<Self> {
        // Check if we have sandboxing capabilities
        let sandbox_type = Self::check_sandbox_available();
        
        if config.sandbox_enabled && sandbox_type == SandboxType::None {
            warn!("Sandboxing enabled but no supported sandbox tool found (firejail, bwrap). Execution will be BLOCKED.");
        }

        Ok(Self {
            config: config.clone(),
            sandbox_type,
        })
    }

    fn check_sandbox_available() -> SandboxType {
        // Check for firejail
        if Self::check_command("firejail") {
            return SandboxType::Firejail;
        }
        
        // Check for bwrap
        if Self::check_command("bwrap") {
            return SandboxType::Bubblewrap;
        }

        SandboxType::None
    }

    fn check_command(cmd: &str) -> bool {
        std::process::Command::new("which")
            .arg(cmd)
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

        let mut cmd = match self.sandbox_type {
            SandboxType::Firejail => {
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
            }
            SandboxType::Bubblewrap => {
                let mut c = Command::new("bwrap");
                c.args([
                    "--ro-bind", "/", "/",
                    "--dev", "/dev",
                    "--proc", "/proc",
                    "--tmpfs", "/tmp",
                    "--unshare-all",
                    "--new-session",
                    "--die-with-parent",
                    "python3",
                    "-c",
                ]);
                c.arg(code);
                c
            }
            SandboxType::None => {
                if self.config.sandbox_enabled {
                    return Err(anyhow!("Sandbox enabled but no sandbox tool available. Install firejail or bubblewrap, or disable sandbox in config (unsafe)."));
                }
                let mut c = Command::new("python3");
                c.arg("-c").arg(code);
                c
            }
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

        let mut cmd = match self.sandbox_type {
            SandboxType::Firejail => {
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
            }
            SandboxType::Bubblewrap => {
                let mut c = Command::new("bwrap");
                c.args([
                    "--ro-bind", "/", "/",
                    "--dev", "/dev",
                    "--proc", "/proc",
                    "--tmpfs", "/tmp",
                    "--unshare-all",
                    "--new-session",
                    "--die-with-parent",
                    "node",
                    "-e",
                ]);
                c.arg(code);
                c
            }
            SandboxType::None => {
                if self.config.sandbox_enabled {
                    return Err(anyhow!("Sandbox enabled but no sandbox tool available. Install firejail or bubblewrap, or disable sandbox in config (unsafe)."));
                }
                let mut c = Command::new("node");
                c.arg("-e").arg(code);
                c
            }
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

        let mut cmd = match self.sandbox_type {
            SandboxType::Firejail => {
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
            }
            SandboxType::Bubblewrap => {
                let mut c = Command::new("bwrap");
                c.args([
                    "--ro-bind", "/", "/",
                    "--dev", "/dev",
                    "--proc", "/proc",
                    "--tmpfs", "/tmp",
                    "--unshare-all",
                    "--new-session",
                    "--die-with-parent",
                    "bash",
                    "-c",
                ]);
                c.arg(code);
                c
            }
            SandboxType::None => {
                // Without sandbox, refuse to run shell regardless of config
                return Err(anyhow!("Shell execution ALWAYS requires sandboxing. Install firejail or bubblewrap."));
            }
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
