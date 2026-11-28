//! Zero-knowledge E2E-encrypted bounded TCP relay server and client fallback.
//!
//! The relay only inspects routing metadata (`device_id`, `to_device_id`) and
//! forwards opaque encrypted `payload` bytes without ever holding keys or
//! attempting decryption.

use super::{hmac_sha256, to_hex};
use anyhow::{anyhow, Context, Result};
use base64::Engine as _;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, mpsc, watch, RwLock};
use tracing::{debug, info, warn};
use x25519_dalek::{PublicKey, StaticSecret};

/// Maximum permitted relay frame size (1 MiB).
pub const MAX_RELAY_FRAME_BYTES: usize = 1024 * 1024;
/// Maximum concurrent client connections permitted by `MeshRelayServer`.
pub const MAX_RELAY_CLIENTS: usize = 1024;

const CLIENT_OUTBOUND_QUEUE_CAP: usize = 64;

/// Length-prefixed JSON control and forwarding frames for the zero-knowledge relay.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum RelayFrame {
    Challenge {
        nonce: String,
        server_pubkey: String,
    },
    Register {
        device_id: String,
        #[serde(default)]
        proof: String,
    },
    Registered {
        device_id: String,
    },
    Forward {
        to_device_id: String,
        from_device_id: String,
        payload: Vec<u8>,
    },
    Ping,
    Pong,
    Error {
        message: String,
    },
}

/// Write a length-prefixed JSON `RelayFrame` (`4-byte big-endian u32 + JSON`).
pub async fn write_relay_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame: &RelayFrame,
) -> Result<()> {
    let encoded = serde_json::to_vec(frame).context("Failed to serialize RelayFrame")?;
    if encoded.len() > MAX_RELAY_FRAME_BYTES {
        return Err(anyhow!(
            "Relay frame size {} exceeds MAX_RELAY_FRAME_BYTES ({})",
            encoded.len(),
            MAX_RELAY_FRAME_BYTES
        ));
    }
    let len_prefix = (encoded.len() as u32).to_be_bytes();
    writer
        .write_all(&len_prefix)
        .await
        .context("Failed to write relay frame length prefix")?;
    writer
        .write_all(&encoded)
        .await
        .context("Failed to write relay frame body")?;
    writer
        .flush()
        .await
        .context("Failed to flush relay frame")?;
    Ok(())
}

/// Read a length-prefixed JSON `RelayFrame`, returning `Ok(None)` on clean EOF.
///
/// Rejects frames exceeding `MAX_RELAY_FRAME_BYTES` before allocating the payload buffer.
pub async fn read_relay_frame<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Option<RelayFrame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("Failed to read relay frame length prefix"),
    }

    let len = u32::from_be_bytes(len_buf) as usize;
    if len > MAX_RELAY_FRAME_BYTES {
        return Err(anyhow!(
            "Relay frame length {len} exceeds MAX_RELAY_FRAME_BYTES ({MAX_RELAY_FRAME_BYTES})"
        ));
    }

    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .context("Unexpected EOF reading relay frame body")?;

    let frame: RelayFrame =
        serde_json::from_slice(&body).context("Failed to deserialize RelayFrame")?;
    Ok(Some(frame))
}

fn compute_relay_proof(
    secret: &StaticSecret,
    server_pubkey: &PublicKey,
    device_id: &str,
    nonce: &str,
) -> String {
    let shared = secret.diffie_hellman(server_pubkey);
    let msg = format!("mycel-relay-auth-v1:{device_id}:{nonce}");
    to_hex(&hmac_sha256(shared.as_bytes(), msg.as_bytes()))
}

fn decode_pubkey_b64(b64: &str) -> Option<PublicKey> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(PublicKey::from(arr))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

type ClientMap = Arc<RwLock<HashMap<String, (u64, mpsc::Sender<RelayFrame>)>>>;

struct ConnectionGuard {
    active_connections: Arc<AtomicUsize>,
    clients: ClientMap,
    conn_id: u64,
    device_id: Option<String>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active_connections.fetch_sub(1, Ordering::SeqCst);
        if let Some(device_id) = self.device_id.take() {
            let clients = Arc::clone(&self.clients);
            let conn_id = self.conn_id;
            tokio::spawn(async move {
                let mut map = clients.write().await;
                if let Some((current_id, _)) = map.get(&device_id) {
                    if *current_id == conn_id {
                        map.remove(&device_id);
                    }
                }
            });
        }
    }
}

