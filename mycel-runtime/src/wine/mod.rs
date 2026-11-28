//! Sandboxed, opt-in Wine compatibility layer for Mycel OS.
//!
//! Disabled by default (`[wine] enabled = false`). When enabled, Windows
//! `.exe` binaries run inside an isolated per-app `WINEPREFIX` wrapped in a
//! bubblewrap (`bwrap`) sandbox with:
//! - Read-only host root (`--ro-bind / /`), private `/tmp` and `/run`
//! - Network disabled by default (`--unshare-net` unless `allow_network = true`)
//! - Host credential and runtime-state directories hidden
//! - Writable bind mount restricted exclusively to the app's `WINEPREFIX`
//! - Cleared environment (`--clearenv`) with explicit `WINEPREFIX` and `HOME`
//! - Wall-clock timeout and 1 MiB per-stream output cap
//!
//! Note for Void Linux (x86_64-musl): Void musl does not package native
//! `wine` (which requires glibc/multilib). On Void musl hosts, users can
//! provide a Wine wrapper from Flatpak/Bottles, a glibc container/chroot, or
//! `box64`, and configure `[wine] wine_binary` accordingly.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::config::MycelConfig;

/// Maximum bytes captured from each of stdout and stderr (1 MiB).
const MAX_OUTPUT_BYTES: usize = 1024 * 1024;

/// Maximum length of a sanitized Windows app name.
const MAX_APP_NAME_LEN: usize = 64;

/// Timeout in seconds when probing `<wine> --version`.
const VERSION_PROBE_TIMEOUT_SECS: u64 = 5;

/// Helpful installation guidance shown on Void musl when Wine is unavailable.
const VOID_MUSL_WINE_GUIDANCE: &str = "Wine is not installed or not found in PATH. On Void Linux \
    (x86_64-musl), native `wine` requires glibc/multilib; use Flatpak/Bottles, a glibc \
    container/chroot, or `box64`, and set [wine] wine_binary if using a wrapper.";

/// Credential and session locations hidden from sandboxed Wine processes
/// (matching `CodeExecutor`). Directories are masked with an empty tmpfs and
/// files with `/dev/null`, relative to `$HOME`.
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

/// Absolute system paths hidden from sandboxed Wine processes.
const HIDDEN_SYSTEM_PATHS: &[&str] = &["/etc/mycel", "/var/lib/mycel", "/root", "/sys/class/net"];

/// Configuration for the opt-in Wine compatibility layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct WineConfig {
    /// Opt-in gate (default: `false`).
    pub enabled: bool,
    /// Wine binary name or path (default: `"wine"`).
    pub wine_binary: String,
    /// Root directory for per-app Wine prefixes (default: `<context_path>/wine-prefixes`).
    pub prefix_root: Option<String>,
    /// Wrap Wine execution inside bubblewrap (`bwrap`) (default: `true`).
    pub sandbox_with_bwrap: bool,
    /// Allow network access inside the Wine sandbox (default: `false`).
    pub allow_network: bool,
    /// Wall-clock execution timeout in seconds (default: `60`).
    pub timeout_secs: u64,
}

impl Default for WineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            wine_binary: "wine".to_string(),
            prefix_root: None,
            sandbox_with_bwrap: true,
            allow_network: false,
            timeout_secs: 60,
        }
    }
}

/// Diagnostic status for the Wine compatibility layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WineStatusInfo {
    pub enabled: bool,
    pub available: bool,
    pub wine_binary: String,
    pub resolved_binary: Option<String>,
    pub wine_version: Option<String>,
    pub prefix_root: String,
    pub sandbox_with_bwrap: bool,
    pub allow_network: bool,
    pub prefixes: Vec<String>,
    pub install_guidance: Option<String>,
}

/// Structured result from running a Windows `.exe` under Wine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WineRunResult {
    pub app_name: String,
    pub exe_path: String,
    pub prefix_path: String,
    pub sandboxed: bool,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub success: bool,
}

