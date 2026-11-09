//! Executor - Code execution for Mycel OS
//!
//! Security model: the model is NOT trusted. Its output is shaped by
//! everything it reads (files, web results, mesh events, chat history), so
//! generated code is treated as untrusted and runs isolated from the host:
//!
//! - `SandboxMode::Bwrap` (default): bubblewrap with no network, a read-only
//!   view of the filesystem, credential and runtime-state locations hidden,
//!   private /tmp and /run (so host unix sockets such as the IPC socket and
//!   docker.sock are unreachable), an empty environment, no capabilities, and
//!   no nested user namespaces. If bwrap is unavailable, execution is
//!   refused; it never silently falls back to the host.
//! - `SandboxMode::Host`: explicit opt-in to run with the daemon's privileges.
//!
//! In both modes: wall-clock timeout that kills the whole process tree, CPU
//! time limit, no core dumps, output capped at 1 MiB per stream. Python gets
//! an address-space limit of `execution_memory_mb`; JavaScript gets a V8 heap
//! cap (RLIMIT_AS breaks node at startup). Shell is not memory-limited here:
//! it may legitimately launch node. Process-count limits need cgroups and are
//! not enforced yet.

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::{MycelConfig, SandboxConfig, SandboxMode};

/// Maximum bytes captured from each of stdout and stderr.
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Where the program file is mounted inside the sandbox.
const SANDBOX_CODE_DIR: &str = "/run/mycel-exec";

/// Credential and session locations hidden from sandboxed code. Directories
/// are replaced by an empty tmpfs, files by /dev/null. Relative to $HOME.
const HIDDEN_HOME_PATHS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".password-store",
    ".config/gh",
    ".config/gcloud",
    ".config/mycel",
    ".local/share/keyrings",
    ".local/share/mycel",
    ".mozilla",
    ".config/google-chrome",
    ".config/chromium",
    ".netrc",
    ".git-credentials",
    ".pgpass",
    ".npmrc",
    ".pypirc",
    ".bash_history",
    ".zsh_history",
    ".python_history",
];

/// Absolute system paths hidden from sandboxed code. `/sys/class/net` is the
/// host's sysfs view (not namespaced) and would list host interface names.
const HIDDEN_SYSTEM_PATHS: &[&str] = &["/etc/mycel", "/var/lib/mycel", "/root", "/sys/class/net"];

/// Code executor - runs AI-generated code inside the configured sandbox.
#[derive(Clone)]
pub struct CodeExecutor {
    config: MycelConfig,
}

impl CodeExecutor {
    pub fn new(config: &MycelConfig) -> Result<Self> {
        match config.sandbox.mode {
            SandboxMode::Bwrap => {
                if bwrap_available(&config.sandbox.bwrap_path) {
                    info!(
                        "Code executor: bubblewrap sandbox (network: {})",
                        if config.sandbox.allow_network {
                            "on"
                        } else {
                            "off"
                        }
                    );
                } else {
                    warn!(
                        "Code executor: bubblewrap not found at '{}'; code execution will be \
                         refused. Install bubblewrap or set [sandbox] mode = \"host\".",
                        config.sandbox.bwrap_path
                    );
                }
            }
            SandboxMode::Host => {
                warn!(
                    "Code executor: HOST mode - generated code runs unsandboxed with daemon \
                     privileges"
                );
            }
        }
        Ok(Self {
            config: config.clone(),
        })
    }

    /// Execute code and return output
    pub async fn run(&self, code: &str) -> Result<String> {
        let language = self.detect_language(code);
        info!(language = ?language, mode = ?self.config.sandbox.mode, "Executing generated code");

        let script = self.write_program(code, language.extension()).await?;
        let result = self.execute(language, &script).await;
        let _ = tokio::fs::remove_file(&script).await;
        result
    }