/// Zero-knowledge TCP relay server for peers behind symmetric NATs.
pub struct MeshRelayServer {
    listener: TcpListener,
    local_addr: SocketAddr,
    clients: ClientMap,
    active_connections: Arc<AtomicUsize>,
    next_conn_id: Arc<AtomicU64>,
    observed_tx: broadcast::Sender<Vec<u8>>,
}

impl MeshRelayServer {
    /// Bind a new `MeshRelayServer` on `listen_addr`.
    pub async fn bind(listen_addr: &str) -> Result<Self> {
        let listener = TcpListener::bind(listen_addr)
            .await
            .with_context(|| format!("Failed to bind MeshRelayServer on '{listen_addr}'"))?;
        let local_addr = listener
            .local_addr()
            .context("Failed to get MeshRelayServer local_addr")?;
        let (observed_tx, _) = broadcast::channel(256);

        Ok(Self {
            listener,
            local_addr,
            clients: Arc::new(RwLock::new(HashMap::new())),
            active_connections: Arc::new(AtomicUsize::new(0)),
            next_conn_id: Arc::new(AtomicU64::new(1)),
            observed_tx,
        })
    }

    /// Return the bound local socket address.
    #[allow(dead_code)]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Return the number of currently registered devices.
    #[allow(dead_code)]
    pub async fn connected_device_count(&self) -> usize {
        self.clients.read().await.len()
    }

    /// Subscribe to a tap of raw forwarded payload bytes observed by the relay.
    #[allow(dead_code)]
    pub fn subscribe_observed_payloads(&self) -> broadcast::Receiver<Vec<u8>> {
        self.observed_tx.subscribe()
    }

    /// Run the relay accept loop until `shutdown_rx` signals `true`.
    pub async fn run(&self, mut shutdown_rx: watch::Receiver<bool>) -> Result<()> {
        info!("MeshRelayServer listening on {}", self.local_addr);

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            tokio::select! {
                changed = shutdown_rx.changed() => {
                    if changed.is_err() || *shutdown_rx.borrow() {
                        break;
                    }
                }
                accepted = self.listener.accept() => {
                    let (mut stream, peer_addr) = match accepted {
                        Ok(pair) => pair,
                        Err(err) => {
                            warn!("MeshRelayServer accept error: {err}");
                            continue;
                        }
                    };

                    let current = self.active_connections.load(Ordering::SeqCst);
                    if current >= MAX_RELAY_CLIENTS {
                        warn!("Rejecting relay client {peer_addr}: MAX_RELAY_CLIENTS ({MAX_RELAY_CLIENTS}) reached");
                        let _ = write_relay_frame(
                            &mut stream,
                            &RelayFrame::Error {
                                message: "Relay server at capacity".to_string(),
                            },
                        )
                        .await;
                        continue;
                    }

                    self.active_connections.fetch_add(1, Ordering::SeqCst);
                    let conn_id = self.next_conn_id.fetch_add(1, Ordering::SeqCst);
                    let clients = Arc::clone(&self.clients);
                    let active_connections = Arc::clone(&self.active_connections);
                    let observed_tx = self.observed_tx.clone();
                    let client_shutdown = shutdown_rx.clone();

                    tokio::spawn(async move {
                        handle_relay_connection(
                            stream,
                            peer_addr,
                            conn_id,
                            clients,
                            active_connections,
                            observed_tx,
                            client_shutdown,
                        )
                        .await;
                    });
                }
            }
        }

        Ok(())
    }
}

