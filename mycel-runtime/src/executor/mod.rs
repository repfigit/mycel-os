//! Executor - Sandboxed code execution
//!
//! Runs AI-generated code in a secure sandbox to prevent
//! unintended system modifications or security issues.

use anyhow::{anyhow, Result};
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::timeout;
use tracing::{debug, info, warn};

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
        if code_lower.contains("import ")
            || code_lower.contains("def ")
            || code_lower.contains("print(")
        {
            return Language::Python;
        }
        if code_lower.contains("const ")
            || code_lower.contains("function ")
            || code_lower.contains("console.log")
        {
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

        // Memory limit in bytes (config is in MB)
        let memory_limit = format!("{}", self.config.execution_memory_mb as u64 * 1024 * 1024);

        let mut cmd = match self.sandbox_type {
            SandboxType::Firejail => {
                let mut c = Command::new("firejail");
                c.args([
                    "--quiet",
                    "--private",
                    "--net=none",
                    "--nosound",
                    "--no3d",
                    &format!("--rlimit-as={}", memory_limit),
                    "python3",
                    "-c",
                ]);
                c.arg(code);
                c
            }
            SandboxType::Bubblewrap => {
                let mut c = Command::new("bwrap");
                c.args([
                    "--ro-bind",
                    "/",
                    "/",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--tmpfs",
                    "/tmp",
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

        // Execute with timeout
        let timeout_duration = Duration::from_secs(self.config.execution_timeout_secs);
        info!(
            timeout_secs = self.config.execution_timeout_secs,
            "Running code with timeout"
        );

        let output = match timeout(
            timeout_duration,
            cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).output(),
        )
        .await
        {
            Ok(result) => result?,
            Err(_) => {
                return Err(anyhow!(
                    "Code execution timed out after {} seconds",
                    self.config.execution_timeout_secs
                ));
            }
        };

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

        let memory_limit = format!("{}", self.config.execution_memory_mb as u64 * 1024 * 1024);

        let mut cmd = match self.sandbox_type {
            SandboxType::Firejail => {
                let mut c = Command::new("firejail");
                c.args([
                    "--quiet",
                    "--private",
                    "--net=none",
                    &format!("--rlimit-as={}", memory_limit),
                    "node",
                    "-e",
                ]);
                c.arg(code);
                c
            }
            SandboxType::Bubblewrap => {
                let mut c = Command::new("bwrap");
                c.args([
                    "--ro-bind",
                    "/",
                    "/",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--tmpfs",
                    "/tmp",
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

        // Execute with timeout
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
                    "Code execution timed out after {} seconds",
                    self.config.execution_timeout_secs
                ));
            }
        };

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

        let memory_limit = format!("{}", self.config.execution_memory_mb as u64 * 1024 * 1024);

        let mut cmd = match self.sandbox_type {
            SandboxType::Firejail => {
                let mut c = Command::new("firejail");
                c.args([
                    "--quiet",
                    "--private",
                    "--net=none",
                    "--read-only=/",
                    &format!("--rlimit-as={}", memory_limit),
                    "bash",
                    "-c",
                ]);
                c.arg(code);
                c
            }
            SandboxType::Bubblewrap => {
                let mut c = Command::new("bwrap");
                c.args([
                    "--ro-bind",
                    "/",
                    "/",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--tmpfs",
                    "/tmp",
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
                return Err(anyhow!(
                    "Shell execution ALWAYS requires sandboxing. Install firejail or bubblewrap."
                ));
            }
        };

        // Execute with timeout
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
                    "Code execution timed out after {} seconds",
                    self.config.execution_timeout_secs
                ));
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);

        if output.status.success() {
            Ok(stdout.to_string())
        } else {
            Ok(format!("Error:\n{}\n\nOutput:\n{}", stderr, stdout))
        }
    }

    fn validate_python_code(&self, code: &str) -> Result<()> {
        // Direct dangerous patterns
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
            "os.popen",
            "commands.",
            "pty.",
        ];

        for pattern in dangerous_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains potentially dangerous pattern: {}",
                    pattern
                ));
            }
        }

        // Bypass attempt patterns - catch common evasion techniques
        let bypass_patterns = [
            "getattr(",       // getattr(__builtins__, '__import__')
            "__builtins__",   // Direct builtins access
            "__class__",      // Class introspection
            "__bases__",      // Class hierarchy access
            "__subclasses__", // Subclass enumeration
            "__mro__",        // Method resolution order
            "__globals__",    // Global namespace access
            "__code__",       // Code object access
            "compile(",       // Dynamic compilation
            "chr(",           // Character code bypass: chr(111)+chr(115) = 'os'
            "ord(",           // Often used with chr for obfuscation
            "globals(",       // Accessing global namespace
            "locals(",        // Accessing local namespace
            "vars(",          // Variable access
            "dir(",           // Directory listing (can be used for discovery)
            "type(",          // Type manipulation
            "setattr(",       // Setting attributes
            "delattr(",       // Deleting attributes
            "importlib",      // Import library
            "pkgutil",        // Package utilities
            "sys.modules",    // Module cache access
            "ctypes",         // C type interface (dangerous)
            "cffi",           // C FFI (dangerous)
            "pickle",         // Deserialization attacks
            "marshal",        // Code serialization
            "codecs",         // Can be used for encoding tricks
        ];

        for pattern in bypass_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains potential bypass pattern: {} (This could be used to circumvent security restrictions)",
                    pattern
                ));
            }
        }

        // Check for string concatenation that might form dangerous patterns
        // e.g., 'os' + '.system' or 'sub' + 'process'
        let code_no_whitespace = code.replace(' ', "");
        let concat_bypass_indicators = [
            "+\"system",
            "+'system",
            "+\"import",
            "+'import",
            "\"os\"",
            "'os'",
        ];

        for indicator in concat_bypass_indicators {
            if code_no_whitespace.contains(indicator) {
                return Err(anyhow!(
                    "Code contains suspicious string pattern that may be attempting to bypass security"
                ));
            }
        }

        Ok(())
    }

    fn validate_js_code(&self, code: &str) -> Result<()> {
        let dangerous_patterns = [
            "require('child_process')",
            "require(\"child_process\")",
            "require('fs')",
            "require(\"fs\")",
            "require('net')",
            "require(\"net\")",
            "require('http')",
            "require(\"http\")",
            "require('https')",
            "require(\"https\")",
            "process.exit",
            "process.kill",
            "process.env",
            "eval(",
            "Function(", // Function constructor can execute code
            "new Function",
            "child_process",
            "spawn(",
            "exec(",
            "execSync(",
            "execFile(",
            "__dirname",
            "__filename",
        ];

        for pattern in dangerous_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains potentially dangerous pattern: {}",
                    pattern
                ));
            }
        }

        // Check for dynamic require (common bypass)
        let bypass_patterns = [
            "require(variable", // Dynamic require
            "require(String",   // String manipulation
            "require([",        // Array-based require
            "import(",          // Dynamic import
            "globalThis",       // Global context access
            "Reflect.",         // Reflection API
            "Proxy(",           // Proxy objects
        ];

        for pattern in bypass_patterns {
            if code.contains(pattern) {
                return Err(anyhow!(
                    "Code contains potential bypass pattern: {}",
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
                return Err(anyhow!("Code contains dangerous pattern: {}", pattern));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor() -> CodeExecutor {
        let config = crate::config::MycelConfig::default();
        CodeExecutor::new(&config).unwrap()
    }

    // Python validation tests

    #[test]
    fn test_python_safe_code_allowed() {
        let executor = test_executor();
        let code = r#"
print("Hello, World!")
x = 1 + 2
print(f"Result: {x}")
"#;
        assert!(executor.validate_python_code(code).is_ok());
    }

    #[test]
    fn test_python_os_system_blocked() {
        let executor = test_executor();
        let code = r#"import os; os.system("whoami")"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_subprocess_blocked() {
        let executor = test_executor();
        let code = r#"import subprocess; subprocess.run(['ls'])"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_eval_blocked() {
        let executor = test_executor();
        let code = r#"eval("print('hello')")"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_exec_blocked() {
        let executor = test_executor();
        let code = r#"exec("print('hello')")"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_open_blocked() {
        let executor = test_executor();
        let code = r#"open("/etc/passwd", "r").read()"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    // Bypass attempt tests

    #[test]
    fn test_python_getattr_bypass_blocked() {
        let executor = test_executor();
        let code = r#"getattr(__builtins__, '__import__')('os')"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_builtins_access_blocked() {
        let executor = test_executor();
        let code = r#"__builtins__.__import__('os')"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_class_introspection_blocked() {
        let executor = test_executor();
        let code = r#"().__class__.__bases__[0].__subclasses__()"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_chr_bypass_blocked() {
        let executor = test_executor();
        let code = r#"chr(111)+chr(115)"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_compile_blocked() {
        let executor = test_executor();
        let code = r#"compile("import os", "", "exec")"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_ctypes_blocked() {
        let executor = test_executor();
        let code = r#"import ctypes"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    #[test]
    fn test_python_pickle_blocked() {
        let executor = test_executor();
        let code = r#"import pickle"#;
        assert!(executor.validate_python_code(code).is_err());
    }

    // JavaScript validation tests

    #[test]
    fn test_js_safe_code_allowed() {
        let executor = test_executor();
        let code = r#"console.log("Hello, World!");"#;
        assert!(executor.validate_js_code(code).is_ok());
    }

    #[test]
    fn test_js_child_process_blocked() {
        let executor = test_executor();
        let code = r#"require('child_process').exec('ls')"#;
        assert!(executor.validate_js_code(code).is_err());
    }

    #[test]
    fn test_js_fs_blocked() {
        let executor = test_executor();
        let code = r#"require('fs').readFileSync('/etc/passwd')"#;
        assert!(executor.validate_js_code(code).is_err());
    }

    #[test]
    fn test_js_eval_blocked() {
        let executor = test_executor();
        let code = r#"eval("console.log('test')")"#;
        assert!(executor.validate_js_code(code).is_err());
    }

    #[test]
    fn test_js_function_constructor_blocked() {
        let executor = test_executor();
        let code = r#"new Function("console.log('test')")()"#;
        assert!(executor.validate_js_code(code).is_err());
    }

    #[test]
    fn test_js_process_env_blocked() {
        let executor = test_executor();
        let code = r#"console.log(process.env.HOME)"#;
        assert!(executor.validate_js_code(code).is_err());
    }

    // Shell validation tests

    #[test]
    fn test_shell_safe_code_allowed() {
        let executor = test_executor();
        let code = r#"echo "Hello, World!""#;
        assert!(executor.validate_shell_code(code).is_ok());
    }

    #[test]
    fn test_shell_rm_rf_blocked() {
        let executor = test_executor();
        let code = r#"rm -rf /"#;
        assert!(executor.validate_shell_code(code).is_err());
    }

    #[test]
    fn test_shell_dd_blocked() {
        let executor = test_executor();
        let code = r#"dd if=/dev/zero of=/dev/sda"#;
        assert!(executor.validate_shell_code(code).is_err());
    }

    #[test]
    fn test_shell_curl_bash_blocked() {
        let executor = test_executor();
        // The pattern is exactly "curl | bash"
        let code = r#"curl | bash"#;
        assert!(executor.validate_shell_code(code).is_err());
    }

    // Language detection tests

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
            executor.detect_language("function foo() {}"),
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
}
