//! Kernel WireGuard dataplane manager using `ip` and `wg` CLI binaries.
//!
//! Avoids shell invocation (`sh -c`) and never passes private keys on the
//! command line or writes them to world-readable files.

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use std::net::{Ipv4Addr, SocketAddr};
use std::process::Stdio;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Derive a deterministic mesh IPv4 address in `10.100.<1..=254>.<1..=254>`
/// from a 32-byte x25519 public key.
pub fn derive_mesh_ip(public_key: &[u8; 32]) -> Ipv4Addr {
    let octet3 = (public_key[0] % 254) + 1;
    let octet4 = (public_key[1] % 254) + 1;
    Ipv4Addr::new(10, 100, octet3, octet4)
}

/// Validate a Linux network interface name: non-empty, <= 15 bytes, and
/// consisting solely of ASCII alphanumeric characters, `-`, or `_`.
pub fn validate_interface_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("WireGuard interface name cannot be empty"));
    }
    if name.len() > 15 {
        return Err(anyhow!(
            "WireGuard interface name '{}' exceeds 15-character Linux IFNAMSIZ limit",
            name
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(anyhow!(
            "WireGuard interface name '{}' contains invalid characters (allowed: ASCII alphanumeric, '-', '_')",
            name
        ));
    }
    Ok(())
}

/// Validate that a base64-encoded key decodes to 32 bytes.
fn validate_key_b64(key_b64: &str, label: &str) -> Result<()> {
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(key_b64.trim())
        .with_context(|| format!("Invalid base64 for {label}"))?;
    if decoded.len() != 32 {
        return Err(anyhow!(
            "Invalid {label} length: expected 32 bytes, got {}",
            decoded.len()
        ));
    }
    Ok(())
}

/// Build the argument vector for `wg set <iface> peer ...`.
pub fn build_set_peer_args(
    interface: &str,
    peer_pubkey_b64: &str,
    allowed_ip: Ipv4Addr,
    endpoint: Option<&str>,
) -> Result<Vec<String>> {
    validate_interface_name(interface)?;
    validate_key_b64(peer_pubkey_b64, "peer public key")?;

    let mut args = vec![
        "set".to_string(),
        interface.to_string(),
        "peer".to_string(),
        peer_pubkey_b64.trim().to_string(),
        "allowed-ips".to_string(),
        format!("{allowed_ip}/32"),
    ];

    if let Some(ep) = endpoint {
        let parsed: SocketAddr = ep
            .parse()
            .with_context(|| format!("Invalid peer endpoint SocketAddr: '{ep}'"))?;
        args.push("endpoint".to_string());
        args.push(parsed.to_string());
    }

    args.push("persistent-keepalive".to_string());
    args.push("25".to_string());

    Ok(args)
}

/// Kernel WireGuard interface manager.
#[derive(Debug, Clone)]
pub struct KernelWireGuard {
    interface: String,
    local_ip: Ipv4Addr,
}

impl KernelWireGuard {
    /// Create a new `KernelWireGuard` manager after validating `interface`.
    pub fn new(interface: String, local_ip: Ipv4Addr) -> Result<Self> {
        validate_interface_name(&interface)?;
        Ok(Self {
            interface,
            local_ip,
        })
    }

    pub fn interface(&self) -> &str {
        &self.interface
    }

    #[allow(dead_code)]
    pub fn local_ip(&self) -> Ipv4Addr {
        self.local_ip
    }

    /// Ensure required `ip` and `wg` binaries are runnable.
    async fn ensure_binaries(&self) -> Result<()> {
        let ip_out = Command::new("ip")
            .arg("-V")
            .output()
            .await
            .context("Failed to execute `ip -V` (is iproute2 installed?)")?;
        if !ip_out.status.success() {
            return Err(anyhow!("`ip -V` exited with status {}", ip_out.status));
        }

        let wg_out = Command::new("wg")
            .arg("--version")
            .output()
            .await
            .context("Failed to execute `wg --version` (is wireguard-tools installed?)")?;
        if !wg_out.status.success() {
            return Err(anyhow!(
                "`wg --version` exited with status {}",
                wg_out.status
            ));
        }

        Ok(())
    }

    /// Create (if needed), address, key, and bring up the WireGuard interface.
    pub async fn configure_interface(&self, private_key_b64: &str, listen_port: u16) -> Result<()> {
        validate_key_b64(private_key_b64, "private key")?;
        self.ensure_binaries().await?;

        // Create interface; tolerate already-exists errors.
        let add_out = Command::new("ip")
            .args(["link", "add", "dev", &self.interface, "type", "wireguard"])
            .output()
            .await
            .context("Failed to run `ip link add`")?;
        if !add_out.status.success() {
            let stderr = String::from_utf8_lossy(&add_out.stderr);
            if !stderr.contains("File exists") && !stderr.contains("exists") {
                return Err(anyhow!(
                    "`ip link add dev {} type wireguard` failed: {}",
                    self.interface,
                    stderr.trim()
                ));
            }
        }

        // Assign mesh IPv4 address.
        let cidr = format!("{}/16", self.local_ip);
        let addr_out = Command::new("ip")
            .args(["address", "replace", &cidr, "dev", &self.interface])
            .output()
            .await
            .context("Failed to run `ip address replace`")?;
        if !addr_out.status.success() {
            let stderr = String::from_utf8_lossy(&addr_out.stderr);
            return Err(anyhow!(
                "`ip address replace {cidr} dev {}` failed: {}",
                self.interface,
                stderr.trim()
            ));
        }

        // Configure listen-port and private-key via piped stdin (never CLI args or logs).
        let port_str = listen_port.to_string();
        let mut child = Command::new("wg")
            .args([
                "set",
                &self.interface,
                "listen-port",
                &port_str,
                "private-key",
                "/dev/stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("Failed to spawn `wg set`")?;

        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(private_key_b64.trim().as_bytes())
                .await
                .context("Failed to write private key to `wg set` stdin")?;
            stdin
                .write_all(b"\n")
                .await
                .context("Failed to terminate private key on `wg set` stdin")?;
            stdin
                .shutdown()
                .await
                .context("Failed to close `wg set` stdin")?;
        } else {
            return Err(anyhow!("`wg set` stdin handle was not available"));
        }

        let wg_out = child
            .wait_with_output()
            .await
            .context("Failed to wait for `wg set`")?;
        if !wg_out.status.success() {
            let stderr = String::from_utf8_lossy(&wg_out.stderr);
            return Err(anyhow!(
                "`wg set {}` failed: {}",
                self.interface,
                stderr.trim()
            ));
        }

        // Bring interface up.
        let up_out = Command::new("ip")
            .args(["link", "set", "up", "dev", &self.interface])
            .output()
            .await
            .context("Failed to run `ip link set up`")?;
        if !up_out.status.success() {
            let stderr = String::from_utf8_lossy(&up_out.stderr);
            return Err(anyhow!(
                "`ip link set up dev {}` failed: {}",
                self.interface,
                stderr.trim()
            ));
        }

        Ok(())
    }

    /// Add or update a WireGuard peer on this interface.
    pub async fn set_peer(
        &self,
        peer_pubkey_b64: &str,
        allowed_ip: Ipv4Addr,
        endpoint: Option<&str>,
    ) -> Result<()> {
        let args = build_set_peer_args(&self.interface, peer_pubkey_b64, allowed_ip, endpoint)?;
        let out = Command::new("wg")
            .args(&args)
            .output()
            .await
            .context("Failed to run `wg set peer`")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "`wg set {} peer` failed: {}",
                self.interface,
                stderr.trim()
            ));
        }
        Ok(())
    }

    /// Remove a WireGuard peer from this interface.
    #[allow(dead_code)]
    pub async fn remove_peer(&self, peer_pubkey_b64: &str) -> Result<()> {
        validate_key_b64(peer_pubkey_b64, "peer public key")?;
        let out = Command::new("wg")
            .args([
                "set",
                &self.interface,
                "peer",
                peer_pubkey_b64.trim(),
                "remove",
            ])
            .output()
            .await
            .context("Failed to run `wg set peer remove`")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "`wg set {} peer remove` failed: {}",
                self.interface,
                stderr.trim()
            ));
        }
        Ok(())
    }

    /// Delete the WireGuard interface, ignoring not-found errors.
    #[allow(dead_code)]
    pub async fn teardown(&self) -> Result<()> {
        let out = Command::new("ip")
            .args(["link", "del", "dev", &self.interface])
            .output()
            .await
            .context("Failed to run `ip link del`")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.contains("Cannot find device") && !stderr.contains("does not exist") {
                return Err(anyhow!(
                    "`ip link del dev {}` failed: {}",
                    self.interface,
                    stderr.trim()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_derive_mesh_ip_deterministic_and_in_range() {
        let pk1 = [0u8; 32];
        assert_eq!(derive_mesh_ip(&pk1), Ipv4Addr::new(10, 100, 1, 1));

        let mut pk2 = [255u8; 32];
        pk2[0] = 253;
        pk2[1] = 254;
        assert_eq!(derive_mesh_ip(&pk2), Ipv4Addr::new(10, 100, 254, 1));

        let mut pk3 = [0u8; 32];
        pk3[0] = 42;
        pk3[1] = 99;
        assert_eq!(derive_mesh_ip(&pk3), Ipv4Addr::new(10, 100, 43, 100));
    }

    #[test]
    fn test_validate_interface_name() {
        assert!(validate_interface_name("mycel0").is_ok());
        assert!(validate_interface_name("wg_mesh-1").is_ok());
        assert!(validate_interface_name("a23456789012345").is_ok());

        assert!(validate_interface_name("").is_err());
        assert!(validate_interface_name("a234567890123456").is_err());
        assert!(validate_interface_name("mycel 0").is_err());
        assert!(validate_interface_name("mycel;rm").is_err());
        assert!(validate_interface_name("mycel/0").is_err());
    }

    #[test]
    fn test_build_set_peer_args_valid() {
        let pubkey = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        let ip = Ipv4Addr::new(10, 100, 8, 8);

        let args_no_ep = build_set_peer_args("mycel0", &pubkey, ip, None).unwrap();
        assert_eq!(
            args_no_ep,
            vec![
                "set",
                "mycel0",
                "peer",
                &pubkey,
                "allowed-ips",
                "10.100.8.8/32",
                "persistent-keepalive",
                "25",
            ]
        );

        let args_with_ep =
            build_set_peer_args("mycel0", &pubkey, ip, Some("192.168.1.10:51820")).unwrap();
        assert_eq!(
            args_with_ep,
            vec![
                "set",
                "mycel0",
                "peer",
                &pubkey,
                "allowed-ips",
                "10.100.8.8/32",
                "endpoint",
                "192.168.1.10:51820",
                "persistent-keepalive",
                "25",
            ]
        );
    }

    #[test]
    fn test_build_set_peer_args_rejects_invalid_inputs() {
        let ip = Ipv4Addr::new(10, 100, 2, 3);
        let valid_pk = base64::engine::general_purpose::STANDARD.encode([1u8; 32]);

        // Bad interface
        assert!(build_set_peer_args("bad iface", &valid_pk, ip, None).is_err());
        // Bad base64
        assert!(build_set_peer_args("mycel0", "not-valid-base64!!!", ip, None).is_err());
        // Wrong decoded length
        let short_pk = base64::engine::general_purpose::STANDARD.encode([1u8; 16]);
        assert!(build_set_peer_args("mycel0", &short_pk, ip, None).is_err());
        // Bad endpoint
        assert!(build_set_peer_args("mycel0", &valid_pk, ip, Some("not-a-socket-addr")).is_err());
    }
}