    fn detect_language(&self, code: &str) -> Language {
        let code_lower = code.to_lowercase();
        let first_line = code.lines().next().unwrap_or("");

        // Check shebang first
        if first_line.starts_with("#!/usr/bin/python")
            || first_line.starts_with("#!/usr/bin/env python")
        {
            return Language::Python;
        }
        if first_line.starts_with("#!/bin/bash") || first_line.starts_with("#!/bin/sh") {
            return Language::Shell;
        }
        if first_line.starts_with("#!/usr/bin/node")
            || first_line.starts_with("#!/usr/bin/env node")
        {
            return Language::JavaScript;
        }

        // Python indicators
        if code_lower.contains("import ")
            || code_lower.contains("def ")
            || code_lower.contains("print(")
        {
            return Language::Python;
        }

        // JavaScript indicators
        if code_lower.contains("const ")
            || code_lower.contains("function ")
            || code_lower.contains("console.log")
        {
            return Language::JavaScript;
        }

        Language::Shell
    }

    /// Write the program to a new owner-only file in the code directory.
    async fn write_program(&self, code: &str, extension: &str) -> Result<PathBuf> {
        let dir = PathBuf::from(&self.config.code_path);
        tokio::fs::create_dir_all(&dir).await?;
        let path = dir.join(format!("{}.{}", uuid::Uuid::new_v4(), extension));
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .await?;
        file.write_all(code.as_bytes()).await?;
        file.flush().await?;
        Ok(path)
    }

    /// The command line run inside the sandbox (or on the host), minus the
    /// script path which is appended last. Resource limits are applied by a
    /// `/bin/sh` prelude so no `unsafe` pre-exec hook is needed.
    fn inner_argv(&self, language: Language) -> Vec<String> {
        let cpu = self.config.execution_timeout_secs.max(1);
        let mut prelude = format!("ulimit -c 0 && ulimit -t {}", cpu);
        let interpreter: &[&str] = match language {
            Language::Python => {
                let kb = u64::from(self.config.execution_memory_mb) * 1024;
                prelude.push_str(&format!(" && ulimit -v {}", kb));
                &["python3"]
            }
            Language::JavaScript => &["node", "--max-old-space-size=256"],
            Language::Shell => &["bash"],
        };
        prelude.push_str(" && exec \"$@\"");
        let mut argv = vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            prelude,
            "mycel-exec".to_string(),
        ];
        argv.extend(interpreter.iter().map(|s| s.to_string()));
        argv
    }

    /// Paths specific to this runtime instance that sandboxed code must not see.
    fn instance_hidden_paths(&self) -> Vec<PathBuf> {
        vec![
            PathBuf::from(&self.config.context_path),
            PathBuf::from(&self.config.code_path),
        ]
    }

    async fn execute(&self, language: Language, script: &Path) -> Result<String> {
        let inner = self.inner_argv(language);
        let mut cmd = match self.config.sandbox.mode {
            SandboxMode::Bwrap => {
                if !bwrap_available(&self.config.sandbox.bwrap_path) {
                    return Err(anyhow!(
                        "sandbox unavailable: bubblewrap not found at '{}'. Refusing to run \
                         generated code on the host. Install bubblewrap, or set \
                         [sandbox] mode = \"host\" to opt out.",
                        self.config.sandbox.bwrap_path
                    ));
                }
                let args = bwrap_args(
                    &self.config.sandbox,
                    &inner,
                    script,
                    &dirs::home_dir(),
                    &self.instance_hidden_paths(),
                );
                let mut c = Command::new(&self.config.sandbox.bwrap_path);
                c.args(&args);
                c
            }
            SandboxMode::Host => {
                let mut c = Command::new(&inner[0]);
                c.args(&inner[1..]).arg(script);
                c
            }
        };
        self.execute_with_timeout(&mut cmd).await
    }

    async fn execute_with_timeout(&self, cmd: &mut Command) -> Result<String> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Own process group, so a timeout kills everything the code spawned.
            .process_group(0)
            .kill_on_drop(true);

        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
        let mut stderr = child.stderr.take().ok_or_else(|| anyhow!("no stderr"))?;

        let secs = self.config.execution_timeout_secs;
        let run = async {
            tokio::join!(
                read_capped(&mut stdout),
                read_capped(&mut stderr),
                child.wait()
            )
        };

        let (out, err, status) = match tokio::time::timeout(Duration::from_secs(secs), run).await {
            Ok(r) => r,
            Err(_) => {
                if let Some(pgid) = pgid {
                    kill_group(pgid);
                }
                return Err(anyhow!("Execution timed out after {} seconds", secs));
            }
        };
        let status = status?;
        let (stdout, out_truncated) = out?;
        let (stderr, err_truncated) = err?;

        let mut result = if status.success() {
            if stdout.is_empty() && !stderr.is_empty() {
                // Some commands output to stderr even on success
                stderr
            } else {
                stdout
            }
        } else {
            let mut r = String::new();
            if !stderr.is_empty() {
                r.push_str(&stderr);
            }
            if !stdout.is_empty() {
                if !r.is_empty() {
                    r.push('\n');
                }
                r.push_str(&stdout);
            }
            if r.is_empty() {
                r = format!("Command exited with code: {:?}", status.code());
            }
            r
        };
        if out_truncated || err_truncated {
            result.push_str("\n[output truncated]");
        }
        debug!(exit = ?status.code(), bytes = result.len(), "Execution finished");
        Ok(result)
    }
}