/// Service managing Wine detection, prefix isolation, and sandboxed execution.
#[derive(Clone)]
pub struct WineService {
    config: WineConfig,
    prefix_root: PathBuf,
    bwrap_path: String,
}

impl WineService {
    /// Construct a `WineService` from the full `MycelConfig`.
    pub fn new(config: &MycelConfig) -> Self {
        Self::from_parts(
            config.wine.clone(),
            &config.context_path,
            config.sandbox.bwrap_path.clone(),
        )
    }

    /// Construct a `WineService` from explicit parts.
    pub fn from_parts(wine: WineConfig, context_path: &str, bwrap_path: String) -> Self {
        let prefix_root = match wine.prefix_root.as_deref().map(str::trim) {
            Some(custom) if !custom.is_empty() => PathBuf::from(custom),
            _ => PathBuf::from(context_path).join("wine-prefixes"),
        };
        Self {
            config: wine,
            prefix_root,
            bwrap_path,
        }
    }

    /// Whether the Wine compatibility layer is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.enabled
    }

    /// Validate and normalize a Wine app name.
    ///
    /// Strictly allows `[a-zA-Z0-9_-]`, length `1..=64`, rejecting empty names,
    /// `.`, `..`, `/`, `\`, whitespace, and NUL bytes.
    pub fn sanitize_app_name(app_name: &str) -> Result<String> {
        if app_name.is_empty() {
            return Err(anyhow!("Wine app name cannot be empty"));
        }
        if app_name.len() > MAX_APP_NAME_LEN {
            return Err(anyhow!(
                "Wine app name exceeds maximum length of {} characters",
                MAX_APP_NAME_LEN
            ));
        }
        if !app_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(anyhow!(
                "Invalid Wine app name '{}': only [a-zA-Z0-9_-] are allowed",
                app_name
            ));
        }
        Ok(app_name.to_string())
    }

    /// Validate a Windows `.exe` path and return its canonicalized `PathBuf`.
    ///
    /// Rejects empty paths, NUL bytes, any `..` (`Component::ParentDir`),
    /// non-existent paths, directories, and non-`.exe` extensions.
    pub fn validate_exe_path(exe_path: &str) -> Result<PathBuf> {
        if exe_path.trim().is_empty() {
            return Err(anyhow!("Executable path cannot be empty"));
        }
        if exe_path.contains('\0') {
            return Err(anyhow!("Executable path contains NUL byte"));
        }

        let raw = Path::new(exe_path);
        for component in raw.components() {
            if matches!(component, Component::ParentDir) {
                return Err(anyhow!(
                    "Executable path '{}' must not contain '..' traversal segments",
                    exe_path
                ));
            }
        }

        if !has_exe_extension(raw) {
            return Err(anyhow!(
                "Executable path '{}' must have a .exe extension",
                exe_path
            ));
        }

        let canonical = std::fs::canonicalize(raw)
            .with_context(|| format!("Executable path '{}' does not exist", exe_path))?;

        if !canonical.is_file() {
            return Err(anyhow!(
                "Executable path '{}' is not a regular file",
                canonical.display()
            ));
        }

        if !has_exe_extension(&canonical) {
            return Err(anyhow!(
                "Resolved executable '{}' does not have a .exe extension",
                canonical.display()
            ));
        }

        Ok(canonical)
    }

    /// Resolve the isolated `WINEPREFIX` directory for `app_name` under `prefix_root`.
    pub fn prefix_dir_for(&self, app_name: &str) -> Result<PathBuf> {
        let sanitized = Self::sanitize_app_name(app_name)?;
        let candidate = self.prefix_root.join(&sanitized);
        if !candidate.starts_with(&self.prefix_root)
            || candidate.parent() != Some(self.prefix_root.as_path())
        {
            return Err(anyhow!(
                "Resolved Wine prefix '{}' escapes prefix root '{}'",
                candidate.display(),
                self.prefix_root.display()
            ));
        }
        Ok(candidate)
    }

    /// Search `PATH` (or direct executable path) for the configured `wine_binary`,
    /// falling back to `"wine64"` and `"wine"`.
    pub fn detect_wine_binary(&self) -> Option<PathBuf> {
        let mut candidates: Vec<&str> = Vec::new();
        let configured = self.config.wine_binary.trim();
        if !configured.is_empty() {
            candidates.push(configured);
        }
        for fallback in ["wine64", "wine"] {
            if !candidates.contains(&fallback) {
                candidates.push(fallback);
            }
        }

        for candidate in candidates {
            if let Some(found) = resolve_executable(candidate) {
                return Some(found);
            }
        }
        None
    }

    /// Return current Wine availability, version, existing prefixes, and
    /// Void musl installation guidance when unavailable.
    pub async fn status(&self) -> WineStatusInfo {
        let resolved = self.detect_wine_binary();
        let available = resolved.is_some();
        let wine_version = match resolved.as_ref() {
            Some(bin) => probe_wine_version(bin).await,
            None => None,
        };

        let prefixes = list_prefix_dirs(&self.prefix_root);
        let install_guidance = if available {
            None
        } else {
            Some(VOID_MUSL_WINE_GUIDANCE.to_string())
        };

        WineStatusInfo {
            enabled: self.config.enabled,
            available,
            wine_binary: self.config.wine_binary.clone(),
            resolved_binary: resolved.map(|p| p.to_string_lossy().into_owned()),
            wine_version,
            prefix_root: self.prefix_root.to_string_lossy().into_owned(),
            sandbox_with_bwrap: self.config.sandbox_with_bwrap,
            allow_network: self.config.allow_network,
            prefixes,
            install_guidance,
        }
    }

    /// Build the `bwrap` argument list for running a Windows `.exe` under Wine.
    pub fn build_bwrap_args(
        &self,
        prefix_dir: &Path,
        exe_path: &Path,
        wine_bin: &Path,
        args: &[String],
    ) -> Vec<String> {
        let home = dirs::home_dir();
        let display = std::env::var("DISPLAY").ok();
        let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
        self.build_bwrap_args_with_env(
            prefix_dir,
            exe_path,
            wine_bin,
            args,
            home.as_deref(),
            display.as_deref(),
            wayland_display.as_deref(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build_bwrap_args_with_env(
        &self,
        prefix_dir: &Path,
        exe_path: &Path,
        wine_bin: &Path,
        args: &[String],
        home: Option<&Path>,
        display: Option<&str>,
        wayland_display: Option<&str>,
    ) -> Vec<String> {
        let mut a: Vec<String> = Vec::new();

        a.extend([
            "--die-with-parent".to_string(),
            "--new-session".to_string(),
            "--unshare-pid".to_string(),
            "--unshare-ipc".to_string(),
            "--unshare-uts".to_string(),
        ]);
        if !self.config.allow_network {
            a.push("--unshare-net".to_string());
        }

        // Read-only host root, fresh /dev and /proc, private /tmp and /run.
        a.extend([
            "--ro-bind".to_string(),
            "/".to_string(),
            "/".to_string(),
            "--dev".to_string(),
            "/dev".to_string(),
            "--proc".to_string(),
            "/proc".to_string(),
            "--tmpfs".to_string(),
            "/tmp".to_string(),
            "--tmpfs".to_string(),
            "/run".to_string(),
        ]);

        // Writable bind mount ONLY for this app's isolated WINEPREFIX.
        let prefix_str = prefix_dir.to_string_lossy().into_owned();
        a.extend(["--bind".to_string(), prefix_str.clone(), prefix_str.clone()]);

        // Read-only bind mount for the target .exe.
        let exe_str = exe_path.to_string_lossy().into_owned();
        a.extend(["--ro-bind".to_string(), exe_str.clone(), exe_str.clone()]);

        // Hide host credential and runtime-state locations (applied after
        // writable prefix bind so masks always take precedence).
        let mut hidden: Vec<PathBuf> = HIDDEN_SYSTEM_PATHS.iter().map(PathBuf::from).collect();
        if let Some(h) = home {
            hidden.extend(HIDDEN_HOME_PATHS.iter().map(|p| h.join(p)));
        }

        let mut seen = HashSet::new();
        for p in hidden {
            let Ok(real) = std::fs::canonicalize(&p) else {
                continue;
            };
            if real == Path::new("/") || !seen.insert(real.clone()) {
                continue;
            }
            let s = real.to_string_lossy().into_owned();
            if real.is_dir() {
                a.extend(["--tmpfs".to_string(), s]);
            } else {
                a.extend(["--ro-bind".to_string(), "/dev/null".to_string(), s]);
            }
        }

        // Cleared environment with explicit Wine variables.
        a.extend([
            "--clearenv".to_string(),
            "--setenv".to_string(),
            "WINEPREFIX".to_string(),
            prefix_str.clone(),
            "--setenv".to_string(),
            "WINEDEBUG".to_string(),
            "-all".to_string(),
            "--setenv".to_string(),
            "PATH".to_string(),
            "/usr/local/bin:/usr/bin:/bin".to_string(),
            "--setenv".to_string(),
            "HOME".to_string(),
            prefix_str,
        ]);

        if let Some(disp) = display.filter(|v| !v.is_empty()) {
            a.extend([
                "--setenv".to_string(),
                "DISPLAY".to_string(),
                disp.to_string(),
            ]);
        }
        if let Some(wayland) = wayland_display.filter(|v| !v.is_empty()) {
            a.extend([
                "--setenv".to_string(),
                "WAYLAND_DISPLAY".to_string(),
                wayland.to_string(),
            ]);
        }

        a.push("--".to_string());
        a.push(wine_bin.to_string_lossy().into_owned());
        a.push(exe_str);
        a.extend(args.iter().cloned());

        a
    }

    /// Run a Windows `.exe` under an isolated per-app `WINEPREFIX`.
    pub async fn run_windows_app(
        &self,
        app_name: &str,
        exe_path: &str,
        args: &[String],
    ) -> Result<WineRunResult> {
        if !self.config.enabled {
            return Err(anyhow!(
                "Wine compatibility layer is disabled (set [wine] enabled = true in config to opt in)"
            ));
        }

        let sanitized_name = Self::sanitize_app_name(app_name)?;
        let validated_exe = Self::validate_exe_path(exe_path)?;
        let prefix_dir = self.prefix_dir_for(&sanitized_name)?;

        let wine_bin = self.detect_wine_binary().ok_or_else(|| {
            anyhow!(
                "Wine binary '{}' not found. {}",
                self.config.wine_binary,
                VOID_MUSL_WINE_GUIDANCE
            )
        })?;

        if self.config.sandbox_with_bwrap && !bwrap_available(&self.bwrap_path) {
            return Err(anyhow!(
                "Sandbox unavailable: bubblewrap not found at '{}'. Refusing to run Wine \
                 unsandboxed on the host. Install bubblewrap or set [wine] sandbox_with_bwrap = false.",
                self.bwrap_path
            ));
        }

        ensure_prefix_dir(&prefix_dir)?;

        let mut cmd = if self.config.sandbox_with_bwrap {
            let bwrap_args = self.build_bwrap_args(&prefix_dir, &validated_exe, &wine_bin, args);
            let mut c = Command::new(&self.bwrap_path);
            c.args(&bwrap_args);
            c
        } else {
            warn!(
                app = %sanitized_name,
                "Running Wine app without bubblewrap sandbox (sandbox_with_bwrap = false)"
            );
            let mut c = Command::new(&wine_bin);
            c.arg(&validated_exe)
                .args(args)
                .env("WINEPREFIX", &prefix_dir)
                .env("WINEDEBUG", "-all")
                .env("HOME", &prefix_dir);
            c
        };

        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0)
            .kill_on_drop(true);

        info!(
            app = %sanitized_name,
            exe = %validated_exe.display(),
            sandboxed = self.config.sandbox_with_bwrap,
            "Launching Windows executable via Wine"
        );

        let mut child = cmd
            .spawn()
            .with_context(|| format!("Failed to spawn Wine process for '{}'", sanitized_name))?;
        let pgid = child.id();
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to capture Wine stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to capture Wine stderr"))?;

        let timeout_secs = self.config.timeout_secs.max(1);
        let run = async {
            tokio::join!(
                read_capped(&mut stdout),
                read_capped(&mut stderr),
                child.wait()
            )
        };

        let exe_display = validated_exe.to_string_lossy().into_owned();
        let prefix_display = prefix_dir.to_string_lossy().into_owned();

        let (out, err, status) =
            match tokio::time::timeout(Duration::from_secs(timeout_secs), run).await {
                Ok(r) => r,
                Err(_) => {
                    if let Some(pgid) = pgid {
                        kill_group(pgid);
                    }
                    return Ok(WineRunResult {
                        app_name: sanitized_name,
                        exe_path: exe_display,
                        prefix_path: prefix_display,
                        sandboxed: self.config.sandbox_with_bwrap,
                        exit_code: None,
                        timed_out: true,
                        stdout: String::new(),
                        stderr: format!("Wine execution timed out after {} seconds", timeout_secs),
                        success: false,
                    });
                }
            };

        let status = status?;
        let stdout_text = out?;
        let stderr_text = err?;
        let exit_code = status.code();
        let success = status.success();

        Ok(WineRunResult {
            app_name: sanitized_name,
            exe_path: exe_display,
            prefix_path: prefix_display,
            sandboxed: self.config.sandbox_with_bwrap,
            exit_code,
            timed_out: false,
            stdout: stdout_text,
            stderr: stderr_text,
            success,
        })
    }
}

fn has_exe_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("exe"))
        .unwrap_or(false)
}