async fn handle_relay_connection(
    stream: TcpStream,
    peer_addr: SocketAddr,
    conn_id: u64,
    clients: ClientMap,
    active_connections: Arc<AtomicUsize>,
    observed_tx: broadcast::Sender<Vec<u8>>,
    mut shutdown_rx: watch::Receiver<bool>,
) {
    let mut guard = ConnectionGuard {
        active_connections,
        clients: Arc::clone(&clients),
        conn_id,
        device_id: None,
    };

    let (mut reader, mut writer) = stream.into_split();

    let (nonce, server_secret, server_pubkey_b64) = {
        let mut rng = rand::thread_rng();
        let mut nonce_bytes = [0u8; 32];
        rng.fill_bytes(&mut nonce_bytes);
        let secret = StaticSecret::random_from_rng(rng);
        let pubkey = PublicKey::from(&secret);
        let pubkey_b64 = base64::engine::general_purpose::STANDARD.encode(pubkey.as_bytes());
        (to_hex(&nonce_bytes), secret, pubkey_b64)
    };

    if write_relay_frame(
        &mut writer,
        &RelayFrame::Challenge {
            nonce: nonce.clone(),
            server_pubkey: server_pubkey_b64,
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let first_frame = tokio::select! {
        res = read_relay_frame(&mut reader) => match res {
            Ok(Some(frame)) => frame,
            Ok(None) => return,
            Err(err) => {
                debug!("Relay client {peer_addr} failed initial read: {err}");
                return;
            }
        },
        _ = shutdown_rx.changed() => return,
    };

    let (device_id, proof) = match first_frame {
        RelayFrame::Register { device_id, proof } if !device_id.trim().is_empty() => {
            (device_id, proof)
        }
        _ => {
            let _ = write_relay_frame(
                &mut writer,
                &RelayFrame::Error {
                    message: "First frame must be Register with non-empty device_id".to_string(),
                },
            )
            .await;
            return;
        }
    };

    let Some(client_pub) = decode_pubkey_b64(&device_id) else {
        let _ = write_relay_frame(
            &mut writer,
            &RelayFrame::Error {
                message: "Invalid device_id: expected base64 32-byte X25519 public key".to_string(),
            },
        )
        .await;
        return;
    };

    let expected_proof = compute_relay_proof(&server_secret, &client_pub, &device_id, &nonce);
    if !constant_time_eq(
        proof.to_ascii_lowercase().as_bytes(),
        expected_proof.as_bytes(),
    ) {
        warn!("Relay client {peer_addr} failed challenge-response authentication for {device_id}");
        let _ = write_relay_frame(
            &mut writer,
            &RelayFrame::Error {
                message: "Invalid registration proof".to_string(),
            },
        )
        .await;
        return;
    }

    let (tx, mut rx) = mpsc::channel::<RelayFrame>(CLIENT_OUTBOUND_QUEUE_CAP);
    clients
        .write()
        .await
        .insert(device_id.clone(), (conn_id, tx.clone()));
    guard.device_id = Some(device_id.clone());

    if write_relay_frame(
        &mut writer,
        &RelayFrame::Registered {
            device_id: device_id.clone(),
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let mut writer_shutdown = shutdown_rx.clone();
    let writer_handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                maybe_frame = rx.recv() => match maybe_frame {
                    Some(frame) => {
                        if write_relay_frame(&mut writer, &frame).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                changed = writer_shutdown.changed() => {
                    if changed.is_err() || *writer_shutdown.borrow() {
                        break;
                    }
                }
            }
        }
    });

    loop {
        let next_frame = tokio::select! {
            res = read_relay_frame(&mut reader) => match res {
                Ok(Some(frame)) => frame,
                Ok(None) => break,
                Err(err) => {
                    debug!("Relay client '{device_id}' read error: {err}");
                    break;
                }
            },
            changed = shutdown_rx.changed() => {
                if changed.is_err() || *shutdown_rx.borrow() {
                    break;
                }
                continue;
            }
        };

        match next_frame {
            RelayFrame::Forward {
                to_device_id,
                from_device_id: _,
                payload,
            } => {
                let _ = observed_tx.send(payload.clone());
                let target = clients
                    .read()
                    .await
                    .get(&to_device_id)
                    .map(|(_, tx)| tx.clone());
                if let Some(target_tx) = target {
                    let forwarded = RelayFrame::Forward {
                        to_device_id,
                        from_device_id: device_id.clone(),
                        payload,
                    };
                    if target_tx.try_send(forwarded).is_err() {
                        debug!(
                            "Dropped forwarded relay frame: target outbound queue full or closed"
                        );
                    }
                } else {
                    debug!("Relay target '{to_device_id}' is not connected");
                }
            }
            RelayFrame::Ping => {
                let _ = tx.try_send(RelayFrame::Pong);
            }
            RelayFrame::Register { .. } => {
                let _ = tx.try_send(RelayFrame::Error {
                    message: "Already registered on this connection".to_string(),
                });
            }
            RelayFrame::Challenge { .. }
            | RelayFrame::Registered { .. }
            | RelayFrame::Pong
            | RelayFrame::Error { .. } => {}
        }
    }

    writer_handle.abort();
}

/// Reconnecting client for `MeshRelayServer`.
#[derive(Clone)]
pub struct RelayClient {
    outbound_tx: mpsc::Sender<(String, Vec<u8>)>,
    connected: Arc<AtomicBool>,
}

impl RelayClient {
    /// Start a background relay client loop that connects to `relay_addr`,
    /// authenticates with `device_secret`, registers `device_id`, and delivers
    /// incoming `(from_device_id, payload)` frames.
    pub fn start(
        relay_addr: String,
        device_id: String,
        device_secret: [u8; 32],
        mut shutdown_rx: watch::Receiver<bool>,
    ) -> (Self, mpsc::Receiver<(String, Vec<u8>)>) {
        let (outbound_tx, mut outbound_rx) = mpsc::channel::<(String, Vec<u8>)>(64);
        let (inbound_tx, inbound_rx) = mpsc::channel::<(String, Vec<u8>)>(64);
        let connected = Arc::new(AtomicBool::new(false));
        let connected_clone = Arc::clone(&connected);

        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(50);
            let max_backoff = Duration::from_secs(5);

            loop {
                if *shutdown_rx.borrow() {
                    break;
                }

                let stream_res = tokio::select! {
                    res = TcpStream::connect(&relay_addr) => res,
                    changed = shutdown_rx.changed() => {
                        if changed.is_err() || *shutdown_rx.borrow() {
                            break;
                        }
                        continue;
                    }
                };

                let stream = match stream_res {
                    Ok(s) => s,
                    Err(err) => {
                        debug!("RelayClient failed to connect to {relay_addr}: {err}");
                        tokio::select! {
                            _ = tokio::time::sleep(backoff) => {}
                            changed = shutdown_rx.changed() => {
                                if changed.is_err() || *shutdown_rx.borrow() {
                                    break;
                                }
                            }
                        }
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                };

                let (mut reader, mut writer) = stream.into_split();
                let (nonce, server_pubkey) = match read_relay_frame(&mut reader).await {
                    Ok(Some(RelayFrame::Challenge {
                        nonce,
                        server_pubkey,
                    })) => match decode_pubkey_b64(&server_pubkey) {
                        Some(pk) => (nonce, pk),
                        None => {
                            connected_clone.store(false, Ordering::SeqCst);
                            tokio::time::sleep(backoff).await;
                            backoff = (backoff * 2).min(max_backoff);
                            continue;
                        }
                    },
                    _ => {
                        connected_clone.store(false, Ordering::SeqCst);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                };

                let secret = StaticSecret::from(device_secret);
                let proof = compute_relay_proof(&secret, &server_pubkey, &device_id, &nonce);

                if write_relay_frame(
                    &mut writer,
                    &RelayFrame::Register {
                        device_id: device_id.clone(),
                        proof,
                    },
                )
                .await
                .is_err()
                {
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(max_backoff);
                    continue;
                }

                match read_relay_frame(&mut reader).await {
                    Ok(Some(RelayFrame::Registered { .. })) => {
                        backoff = Duration::from_millis(50);
                        connected_clone.store(true, Ordering::SeqCst);
                    }
                    _ => {
                        connected_clone.store(false, Ordering::SeqCst);
                        tokio::time::sleep(backoff).await;
                        backoff = (backoff * 2).min(max_backoff);
                        continue;
                    }
                }

                // Run writer & reader until disconnect or shutdown.
                let (conn_stop_tx, mut conn_stop_rx) = watch::channel(false);
                let dev_id_for_writer = device_id.clone();

                // Read loop in a separate task so read_exact is never cancelled mid-frame.
                let inbound_tx_clone = inbound_tx.clone();
                let mut reader_shutdown = shutdown_rx.clone();
                let reader_stop_tx = conn_stop_tx.clone();
                let reader_handle = tokio::spawn(async move {
                    loop {
                        let frame = tokio::select! {
                            res = read_relay_frame(&mut reader) => match res {
                                Ok(Some(f)) => f,
                                Ok(None) | Err(_) => break,
                            },
                            changed = reader_shutdown.changed() => {
                                if changed.is_err() || *reader_shutdown.borrow() {
                                    break;
                                }
                                continue;
                            }
                        };

                        if let RelayFrame::Forward {
                            from_device_id,
                            payload,
                            ..
                        } = frame
                        {
                            if inbound_tx_clone
                                .send((from_device_id, payload))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    }
                    let _ = reader_stop_tx.send(true);
                });

                loop {
                    tokio::select! {
                        maybe_out = outbound_rx.recv() => match maybe_out {
                            Some((to_device_id, payload)) => {
                                let frame = RelayFrame::Forward {
                                    to_device_id,
                                    from_device_id: dev_id_for_writer.clone(),
                                    payload,
                                };
                                if write_relay_frame(&mut writer, &frame).await.is_err() {
                                    break;
                                }
                            }
                            None => {
                                let _ = conn_stop_tx.send(true);
                                connected_clone.store(false, Ordering::SeqCst);
                                reader_handle.abort();
                                return;
                            }
                        },
                        _ = conn_stop_rx.changed() => {
                            if *conn_stop_rx.borrow() {
                                break;
                            }
                        }
                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                break;
                            }
                        }
                    }
                }

                connected_clone.store(false, Ordering::SeqCst);
                reader_handle.abort();
            }

            connected_clone.store(false, Ordering::SeqCst);
        });

        (
            Self {
                outbound_tx,
                connected,
            },
            inbound_rx,
        )
    }

    /// Return whether the relay client currently has an active registered connection.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::SeqCst)
    }

    /// Forward an opaque payload to `to_device_id` via the relay.
    pub async fn forward(&self, to_device_id: &str, payload: Vec<u8>) -> Result<()> {
        if !self.is_connected() {
            return Err(anyhow!("RelayClient is not connected"));
        }
        self.outbound_tx
            .send((to_device_id.to_string(), payload))
            .await
            .map_err(|_| anyhow!("RelayClient outbound channel closed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_relay_framing_round_trip_and_oversize_rejection() {
        let (mut client, mut server) = duplex(4096);
        let frame = RelayFrame::Forward {
            to_device_id: "peer-b".to_string(),
            from_device_id: "peer-a".to_string(),
            payload: vec![1, 2, 3, 4, 5],
        };

        write_relay_frame(&mut client, &frame).await.unwrap();
        let decoded = read_relay_frame(&mut server).await.unwrap().unwrap();
        assert_eq!(decoded, frame);

        // Oversize frame (> 1 MiB) rejected before allocation
        let oversize_len = ((MAX_RELAY_FRAME_BYTES + 1) as u32).to_be_bytes();
        client.write_all(&oversize_len).await.unwrap();
        client.flush().await.unwrap();
        let err = read_relay_frame(&mut server).await.unwrap_err();
        assert!(err.to_string().contains("exceeds MAX_RELAY_FRAME_BYTES"));

        // Clean EOF
        drop(client);
        assert!(read_relay_frame(&mut server).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_relay_server_registration_ping_and_forwarding() {
        let server = Arc::new(MeshRelayServer::bind("127.0.0.1:0").await.unwrap());
        let addr = server.local_addr();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let srv_clone = Arc::clone(&server);
        let srv_task = tokio::spawn(async move {
            srv_clone.run(shutdown_rx).await.unwrap();
        });

        let mut observed_rx = server.subscribe_observed_payloads();

        // Connect two raw clients to test Challenge/Register, Ping/Pong & from_device_id overwrite
        let secret_a = StaticSecret::random_from_rng(rand::thread_rng());
        let dev_a =
            base64::engine::general_purpose::STANDARD.encode(PublicKey::from(&secret_a).as_bytes());
        let secret_b = StaticSecret::random_from_rng(rand::thread_rng());
        let dev_b =
            base64::engine::general_purpose::STANDARD.encode(PublicKey::from(&secret_b).as_bytes());

        let mut stream_a = TcpStream::connect(addr).await.unwrap();
        let (nonce_a, srv_pk_a) = match read_relay_frame(&mut stream_a).await.unwrap().unwrap() {
            RelayFrame::Challenge {
                nonce,
                server_pubkey,
            } => (nonce, decode_pubkey_b64(&server_pubkey).unwrap()),
            other => panic!("Expected Challenge frame, got {other:?}"),
        };
        let proof_a = compute_relay_proof(&secret_a, &srv_pk_a, &dev_a, &nonce_a);
        write_relay_frame(
            &mut stream_a,
            &RelayFrame::Register {
                device_id: dev_a.clone(),
                proof: proof_a,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            read_relay_frame(&mut stream_a).await.unwrap().unwrap(),
            RelayFrame::Registered {
                device_id: dev_a.clone()
            }
        );

        write_relay_frame(&mut stream_a, &RelayFrame::Ping)
            .await
            .unwrap();
        assert_eq!(
            read_relay_frame(&mut stream_a).await.unwrap().unwrap(),
            RelayFrame::Pong
        );

        let mut stream_b = TcpStream::connect(addr).await.unwrap();
        let (nonce_b, srv_pk_b) = match read_relay_frame(&mut stream_b).await.unwrap().unwrap() {
            RelayFrame::Challenge {
                nonce,
                server_pubkey,
            } => (nonce, decode_pubkey_b64(&server_pubkey).unwrap()),
            other => panic!("Expected Challenge frame, got {other:?}"),
        };
        let proof_b = compute_relay_proof(&secret_b, &srv_pk_b, &dev_b, &nonce_b);
        write_relay_frame(
            &mut stream_b,
            &RelayFrame::Register {
                device_id: dev_b.clone(),
                proof: proof_b,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            read_relay_frame(&mut stream_b).await.unwrap().unwrap(),
            RelayFrame::Registered {
                device_id: dev_b.clone()
            }
        );

        assert_eq!(server.connected_device_count().await, 2);

        // Spoofed from_device_id should be overwritten by server with dev_a
        let opaque_ciphertext = b"\xde\xad\xbe\xef\x01\x02\x03".to_vec();
        write_relay_frame(
            &mut stream_a,
            &RelayFrame::Forward {
                to_device_id: dev_b.clone(),
                from_device_id: "spoofed-id".to_string(),
                payload: opaque_ciphertext.clone(),
            },
        )
        .await
        .unwrap();

        let received = read_relay_frame(&mut stream_b).await.unwrap().unwrap();
        assert_eq!(
            received,
            RelayFrame::Forward {
                to_device_id: dev_b,
                from_device_id: dev_a,
                payload: opaque_ciphertext.clone(),
            }
        );

        assert_eq!(observed_rx.recv().await.unwrap(), opaque_ciphertext);

        let _ = shutdown_tx.send(true);
        let _ = srv_task.await;
    }

    #[tokio::test]
    async fn test_relay_server_max_clients_cap() {
        let server = Arc::new(MeshRelayServer::bind("127.0.0.1:0").await.unwrap());
        let addr = server.local_addr();
        server
            .active_connections
            .store(MAX_RELAY_CLIENTS, Ordering::SeqCst);

        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let srv_clone = Arc::clone(&server);
        let srv_task = tokio::spawn(async move {
            srv_clone.run(shutdown_rx).await.unwrap();
        });

        let mut stream = TcpStream::connect(addr).await.unwrap();
        let reply = read_relay_frame(&mut stream).await.unwrap().unwrap();
        match reply {
            RelayFrame::Error { message } => assert!(message.contains("capacity")),
            other => panic!("Expected Error frame, got {other:?}"),
        }

        let _ = shutdown_tx.send(true);
        let _ = srv_task.await;
    }

    #[tokio::test]
    async fn test_relay_client_end_to_end_opaque_forwarding() {
        let server = Arc::new(MeshRelayServer::bind("127.0.0.1:0").await.unwrap());
        let addr = server.local_addr().to_string();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let srv_clone = Arc::clone(&server);
        let srv_task = tokio::spawn(async move {
            srv_clone.run(shutdown_rx).await.unwrap();
        });

        let secret_a = StaticSecret::random_from_rng(rand::thread_rng());
        let dev_a =
            base64::engine::general_purpose::STANDARD.encode(PublicKey::from(&secret_a).as_bytes());
        let secret_b = StaticSecret::random_from_rng(rand::thread_rng());
        let dev_b =
            base64::engine::general_purpose::STANDARD.encode(PublicKey::from(&secret_b).as_bytes());

        let (client_a, _rx_a) = RelayClient::start(
            addr.clone(),
            dev_a.clone(),
            secret_a.to_bytes(),
            shutdown_tx.subscribe(),
        );
        let (client_b, mut rx_b) = RelayClient::start(
            addr,
            dev_b.clone(),
            secret_b.to_bytes(),
            shutdown_tx.subscribe(),
        );

        for _ in 0..50 {
            if client_a.is_connected() && client_b.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(client_a.is_connected());
        assert!(client_b.is_connected());

        let opaque = vec![0xaa, 0xbb, 0xcc, 0xdd, 0xee];
        client_a.forward(&dev_b, opaque.clone()).await.unwrap();

        let (from, payload) = tokio::time::timeout(Duration::from_secs(2), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, dev_a);
        assert_eq!(payload, opaque);

        let _ = shutdown_tx.send(true);
        let _ = srv_task.await;
    }

    #[tokio::test]
    async fn test_relay_server_rejects_unauthenticated_hijack_and_preserves_victim_slot() {
        let server = Arc::new(MeshRelayServer::bind("127.0.0.1:0").await.unwrap());
        let addr = server.local_addr();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);

        let srv_clone = Arc::clone(&server);
        let srv_task = tokio::spawn(async move {
            srv_clone.run(shutdown_rx).await.unwrap();
        });

        let victim_secret = StaticSecret::random_from_rng(rand::thread_rng());
        let victim_id = base64::engine::general_purpose::STANDARD
            .encode(PublicKey::from(&victim_secret).as_bytes());
        let peer_secret = StaticSecret::random_from_rng(rand::thread_rng());
        let peer_id = base64::engine::general_purpose::STANDARD
            .encode(PublicKey::from(&peer_secret).as_bytes());

        let (victim_client, mut victim_rx) = RelayClient::start(
            addr.to_string(),
            victim_id.clone(),
            victim_secret.to_bytes(),
            shutdown_tx.subscribe(),
        );
        let (peer_client, mut peer_rx) = RelayClient::start(
            addr.to_string(),
            peer_id.clone(),
            peer_secret.to_bytes(),
            shutdown_tx.subscribe(),
        );

        for _ in 0..50 {
            if victim_client.is_connected() && peer_client.is_connected() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(victim_client.is_connected());
        assert!(peer_client.is_connected());

        // Attacker 1: missing/empty proof claiming victim_id
        let mut attacker1 = TcpStream::connect(addr).await.unwrap();
        assert!(matches!(
            read_relay_frame(&mut attacker1).await.unwrap().unwrap(),
            RelayFrame::Challenge { .. }
        ));
        write_relay_frame(
            &mut attacker1,
            &RelayFrame::Register {
                device_id: victim_id.clone(),
                proof: String::new(),
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_relay_frame(&mut attacker1).await.unwrap().unwrap(),
            RelayFrame::Error { .. }
        ));
        assert!(read_relay_frame(&mut attacker1).await.unwrap().is_none());
        drop(attacker1);

        // Attacker 2: proof signed with wrong private key claiming victim_id
        let mut attacker2 = TcpStream::connect(addr).await.unwrap();
        let (nonce, srv_pk) = match read_relay_frame(&mut attacker2).await.unwrap().unwrap() {
            RelayFrame::Challenge {
                nonce,
                server_pubkey,
            } => (nonce, decode_pubkey_b64(&server_pubkey).unwrap()),
            other => panic!("Expected Challenge, got {other:?}"),
        };
        let attacker_secret = StaticSecret::random_from_rng(rand::thread_rng());
        let forged_proof = compute_relay_proof(&attacker_secret, &srv_pk, &victim_id, &nonce);
        write_relay_frame(
            &mut attacker2,
            &RelayFrame::Register {
                device_id: victim_id.clone(),
                proof: forged_proof,
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_relay_frame(&mut attacker2).await.unwrap().unwrap(),
            RelayFrame::Error { .. }
        ));
        assert!(read_relay_frame(&mut attacker2).await.unwrap().is_none());
        drop(attacker2);

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Victim's slot must remain intact and functional in both directions
        peer_client
            .forward(&victim_id, b"to-victim".to_vec())
            .await
            .unwrap();
        let (from, payload) = tokio::time::timeout(Duration::from_secs(2), victim_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from, peer_id);
        assert_eq!(payload, b"to-victim");

        victim_client
            .forward(&peer_id, b"from-victim".to_vec())
            .await
            .unwrap();
        let (from2, payload2) = tokio::time::timeout(Duration::from_secs(2), peer_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(from2, victim_id);
        assert_eq!(payload2, b"from-victim");

        let _ = shutdown_tx.send(true);
        let _ = srv_task.await;
    }
}