/// Read a stream to completion, keeping at most MAX_OUTPUT_BYTES.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<(String, bool)> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
        if n > room {
            truncated = true;
        }
    }
    Ok((String::from_utf8_lossy(&kept).into_owned(), truncated))
}

/// SIGKILL a process group. In bwrap mode this kills bwrap, and with
/// --die-with-parent plus the PID namespace the whole sandbox goes with it.
fn kill_group(pgid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let Ok(raw) = i32::try_from(pgid) else {
        return;
    };
    if let Err(e) = killpg(Pid::from_raw(raw), Signal::SIGKILL) {
        debug!("killpg({}) failed: {}", pgid, e);
    }
}

/// Whether bwrap can be executed.
fn bwrap_available(path: &str) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Expand a leading `~` against `home`.
fn expand_home(p: &str, home: &Option<PathBuf>) -> Option<PathBuf> {
    if let Some(rest) = p.strip_prefix("~/") {
        home.as_ref().map(|h| h.join(rest))
    } else if p == "~" {
        home.clone()
    } else {
        Some(PathBuf::from(p))
    }
}

/// Build the bwrap argument list. `inner` is the command run inside the
/// sandbox; the script path inside the sandbox is appended to it. Hidden
/// paths are resolved through symlinks and skipped if they don't exist.
fn bwrap_args(
    cfg: &SandboxConfig,
    inner: &[String],
    script: &Path,
    home: &Option<PathBuf>,
    extra_hidden: &[PathBuf],
) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    fn push(a: &mut Vec<String>, items: &[&str]) {
        a.extend(items.iter().map(|s| s.to_string()));
    }

    // Read-only view of the host, fresh /dev and /proc, private /run and /tmp.
    push(
        &mut a,
        &["--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc"],
    );
    push(&mut a, &["--tmpfs", "/run"]);
    let tmp_bytes = u64::from(cfg.tmp_size_mb) * 1024 * 1024;
    a.extend([
        "--size".to_string(),
        tmp_bytes.to_string(),
        "--tmpfs".to_string(),
        "/tmp".to_string(),
    ]);

    // Hide credential and runtime-state locations.
    let mut hidden: Vec<PathBuf> = HIDDEN_SYSTEM_PATHS.iter().map(PathBuf::from).collect();
    if let Some(h) = home {
        hidden.extend(HIDDEN_HOME_PATHS.iter().map(|p| h.join(p)));
    }
    hidden.extend(cfg.hidden_paths.iter().filter_map(|p| expand_home(p, home)));
    hidden.extend(extra_hidden.iter().cloned());
    let mut seen = std::collections::HashSet::new();
    for p in hidden {
        // Resolve symlinks so the mount lands on the real location.
        let Ok(real) = std::fs::canonicalize(&p) else {
            continue; // doesn't exist: nothing to hide
        };
        if real == Path::new("/") || !seen.insert(real.clone()) {
            continue;
        }
        let s = real.to_string_lossy().to_string();
        if real.is_dir() {
            a.extend(["--tmpfs".to_string(), s]);
        } else {
            a.extend(["--ro-bind".to_string(), "/dev/null".to_string(), s]);
        }
    }

    // Explicitly writable directories.
    for w in cfg
        .writable_paths
        .iter()
        .filter_map(|p| expand_home(p, home))
    {
        if let Ok(real) = std::fs::canonicalize(&w) {
            if real.is_dir() {
                let s = real.to_string_lossy().to_string();
                a.extend(["--bind".to_string(), s.clone(), s]);
            }
        }
    }

    // The program, read-only, under the private /run.
    let inside = format!(
        "{}/{}",
        SANDBOX_CODE_DIR,
        script
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_else(|| "program".to_string())
    );
    a.extend([
        "--ro-bind".to_string(),
        script.to_string_lossy().to_string(),
        inside.clone(),
    ]);

    // Namespaces: everything (network only if explicitly allowed). No
    // capabilities, no nested user namespaces, no controlling terminal.
    push(
        &mut a,
        &["--unshare-all", "--unshare-user", "--disable-userns"],
    );
    if cfg.allow_network {
        push(&mut a, &["--share-net"]);
    }
    push(
        &mut a,
        &["--cap-drop", "ALL", "--die-with-parent", "--new-session"],
    );

    // Minimal environment.
    push(
        &mut a,
        &[
            "--clearenv",
            "--setenv",
            "PATH",
            "/usr/local/bin:/usr/bin:/bin",
        ],
    );
    push(
        &mut a,
        &["--setenv", "LANG", "C.UTF-8", "--setenv", "TMPDIR", "/tmp"],
    );
    let home_s = home
        .as_ref()
        .map(|h| h.to_string_lossy().to_string())
        .unwrap_or_else(|| "/tmp".to_string());
    a.extend(["--setenv".to_string(), "HOME".to_string(), home_s]);
    push(&mut a, &["--chdir", "/tmp"]);

    a.push("--".to_string());
    a.extend(inner.iter().cloned());
    a.push(inside);
    a
}