fn is_executable_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            return meta.permissions().mode() & 0o111 != 0;
        }
        false
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn resolve_executable(candidate: &str) -> Option<PathBuf> {
    let candidate_path = Path::new(candidate);
    if candidate.contains('/') || candidate.contains(std::path::MAIN_SEPARATOR) {
        return is_executable_file(candidate_path).then(|| candidate_path.to_path_buf());
    }

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let full = dir.join(candidate);
        if is_executable_file(&full) {
            return Some(full);
        }
    }
    None
}

async fn probe_wine_version(wine_bin: &Path) -> Option<String> {
    let mut cmd = Command::new(wine_bin);
    cmd.arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let output = tokio::time::timeout(
        Duration::from_secs(VERSION_PROBE_TIMEOUT_SECS),
        cmd.output(),
    )
    .await
    .ok()?
    .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let line = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    Some(line.to_string())
}

fn list_prefix_dirs(prefix_root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(prefix_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}

fn ensure_prefix_dir(prefix_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(prefix_dir).with_context(|| {
        format!(
            "Failed to create Wine prefix directory '{}'",
            prefix_dir.display()
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(prefix_dir, std::fs::Permissions::from_mode(0o700)).with_context(
            || {
                format!(
                    "Failed to set 0700 permissions on Wine prefix '{}'",
                    prefix_dir.display()
                )
            },
        )?;
    }
    Ok(())
}

fn bwrap_available(path: &str) -> bool {
    std::process::Command::new(path)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> Result<String> {
    let mut kept = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        let room = MAX_OUTPUT_BYTES.saturating_sub(kept.len());
        kept.extend_from_slice(&buf[..n.min(room)]);
    }
    Ok(String::from_utf8_lossy(&kept).into_owned())
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_service(cfg: WineConfig, context_path: &str, bwrap_path: &str) -> WineService {
        WineService::from_parts(cfg, context_path, bwrap_path.to_string())
    }

    #[test]
    fn test_wine_config_safe_defaults() {
        let cfg = WineConfig::default();
        assert!(!cfg.enabled);
        assert_eq!(cfg.wine_binary, "wine");
        assert!(cfg.prefix_root.is_none());
        assert!(cfg.sandbox_with_bwrap);
        assert!(!cfg.allow_network);
        assert_eq!(cfg.timeout_secs, 60);

        let parsed: WineConfig = toml::from_str("").expect("empty TOML should parse to defaults");
        assert_eq!(parsed, cfg);
    }

    #[test]
    fn test_sanitize_app_name() {
        assert_eq!(
            WineService::sanitize_app_name("notepad_1-test").expect("valid app name"),
            "notepad_1-test"
        );
        assert_eq!(
            WineService::sanitize_app_name("A9").expect("valid short app name"),
            "A9"
        );

        let max_len = "a".repeat(64);
        assert_eq!(
            WineService::sanitize_app_name(&max_len).expect("64-char name should succeed"),
            max_len
        );

        let too_long = "a".repeat(65);
        for invalid in [
            "",
            ".",
            "..",
            "../evil",
            "a/b",
            "a\\b",
            "a b",
            "app\0name",
            too_long.as_str(),
        ] {
            assert!(
                WineService::sanitize_app_name(invalid).is_err(),
                "expected '{}' to be rejected",
                invalid.escape_default()
            );
        }
    }

    #[test]
    fn test_validate_exe_path() {
        let temp_dir =
            std::env::temp_dir().join(format!("mycel-wine-exe-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");

        let valid_exe = temp_dir.join("Setup.EXE");
        std::fs::write(&valid_exe, b"MZ").expect("write dummy exe");
        let non_exe = temp_dir.join("notes.txt");
        std::fs::write(&non_exe, b"hello").expect("write non-exe");

        // Valid .EXE (case-insensitive)
        let canonical =
            WineService::validate_exe_path(&valid_exe.to_string_lossy()).expect("valid .EXE");
        assert!(canonical.is_file());

        // Traversal rejected even if it resolves to an existing file
        let traversal = format!(
            "{}/../{}/Setup.EXE",
            temp_dir.display(),
            temp_dir.file_name().expect("dir name").to_string_lossy()
        );
        assert!(WineService::validate_exe_path(&traversal).is_err());

        // Missing file, directory, non-exe, empty, NUL all rejected
        assert!(
            WineService::validate_exe_path(&temp_dir.join("missing.exe").to_string_lossy())
                .is_err()
        );
        assert!(WineService::validate_exe_path(&temp_dir.to_string_lossy()).is_err());
        assert!(WineService::validate_exe_path(&non_exe.to_string_lossy()).is_err());
        assert!(WineService::validate_exe_path("").is_err());
        assert!(WineService::validate_exe_path("app\0.exe").is_err());

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_prefix_dir_for_stays_inside_root() {
        let svc = test_service(WineConfig::default(), "/var/lib/mycel-ctx", "bwrap");
        let dir = svc.prefix_dir_for("my-app_1").expect("valid prefix dir");
        assert_eq!(
            dir,
            PathBuf::from("/var/lib/mycel-ctx/wine-prefixes/my-app_1")
        );
        assert!(svc.prefix_dir_for("../escape").is_err());
    }

    #[test]
    fn test_build_bwrap_args_isolation_and_network_toggle() {
        let temp_dir =
            std::env::temp_dir().join(format!("mycel-wine-bwrap-test-{}", uuid::Uuid::new_v4()));
        let home = temp_dir.join("home");
        let ssh_dir = home.join(".ssh");
        let netrc = home.join(".netrc");
        let prefix = temp_dir.join("prefixes/demo");
        let exe = temp_dir.join("app.exe");
        std::fs::create_dir_all(&ssh_dir).expect("create ssh dir");
        std::fs::create_dir_all(&prefix).expect("create prefix dir");
        std::fs::write(&netrc, b"secret").expect("write netrc");
        std::fs::write(&exe, b"MZ").expect("write exe");

        let wine_bin = Path::new("/usr/bin/wine");
        let app_args = vec!["--silent".to_string()];

        // Network off by default
        let svc_no_net = test_service(WineConfig::default(), "/tmp/ctx", "bwrap");
        let args_no_net = svc_no_net.build_bwrap_args_with_env(
            &prefix,
            &exe,
            wine_bin,
            &app_args,
            Some(&home),
            Some(":0"),
            None,
        );

        assert!(args_no_net.contains(&"--unshare-net".to_string()));
        assert!(args_no_net.contains(&"--clearenv".to_string()));
        assert!(args_no_net.contains(&"--die-with-parent".to_string()));
        assert!(args_no_net.contains(&"--new-session".to_string()));
        assert!(args_no_net.contains(&"--unshare-pid".to_string()));
        assert!(args_no_net.contains(&"--unshare-ipc".to_string()));
        assert!(args_no_net.contains(&"--unshare-uts".to_string()));

        // Verify --ro-bind / /
        assert!(args_no_net.windows(3).any(|w| w == ["--ro-bind", "/", "/"]));

        // Verify writable --bind ONLY on prefix
        let bind_targets: Vec<&str> = args_no_net
            .windows(2)
            .filter_map(|w| (w[0] == "--bind").then_some(w[1].as_str()))
            .collect();
        assert_eq!(bind_targets, vec![prefix.to_string_lossy().as_ref()]);

        // Verify credential hiding
        let ssh_real = std::fs::canonicalize(&ssh_dir).expect("canonicalize ssh");
        let netrc_real = std::fs::canonicalize(&netrc).expect("canonicalize netrc");
        assert!(args_no_net
            .windows(2)
            .any(|w| w == ["--tmpfs", ssh_real.to_string_lossy().as_ref()]));
        assert!(args_no_net.windows(3).any(|w| {
            w == [
                "--ro-bind",
                "/dev/null",
                netrc_real.to_string_lossy().as_ref(),
            ]
        }));

        // Verify WINEPREFIX and DISPLAY setenv, and trailing command
        assert!(args_no_net
            .windows(3)
            .any(|w| { w == ["--setenv", "WINEPREFIX", prefix.to_string_lossy().as_ref()] }));
        assert!(args_no_net
            .windows(3)
            .any(|w| w == ["--setenv", "DISPLAY", ":0"]));
        assert!(args_no_net.windows(4).any(|w| {
            w == [
                "--",
                "/usr/bin/wine",
                exe.to_string_lossy().as_ref(),
                "--silent",
            ]
        }));

        // Network opt-in removes --unshare-net
        let net_cfg = WineConfig {
            allow_network: true,
            ..WineConfig::default()
        };
        let svc_net = test_service(net_cfg, "/tmp/ctx", "bwrap");
        let args_net = svc_net.build_bwrap_args_with_env(
            &prefix,
            &exe,
            wine_bin,
            &[],
            Some(&home),
            None,
            None,
        );
        assert!(!args_net.contains(&"--unshare-net".to_string()));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn test_disabled_by_default_refuses_execution() {
        let svc = test_service(WineConfig::default(), "/tmp/ctx", "bwrap");
        assert!(!svc.is_enabled());
        let err = svc
            .run_windows_app("notepad", "/tmp/notepad.exe", &[])
            .await
            .expect_err("disabled WineService must refuse execution");
        assert!(err.to_string().contains("disabled"));
    }

    #[tokio::test]
    async fn test_missing_wine_binary_refuses_with_guidance() {
        let temp_dir =
            std::env::temp_dir().join(format!("mycel-wine-missing-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp_dir).expect("create temp dir");
        let exe = temp_dir.join("app.exe");
        std::fs::write(&exe, b"MZ").expect("write exe");

        let cfg = WineConfig {
            enabled: true,
            wine_binary: "/nonexistent/wine-binary-xyz".to_string(),
            prefix_root: Some(temp_dir.join("prefixes").to_string_lossy().into_owned()),
            ..WineConfig::default()
        };
        let svc = test_service(cfg, &temp_dir.to_string_lossy(), "bwrap");

        // Only run the missing-binary assertion if neither wine64 nor wine is
        // installed on the host PATH, or test status guidance directly.
        if svc.detect_wine_binary().is_none() {
            let status = svc.status().await;
            assert!(!status.available);
            assert!(status
                .install_guidance
                .as_deref()
                .unwrap_or("")
                .contains("Flatpak/Bottles"));

            let err = svc
                .run_windows_app("testapp", &exe.to_string_lossy(), &[])
                .await
                .expect_err("missing wine binary must refuse");
            assert!(err.to_string().contains("Flatpak/Bottles"));
        }

        let _ = std::fs::remove_dir_all(&temp_dir);
    }
}
