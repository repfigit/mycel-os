//! Pure-Rust RFC 5389 STUN client over UDP for NAT traversal and public endpoint discovery.

use anyhow::{anyhow, Context, Result};
use rand::RngCore;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};
use std::time::Duration;
use tokio::net::UdpSocket;
use tracing::{debug, warn};

/// STUN Binding Request message type (RFC 5389).
pub const STUN_BINDING_REQUEST: u16 = 0x0001;
/// STUN Binding Success Response message type (RFC 5389).
pub const STUN_BINDING_SUCCESS: u16 = 0x0101;
/// STUN Magic Cookie (RFC 5389).
pub const STUN_MAGIC_COOKIE: u32 = 0x2112_A442;
/// STUN MAPPED-ADDRESS attribute type.
pub const ATTR_MAPPED_ADDRESS: u16 = 0x0001;
/// STUN XOR-MAPPED-ADDRESS attribute type.
pub const ATTR_XOR_MAPPED_ADDRESS: u16 = 0x0020;

const STUN_HEADER_LEN: usize = 20;
const FAMILY_IPV4: u8 = 0x01;
const FAMILY_IPV6: u8 = 0x02;

/// Build a 20-byte RFC 5389 STUN Binding Request packet for the given 12-byte transaction ID.
pub fn build_binding_request(tx_id: [u8; 12]) -> [u8; 20] {
    let mut packet = [0u8; STUN_HEADER_LEN];
    let msg_type = STUN_BINDING_REQUEST.to_be_bytes();
    let msg_len = 0u16.to_be_bytes();
    let cookie = STUN_MAGIC_COOKIE.to_be_bytes();

    packet[0] = msg_type[0];
    packet[1] = msg_type[1];
    packet[2] = msg_len[0];
    packet[3] = msg_len[1];
    packet[4] = cookie[0];
    packet[5] = cookie[1];
    packet[6] = cookie[2];
    packet[7] = cookie[3];
    packet[8..20].copy_from_slice(&tx_id);

    packet
}

/// Parse an RFC 5389 STUN Binding Success Response and extract the mapped `SocketAddr`.
///
/// Prefers `XOR-MAPPED-ADDRESS` and falls back to `MAPPED-ADDRESS`.
pub fn parse_binding_response(packet: &[u8], expected_tx_id: &[u8; 12]) -> Result<SocketAddr> {
    if packet.len() < STUN_HEADER_LEN {
        return Err(anyhow!(
            "STUN packet too short: {} bytes (minimum {})",
            packet.len(),
            STUN_HEADER_LEN
        ));
    }

    let msg_type = u16::from_be_bytes([packet[0], packet[1]]);
    if msg_type != STUN_BINDING_SUCCESS {
        return Err(anyhow!(
            "Unexpected STUN message type: 0x{msg_type:04x} (expected 0x{STUN_BINDING_SUCCESS:04x})"
        ));
    }

    let msg_len = u16::from_be_bytes([packet[2], packet[3]]) as usize;
    let total_len = STUN_HEADER_LEN + msg_len;
    if total_len > packet.len() {
        return Err(anyhow!(
            "STUN message length {msg_len} exceeds packet buffer ({} bytes)",
            packet.len()
        ));
    }

    let cookie = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
    if cookie != STUN_MAGIC_COOKIE {
        return Err(anyhow!(
            "Invalid STUN magic cookie: 0x{cookie:08x} (expected 0x{STUN_MAGIC_COOKIE:08x})"
        ));
    }

    if &packet[8..20] != expected_tx_id {
        return Err(anyhow!("STUN transaction ID mismatch"));
    }

    let mut xor_mapped: Option<SocketAddr> = None;
    let mut mapped: Option<SocketAddr> = None;
    let mut offset = STUN_HEADER_LEN;

    while offset + 4 <= total_len {
        let attr_type = u16::from_be_bytes([packet[offset], packet[offset + 1]]);
        let attr_len = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
        let value_start = offset + 4;
        let value_end = value_start + attr_len;

        if value_end > total_len {
            return Err(anyhow!(
                "STUN attribute 0x{attr_type:04x} length {attr_len} exceeds message bounds"
            ));
        }

        let value = &packet[value_start..value_end];
        match attr_type {
            ATTR_XOR_MAPPED_ADDRESS if xor_mapped.is_none() => {
                xor_mapped = Some(parse_xor_mapped_address(value, expected_tx_id)?);
            }
            ATTR_MAPPED_ADDRESS if mapped.is_none() => {
                mapped = Some(parse_mapped_address(value)?);
            }
            _ => {}
        }

        // STUN attributes are padded to 4-byte boundaries.
        let padded_len = (attr_len + 3) & !3;
        offset = value_start + padded_len;
    }

    xor_mapped
        .or(mapped)
        .ok_or_else(|| anyhow!("STUN response contained no mapped address attribute"))
}