#[derive(Debug, Clone, Copy)]
enum Language {
    Python,
    JavaScript,
    Shell,
}

impl Language {
    fn extension(self) -> &'static str {
        match self {
            Language::Python => "py",
            Language::JavaScript => "js",
            Language::Shell => "sh",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_executor() -> CodeExecutor {
        let config = crate::config::MycelConfig::default();
        CodeExecutor::new(&config).unwrap()
    }

    fn executor_with(sandbox: SandboxConfig, timeout: u64) -> CodeExecutor {
        let base = std::env::temp_dir().join(format!("mycel-exec-test-{}", uuid::Uuid::new_v4()));
        let config = MycelConfig {
            sandbox,
            code_path: base.join("code").to_string_lossy().to_string(),
            context_path: base.join("data").to_string_lossy().to_string(),
            execution_timeout_secs: timeout,
            ..Default::default()
        };
        CodeExecutor::new(&config).unwrap()
    }

    /// Sandboxed executor, or None when bwrap isn't installed (skips test).
    fn sandboxed(timeout: u64) -> Option<CodeExecutor> {
        if bwrap_available("bwrap") {
            Some(executor_with(SandboxConfig::default(), timeout))
        } else {
            eprintln!("bwrap not installed; skipping sandbox integration test");
            None
        }
    }

    fn args_for(cfg: &SandboxConfig, home: &Option<PathBuf>) -> Vec<String> {
        bwrap_args(
            cfg,
            &["bash".to_string()],
            Path::new("/x/abc.sh"),
            home,
            &[],
        )
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
        assert!(matches!(
            executor.detect_language("ls -la"),
            Language::Shell
        ));
    }

    #[test]
    fn test_default_mode_is_sandboxed_without_network() {
        let cfg = SandboxConfig::default();
        assert_eq!(cfg.mode, SandboxMode::Bwrap);
        assert!(!cfg.allow_network);
    }

    #[test]
    fn test_bwrap_args_isolation_flags() {
        let args = args_for(
            &SandboxConfig::default(),
            &Some(PathBuf::from("/nonexistent-home")),
        );
        let joined = args.join(" ");
        for flag in [
            "--ro-bind / /",
            "--tmpfs /run",
            "--unshare-all",
            "--disable-userns",
            "--cap-drop ALL",
            "--die-with-parent",
            "--new-session",
            "--clearenv",
        ] {
            assert!(joined.contains(flag), "missing {flag}: {joined}");
        }
        assert!(!joined.contains("--share-net"));
        assert!(joined.ends_with("-- bash /run/mycel-exec/abc.sh"));
    }

    #[test]
    fn test_bwrap_args_network_opt_in() {
        let cfg = SandboxConfig {
            allow_network: true,
            ..Default::default()
        };
        assert!(args_for(&cfg, &None).iter().any(|a| a == "--share-net"));
    }

    #[test]
    fn test_bwrap_args_hides_existing_secrets() {
        let home = std::env::temp_dir().join(format!("mycel-home-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        std::fs::write(home.join(".netrc"), "machine x").unwrap();
        let args = args_for(&SandboxConfig::default(), &Some(home.clone()));
        let real = std::fs::canonicalize(&home).unwrap();
        let ssh = real.join(".ssh").to_string_lossy().to_string();
        let netrc = real.join(".netrc").to_string_lossy().to_string();
        assert!(args.windows(2).any(|w| w[0] == "--tmpfs" && w[1] == ssh));
        assert!(args
            .windows(3)
            .any(|w| w[0] == "--ro-bind" && w[1] == "/dev/null" && w[2] == netrc));
        // Paths that don't exist are not mounted (bwrap would fail on them).
        assert!(!args.iter().any(|a| a.ends_with("/.aws")));
        std::fs::remove_dir_all(home).ok();
    }

    #[test]
    fn test_inner_argv_limits() {
        let exec = test_executor();
        let py = exec.inner_argv(Language::Python).join(" ");
        assert!(py.contains("ulimit -c 0"));
        assert!(py.contains("ulimit -v 524288"), "{py}");
        assert!(py.ends_with("exec \"$@\" mycel-exec python3"));
        let js = exec.inner_argv(Language::JavaScript).join(" ");
        assert!(js.contains("--max-old-space-size=256"));
        assert!(!js.contains("ulimit -v"), "RLIMIT_AS breaks node: {js}");
    }

    #[tokio::test]
    async fn test_missing_bwrap_refuses_instead_of_host_fallback() {
        let exec = executor_with(
            SandboxConfig {
                bwrap_path: "/nonexistent/bwrap".to_string(),
                ..Default::default()
            },
            5,
        );
        let marker = std::env::temp_dir().join(format!("mycel-ran-{}", uuid::Uuid::new_v4()));
        let err = exec
            .run(&format!("touch {}", marker.display()))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("sandbox unavailable"));
        assert!(!marker.exists(), "code ran on the host");
    }

    #[tokio::test]
    async fn test_sandbox_runs_code() {
        let Some(exec) = sandboxed(10) else { return };
        assert_eq!(exec.run("echo hello").await.unwrap().trim(), "hello");
        assert_eq!(exec.run("print(6 * 7)").await.unwrap().trim(), "42");
        assert_eq!(exec.run("console.log(6 * 7)").await.unwrap().trim(), "42");
    }

    #[tokio::test]
    async fn test_sandbox_filesystem_is_read_only() {
        let Some(exec) = sandboxed(10) else { return };
        let target = dirs::home_dir()
            .unwrap()
            .join(format!("mycel-escape-{}", uuid::Uuid::new_v4()));
        let out = exec
            .run(&format!("touch {} && echo WROTE", target.display()))
            .await
            .unwrap();
        assert!(!out.contains("WROTE"), "write succeeded: {out}");
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn test_sandbox_has_no_network() {
        let Some(exec) = sandboxed(10) else { return };
        // Namespace view of interfaces: loopback only.
        let ifaces = exec
            .run("tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\\n' ' '")
            .await
            .unwrap();
        assert_eq!(ifaces.trim(), "lo", "interfaces: {ifaces}");
        // Real connections: external unreachable, host loopback services
        // (e.g. Ollama on 127.0.0.1) not reachable either.
        let out = exec
            .run(
                "import socket\n\
                 for host, port in [('1.1.1.1', 53), ('127.0.0.1', 11434)]:\n\
                 \x20   s = socket.socket(); s.settimeout(2)\n\
                 \x20   try:\n\
                 \x20       s.connect((host, port)); print('CONNECTED', host)\n\
                 \x20   except OSError:\n\
                 \x20       print('blocked', host)\n",
            )
            .await
            .unwrap();
        assert!(!out.contains("CONNECTED"), "network reachable: {out}");
        assert_eq!(out.matches("blocked").count(), 2, "{out}");
    }

    #[tokio::test]
    async fn test_sandbox_hides_host_interface_names() {
        let Some(exec) = sandboxed(10) else { return };
        let out = exec
            .run("ls /sys/class/net 2>/dev/null | wc -l")
            .await
            .unwrap();
        assert_eq!(out.trim(), "0", "host interfaces listed: {out}");
    }

    #[tokio::test]
    async fn test_sandbox_environment_is_clean() {
        let Some(exec) = sandboxed(10) else { return };
        // The daemon's environment (OPENROUTER_API_KEY etc.) must not leak.
        std::env::set_var("MYCEL_TEST_SECRET", "leaked");
        let out = exec
            .run("echo \"secret=${MYCEL_TEST_SECRET:-none}\"")
            .await
            .unwrap();
        assert_eq!(out.trim(), "secret=none");
    }

    #[tokio::test]
    async fn test_sandbox_hides_ssh_dir() {
        let Some(exec) = sandboxed(10) else { return };
        let ssh = dirs::home_dir().unwrap().join(".ssh");
        if !ssh.is_dir() {
            return;
        }
        let out = exec
            .run(&format!("ls -A {} | wc -l", ssh.display()))
            .await
            .unwrap();
        assert_eq!(out.trim(), "0", "~/.ssh visible: {out}");
    }

    #[tokio::test]
    async fn test_sandbox_hides_runtime_state() {
        let Some(exec) = sandboxed(10) else { return };
        let data = PathBuf::from(&exec.config.context_path);
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("device_key"), [7u8; 32]).unwrap();
        let out = exec
            .run(&format!("ls -A {} | wc -l", data.display()))
            .await
            .unwrap();
        assert_eq!(out.trim(), "0", "runtime state visible: {out}");
    }

    #[tokio::test]
    async fn test_python_memory_is_limited() {
        let Some(exec) = sandboxed(20) else { return };
        let out = exec
            .run("import sys\nb = bytearray(1 << 30)\nprint('ALLOCATED')")
            .await
            .unwrap();
        assert!(
            out.contains("MemoryError"),
            "1 GiB allocation allowed: {out}"
        );
    }

    #[tokio::test]
    async fn test_timeout_kills_process_tree() {
        let Some(exec) = sandboxed(1) else { return };
        let tag = format!(
            "mycelsleep{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        );
        let err = exec
            .run(&format!(
                "(exec -a {tag} sleep 60) & (exec -a {tag} sleep 61) & wait"
            ))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));
        tokio::time::sleep(Duration::from_millis(500)).await;
        let left = std::process::Command::new("pgrep")
            .args(["-c", &tag])
            .output()
            .unwrap();
        assert_eq!(
            String::from_utf8_lossy(&left.stdout).trim(),
            "0",
            "sandboxed children survived the timeout"
        );
    }

    #[tokio::test]
    async fn test_output_is_capped() {
        let Some(exec) = sandboxed(10) else { return };
        let out = exec
            .run("head -c 5000000 /dev/zero | tr '\\0' a")
            .await
            .unwrap();
        assert!(out.len() <= MAX_OUTPUT_BYTES + 32);
        assert!(out.ends_with("[output truncated]"));
    }

    #[tokio::test]
    async fn test_host_mode_runs_unsandboxed() {
        let exec = executor_with(
            SandboxConfig {
                mode: SandboxMode::Host,
                ..Default::default()
            },
            5,
        );
        assert_eq!(exec.run("echo host").await.unwrap().trim(), "host");
    }
}