fn parse_mapped_address(value: &[u8]) -> Result<SocketAddr> {
    if value.len() < 4 {
        return Err(anyhow!("MAPPED-ADDRESS attribute too short"));
    }
    let family = value[1];
    let port = u16::from_be_bytes([value[2], value[3]]);

    match family {
        FAMILY_IPV4 => {
            if value.len() < 8 {
                return Err(anyhow!(
                    "IPv4 MAPPED-ADDRESS too short: {} bytes",
                    value.len()
                ));
            }
            let ip = Ipv4Addr::new(value[4], value[5], value[6], value[7]);
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        FAMILY_IPV6 => {
            if value.len() < 20 {
                return Err(anyhow!(
                    "IPv6 MAPPED-ADDRESS too short: {} bytes",
                    value.len()
                ));
            }
            let mut octets = [0u8; 16];
            octets.copy_from_slice(&value[4..20]);
            let ip = Ipv6Addr::from(octets);
            Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
        }
        other => Err(anyhow!("Unsupported STUN address family: 0x{other:02x}")),
    }
}

fn parse_xor_mapped_address(value: &[u8], tx_id: &[u8; 12]) -> Result<SocketAddr> {
    if value.len() < 4 {
        return Err(anyhow!("XOR-MAPPED-ADDRESS attribute too short"));
    }
    let family = value[1];
    let x_port = u16::from_be_bytes([value[2], value[3]]);
    let port = x_port ^ ((STUN_MAGIC_COOKIE >> 16) as u16);
    let cookie_bytes = STUN_MAGIC_COOKIE.to_be_bytes();

    match family {
        FAMILY_IPV4 => {
            if value.len() < 8 {
                return Err(anyhow!(
                    "IPv4 XOR-MAPPED-ADDRESS too short: {} bytes",
                    value.len()
                ));
            }
            let ip = Ipv4Addr::new(
                value[4] ^ cookie_bytes[0],
                value[5] ^ cookie_bytes[1],
                value[6] ^ cookie_bytes[2],
                value[7] ^ cookie_bytes[3],
            );
            Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
        }
        FAMILY_IPV6 => {
            if value.len() < 20 {
                return Err(anyhow!(
                    "IPv6 XOR-MAPPED-ADDRESS too short: {} bytes",
                    value.len()
                ));
            }
            let mut octets = [0u8; 16];
            for i in 0..4 {
                octets[i] = value[4 + i] ^ cookie_bytes[i];
            }
            for i in 0..12 {
                octets[4 + i] = value[8 + i] ^ tx_id[i];
            }
            let ip = Ipv6Addr::from(octets);
            Ok(SocketAddr::V6(SocketAddrV6::new(ip, port, 0, 0)))
        }
        other => Err(anyhow!("Unsupported STUN address family: 0x{other:02x}")),
    }
}

/// Send a STUN Binding Request over `socket` to `server` and wait up to `timeout`
/// for a matching Binding Success Response.
pub async fn discover_via_server(
    socket: &UdpSocket,
    server: SocketAddr,
    timeout: Duration,
) -> Result<SocketAddr> {
    let mut tx_id = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut tx_id);
    let request = build_binding_request(tx_id);

    socket
        .send_to(&request, server)
        .await
        .with_context(|| format!("Failed to send STUN binding request to {server}"))?;

    let mut buf = [0u8; 1024];
    let result = tokio::time::timeout(timeout, async {
        loop {
            let (n, _src) = socket
                .recv_from(&mut buf)
                .await
                .context("Failed to receive STUN response")?;
            match parse_binding_response(&buf[..n], &tx_id) {
                Ok(addr) => return Ok(addr),
                Err(err) => {
                    debug!("Ignoring non-matching UDP packet while waiting for STUN from {server}: {err}");
                }
            }
        }
    })
    .await;

    match result {
        Ok(inner) => inner,
        Err(_) => Err(anyhow!(
            "STUN binding request to {server} timed out after {:?}",
            timeout
        )),
    }
}

/// Discover this node's public `SocketAddr` by querying `stun_servers` in order
/// with a per-server `timeout`, returning the first discovered address.
pub async fn discover_public_endpoint(
    local_bind: SocketAddr,
    stun_servers: &[String],
    timeout: Duration,
) -> Result<SocketAddr> {
    if stun_servers.is_empty() {
        return Err(anyhow!("No STUN servers configured for NAT discovery"));
    }

    let socket = UdpSocket::bind(local_bind)
        .await
        .with_context(|| format!("Failed to bind UDP socket at {local_bind}"))?;

    let mut last_err: Option<anyhow::Error> = None;

    for server_str in stun_servers {
        let resolved =
            match tokio::time::timeout(timeout, tokio::net::lookup_host(server_str)).await {
                Ok(Ok(addrs)) => addrs.collect::<Vec<_>>(),
                Ok(Err(err)) => {
                    warn!("Failed to resolve STUN server '{server_str}': {err}");
                    last_err = Some(err.into());
                    continue;
                }
                Err(_) => {
                    warn!("DNS lookup timed out for STUN server '{server_str}'");
                    last_err = Some(anyhow!("DNS lookup timed out for '{server_str}'"));
                    continue;
                }
            };

        for addr in resolved {
            match discover_via_server(&socket, addr, timeout).await {
                Ok(public_addr) => return Ok(public_addr),
                Err(err) => {
                    debug!("STUN discovery via {server_str} ({addr}) failed: {err}");
                    last_err = Some(err);
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| anyhow!("Failed to discover public endpoint via STUN")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::IpAddr;

    fn build_mock_stun_response(tx_id: &[u8; 12], attrs: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut body = Vec::new();
        for (attr_type, value) in attrs {
            body.extend_from_slice(&attr_type.to_be_bytes());
            body.extend_from_slice(&(value.len() as u16).to_be_bytes());
            body.extend_from_slice(value);
            let pad = (4 - (value.len() % 4)) % 4;
            body.extend(std::iter::repeat_n(0u8, pad));
        }

        let mut pkt = Vec::with_capacity(STUN_HEADER_LEN + body.len());
        pkt.extend_from_slice(&STUN_BINDING_SUCCESS.to_be_bytes());
        pkt.extend_from_slice(&(body.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&STUN_MAGIC_COOKIE.to_be_bytes());
        pkt.extend_from_slice(tx_id);
        pkt.extend_from_slice(&body);
        pkt
    }

    fn encode_xor_mapped_v4(ip: Ipv4Addr, port: u16) -> Vec<u8> {
        let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
        let x_port = (port ^ ((STUN_MAGIC_COOKIE >> 16) as u16)).to_be_bytes();
        let octets = ip.octets();
        vec![
            0x00,
            FAMILY_IPV4,
            x_port[0],
            x_port[1],
            octets[0] ^ cookie[0],
            octets[1] ^ cookie[1],
            octets[2] ^ cookie[2],
            octets[3] ^ cookie[3],
        ]
    }

    fn encode_xor_mapped_v6(ip: Ipv6Addr, port: u16, tx_id: &[u8; 12]) -> Vec<u8> {
        let cookie = STUN_MAGIC_COOKIE.to_be_bytes();
        let x_port = (port ^ ((STUN_MAGIC_COOKIE >> 16) as u16)).to_be_bytes();
        let octets = ip.octets();
        let mut out = vec![0x00, FAMILY_IPV6, x_port[0], x_port[1]];
        for i in 0..4 {
            out.push(octets[i] ^ cookie[i]);
        }
        for i in 0..12 {
            out.push(octets[4 + i] ^ tx_id[i]);
        }
        out
    }

    fn encode_mapped_v4(ip: Ipv4Addr, port: u16) -> Vec<u8> {
        let p = port.to_be_bytes();
        let o = ip.octets();
        vec![0x00, FAMILY_IPV4, p[0], p[1], o[0], o[1], o[2], o[3]]
    }

    #[test]
    fn test_build_and_parse_xor_mapped_v4_and_v6() {
        let tx_id = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
        let req = build_binding_request(tx_id);
        assert_eq!(req.len(), 20);
        assert_eq!(&req[0..2], &STUN_BINDING_REQUEST.to_be_bytes());
        assert_eq!(&req[4..8], &STUN_MAGIC_COOKIE.to_be_bytes());
        assert_eq!(&req[8..20], &tx_id);

        // Valid IPv4 XOR-MAPPED-ADDRESS (with an unaligned dummy attribute before it to test padding)
        let v4_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 42)), 51820);
        let pkt_v4 = build_mock_stun_response(
            &tx_id,
            &[
                (0x8022, b"mycel".to_vec()), // 5 bytes -> padded to 8
                (
                    ATTR_XOR_MAPPED_ADDRESS,
                    encode_xor_mapped_v4(Ipv4Addr::new(203, 0, 113, 42), 51820),
                ),
            ],
        );
        assert_eq!(parse_binding_response(&pkt_v4, &tx_id).unwrap(), v4_addr);

        // Valid IPv6 XOR-MAPPED-ADDRESS
        let v6_ip = Ipv6Addr::new(0x2001, 0x0db8, 0x85a3, 0, 0, 0x8a2e, 0x0370, 0x7334);
        let v6_addr = SocketAddr::new(IpAddr::V6(v6_ip), 44123);
        let pkt_v6 = build_mock_stun_response(
            &tx_id,
            &[(
                ATTR_XOR_MAPPED_ADDRESS,
                encode_xor_mapped_v6(v6_ip, 44123, &tx_id),
            )],
        );
        assert_eq!(parse_binding_response(&pkt_v6, &tx_id).unwrap(), v6_addr);
    }

    #[test]
    fn test_parse_mapped_address_fallback_and_tx_id_mismatch() {
        let tx_id = [9u8; 12];
        let mapped_ip = Ipv4Addr::new(198, 51, 100, 7);
        let pkt = build_mock_stun_response(
            &tx_id,
            &[(ATTR_MAPPED_ADDRESS, encode_mapped_v4(mapped_ip, 3478))],
        );

        assert_eq!(
            parse_binding_response(&pkt, &tx_id).unwrap(),
            SocketAddr::new(IpAddr::V4(mapped_ip), 3478)
        );

        let wrong_tx_id = [8u8; 12];
        assert!(parse_binding_response(&pkt, &wrong_tx_id).is_err());
    }

    #[tokio::test]
    async fn test_discover_via_server_timeout_and_multi_server_fallback() {
        // Bind a dead socket that never replies
        let dead_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let dead_addr = dead_sock.local_addr().unwrap();

        // Bind a mock STUN server socket
        let mock_server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mock_addr = mock_server.local_addr().unwrap();
        let expected_public = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 99)), 51820);

        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 256];
            if let Ok((n, peer)) = mock_server.recv_from(&mut buf).await {
                if n >= 20 {
                    let mut tx_id = [0u8; 12];
                    tx_id.copy_from_slice(&buf[8..20]);
                    let resp = build_mock_stun_response(
                        &tx_id,
                        &[(
                            ATTR_XOR_MAPPED_ADDRESS,
                            encode_xor_mapped_v4(Ipv4Addr::new(198, 51, 100, 99), 51820),
                        )],
                    );
                    let _ = mock_server.send_to(&resp, peer).await;
                }
            }
        });

        // Direct timeout test on dead server
        let client_sock = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let err = discover_via_server(&client_sock, dead_addr, Duration::from_millis(40))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"));

        // Multi-server fallback: dead server first -> mock server second
        let discovered = discover_public_endpoint(
            "127.0.0.1:0".parse().unwrap(),
            &[dead_addr.to_string(), mock_addr.to_string()],
            Duration::from_millis(60),
        )
        .await
        .unwrap();
        assert_eq!(discovered, expected_public);

        let _ = server_task.await;
    }
}
