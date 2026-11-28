use std::time::Duration;

use futures::stream;
use iced::widget::{
    button, column, container, row, scrollable, text, text_input, Column, Row, Space,
};
use iced::{Alignment, Background, Border, Color, Element, Font, Length, Subscription, Task};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use crate::ipc_client::{
    spawn_event_subscriber, IpcClientConfig, IpcRequestClient, PairingCodeSnapshot,
    PeerAddedSnapshot, ProvidersSnapshot, StatusSnapshot, SubscriberEvent, SyncSnapshot,
};
use crate::protocol::{IpcResponse, LlmProvider, PeerStatus, ProviderSummary, SystemEvent};

const MAX_RECENT_EVENTS: usize = 20;

// Dark minimal OS shell palette
const BG_ROOT: Color = Color::from_rgb(0.067, 0.078, 0.102); // #11141A
const BG_SURFACE: Color = Color::from_rgb(0.102, 0.118, 0.153); // #1A1E27
const BG_ELEVATED: Color = Color::from_rgb(0.133, 0.153, 0.196); // #222732
const BG_CODE: Color = Color::from_rgb(0.047, 0.055, 0.075); // #0C0E13
const BORDER_SUBTLE: Color = Color::from_rgb(0.204, 0.231, 0.290); // #343B4A

const TEXT_PRIMARY: Color = Color::from_rgb(0.925, 0.941, 0.969); // #ECF0F7
const TEXT_MUTED: Color = Color::from_rgb(0.580, 0.631, 0.718); // #94A1B7

const ACCENT_CYAN: Color = Color::from_rgb(0.220, 0.741, 0.973); // #38BDF8
const ACCENT_EMERALD: Color = Color::from_rgb(0.204, 0.827, 0.600); // #34D399
const ACCENT_AMBER: Color = Color::from_rgb(0.984, 0.749, 0.141); // #FBBF24
const ACCENT_ROSE: Color = Color::from_rgb(0.984, 0.443, 0.522); // #FB7185

/// Connection state to `mycel-runtime`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionState {
    Connecting,
    Connected,
    Reconnecting(String),
}

/// Severity level for the live system event / recovery banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BannerSeverity {
    Info,
    Warning,
    Error,
    Success,
}

/// Active banner notice displayed below the status header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BannerNotice {
    pub severity: BannerSeverity,
    pub summary: String,
}

/// A single entry in the conversation stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatEntry {
    User {
        content: String,
    },
    Assistant {
        content: String,
    },
    System {
        content: String,
        is_error: bool,
    },
    CodeResult {
        code: String,
        output: String,
        success: bool,
    },
}

/// Active side drawer in the shell UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveDrawer {
    Providers,
    Mesh,
}

/// Interactive confirmation card for policy-gated actions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingConfirmationCard {
    pub id: String,
    pub action: String,
    pub code: String,
    pub reason: String,
    pub busy: bool,
}

/// Messages processed by `MycelShellApp`.
#[derive(Debug, Clone)]
pub enum Message {
    SubscriberUpdate(SubscriberEvent),
    StatusFetched(Result<StatusSnapshot, String>),
    SyncStatusFetched(Result<SyncSnapshot, String>),
    ProvidersFetched(Result<ProvidersSnapshot, String>),
    RefreshStatus,
    CycleProvider,
    ToggleDrawer(ActiveDrawer),
    SelectProvider(String),
    ProviderModelInputChanged(String),
    ApplyProvider,
    SetProviderCompleted(Result<String, String>),
    MeshEndpointInputChanged(String),
    GeneratePairingCode,
    PairingCodeGenerated(Result<PairingCodeSnapshot, String>),
    MeshPairInputChanged(String),
    PairPeer,
    PairPeerCompleted(Result<PeerAddedSnapshot, String>),
    TriggerSync,
    TriggerSyncCompleted(Result<String, String>),
    InputChanged(String),
    SendPrompt,
    ChatChunk(String),
    ChatFinished(Result<IpcResponse, String>),
    DismissBanner,
    ConfirmDecision {
        id: String,
        approved: bool,
    },
    ConfirmCompleted {
        id: String,
        approved: bool,
        result: Result<IpcResponse, String>,
    },
}

/// Root state for the Mycel OS GUI shell.
pub struct MycelShellApp {
    pub socket_path: String,
    pub session_id: String,
    pub client: IpcRequestClient,
    pub connection: ConnectionState,
    pub runtime_version: Option<String>,
    pub active_model: Option<String>,
    pub mesh_enabled: bool,
    pub mesh_connected_peers: usize,
    pub mesh_trusted_peers: usize,
    pub mesh_transport: Option<String>,
    pub active_drawer: Option<ActiveDrawer>,
    pub providers_snapshot: Option<ProvidersSnapshot>,
    pub selected_provider_id: Option<String>,
    pub provider_model_input: String,
    pub provider_busy: bool,
    pub provider_feedback: Option<(String, bool)>,
    pub sync_snapshot: Option<SyncSnapshot>,
    pub mesh_endpoint_input: String,
    pub generated_pairing_code: Option<PairingCodeSnapshot>,
    pub mesh_pair_input: String,
    pub mesh_busy: bool,
    pub mesh_feedback: Option<(String, bool)>,
    pub messages: Vec<ChatEntry>,
    pub streaming_buffer: Option<String>,
    pub in_flight: bool,
    pub input_value: String,
    pub provider: LlmProvider,
    pub banner: Option<BannerNotice>,
    pub recent_events: Vec<String>,
    pub pending_confirmations: Vec<PendingConfirmationCard>,
}

impl MycelShellApp {
    /// Create a new app instance with a fresh session UUID.
    pub fn new(socket_path: String) -> Self {
        let session_id = Uuid::new_v4().to_string();
        let client = IpcRequestClient::new(IpcClientConfig {
            socket_path: socket_path.clone(),
            session_id: session_id.clone(),
        });

        Self {
            socket_path,
            session_id,
            client,
            connection: ConnectionState::Connecting,
            runtime_version: None,
            active_model: None,
            mesh_enabled: false,
            mesh_connected_peers: 0,
            mesh_trusted_peers: 0,
            mesh_transport: None,
            active_drawer: None,
            providers_snapshot: None,
            selected_provider_id: None,
            provider_model_input: String::new(),
            provider_busy: false,
            provider_feedback: None,
            sync_snapshot: None,
            mesh_endpoint_input: String::new(),
            generated_pairing_code: None,
            mesh_pair_input: String::new(),
            mesh_busy: false,
            mesh_feedback: None,
            messages: Vec::new(),
            streaming_buffer: None,
            in_flight: false,
            input_value: String::new(),
            provider: LlmProvider::Auto,
            banner: None,
            recent_events: Vec::new(),
            pending_confirmations: Vec::new(),
        }
    }

    /// Boot hook for `iced::application`.
    pub fn boot(socket_path: String) -> (Self, Task<Message>) {
        let app = Self::new(socket_path);
        let tasks = app.fetch_all_status_tasks();
        (app, tasks)
    }

    /// Subscription to the dedicated push-only `Subscribe` event stream.
    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::run_with(self.socket_path.clone(), subscribe_events)
    }

    /// Update state in response to UI, IPC, or subscription messages.
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::SubscriberUpdate(event) => match event {
                SubscriberEvent::Connected => {
                    self.connection = ConnectionState::Connected;
                    return self.fetch_all_status_tasks();
                }
                SubscriberEvent::Disconnected(reason) => {
                    self.connection = ConnectionState::Reconnecting(reason);
                }
                SubscriberEvent::Event(sys_event) => {
                    self.handle_system_event(sys_event);
                }
            },
            Message::StatusFetched(Ok(status)) => {
                self.runtime_version = Some(status.version);
                self.active_model = Some(status.llm_model);
            }
            Message::StatusFetched(Err(err)) => {
                self.banner = Some(BannerNotice {
                    severity: BannerSeverity::Warning,
                    summary: format!("Failed to fetch status: {err}"),
                });
            }
            Message::SyncStatusFetched(Ok(sync)) => {
                self.mesh_enabled = sync.enabled;
                self.mesh_connected_peers = sync.connected_peers;
                self.mesh_trusted_peers = sync.trusted_peers;
                self.mesh_transport = Some(sync.transport.clone());
                self.sync_snapshot = Some(sync);
            }
            Message::SyncStatusFetched(Err(_err)) => {
                // Mesh sync may be disabled or unavailable during startup; keep header clean.
            }
            Message::ProvidersFetched(Ok(providers)) => {
                if self.selected_provider_id.is_none() {
                    self.selected_provider_id = providers.active_provider.clone();
                }
                self.providers_snapshot = Some(providers);
            }
            Message::ProvidersFetched(Err(_err)) => {
                // Runtime may still be starting up; keep header clean.
            }
            Message::RefreshStatus => {
                return self.fetch_all_status_tasks();
            }
            Message::CycleProvider => {
                self.provider = match self.provider {
                    LlmProvider::Auto => LlmProvider::Local,
                    LlmProvider::Local => LlmProvider::Cloud,
                    LlmProvider::Cloud => LlmProvider::Auto,
                };
            }
            Message::ToggleDrawer(drawer) => {
                if self.active_drawer == Some(drawer) {
                    self.active_drawer = None;
                } else {
                    self.active_drawer = Some(drawer);
                    return match drawer {
                        ActiveDrawer::Providers => self.fetch_providers_task(),
                        ActiveDrawer::Mesh => self.fetch_sync_status_task(),
                    };
                }
            }
            Message::SelectProvider(id) => {
                if self.selected_provider_id.as_deref() != Some(id.as_str()) {
                    self.provider_model_input.clear();
                }
                self.selected_provider_id = Some(id);
                self.provider_feedback = None;
            }
            Message::ProviderModelInputChanged(value) => {
                self.provider_model_input = value;
            }
            Message::ApplyProvider => {
                if self.provider_busy {
                    return Task::none();
                }
                let Some(provider_id) = self.selected_provider_id.clone() else {
                    return Task::none();
                };
                let model = {
                    let trimmed = self.provider_model_input.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                };
                self.provider_busy = true;
                self.provider_feedback = None;
                let client = self.client.clone();
                return Task::perform(
                    async move {
                        client
                            .set_provider(provider_id, model)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::SetProviderCompleted,
                );
            }
            Message::SetProviderCompleted(result) => {
                self.provider_busy = false;
                match result {
                    Ok(msg) => {
                        self.provider_feedback = Some((msg, false));
                        return Task::batch([
                            self.fetch_providers_task(),
                            self.fetch_status_task(),
                        ]);
                    }
                    Err(err) => {
                        self.provider_feedback = Some((err, true));
                    }
                }
            }
            Message::MeshEndpointInputChanged(value) => {
                self.mesh_endpoint_input = value;
            }
            Message::GeneratePairingCode => {
                if self.mesh_busy {
                    return Task::none();
                }
                let endpoint = {
                    let trimmed = self.mesh_endpoint_input.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        Some(trimmed.to_string())
                    }
                };
                self.mesh_busy = true;
                self.mesh_feedback = None;
                let client = self.client.clone();
                return Task::perform(
                    async move {
                        client
                            .generate_pairing_code(endpoint)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    Message::PairingCodeGenerated,
                );
            }
            Message::PairingCodeGenerated(result) => {
                self.mesh_busy = false;
                match result {
                    Ok(snapshot) => {
                        self.mesh_feedback = Some((
                            format!("Generated pairing code for {}", snapshot.device_id),
                            false,
                        ));
                        self.generated_pairing_code = Some(snapshot);
                    }
                    Err(err) => {
                        self.mesh_feedback = Some((err, true));
                    }
                }
            }
            Message::MeshPairInputChanged(value) => {
                self.mesh_pair_input = value;
            }
            Message::PairPeer => {
                if self.mesh_busy {
                    return Task::none();
                }
                let code = self.mesh_pair_input.trim().to_string();
                if code.is_empty() {
                    return Task::none();
                }
                self.mesh_busy = true;
                self.mesh_feedback = None;
                let client = self.client.clone();
                return Task::perform(
                    async move { client.pair_peer(code).await.map_err(|e| e.to_string()) },
                    Message::PairPeerCompleted,
                );
            }
            Message::PairPeerCompleted(result) => {
                self.mesh_busy = false;
                match result {
                    Ok(added) => {
                        self.mesh_pair_input.clear();
                        self.mesh_feedback = Some((
                            format!(
                                "Paired {} ({}) — {} synced event(s)",
                                added.peer.name, added.peer.id, added.synced_events
                            ),
                            false,
                        ));
                        return self.fetch_sync_status_task();
                    }
                    Err(err) => {
                        self.mesh_feedback = Some((err, true));
                    }
                }
            }
            Message::TriggerSync => {
                if self.mesh_busy {
                    return Task::none();
                }
                self.mesh_busy = true;
                self.mesh_feedback = None;
                let client = self.client.clone();
                return Task::perform(
                    async move { client.trigger_sync(None).await.map_err(|e| e.to_string()) },
                    Message::TriggerSyncCompleted,
                );
            }
            Message::TriggerSyncCompleted(result) => {
                self.mesh_busy = false;
                match result {
                    Ok(msg) => {
                        self.mesh_feedback = Some((msg, false));
                        return self.fetch_sync_status_task();
                    }
                    Err(err) => {
                        self.mesh_feedback = Some((err, true));
                    }
                }
            }
            Message::InputChanged(value) => {
                self.input_value = value;
            }
            Message::SendPrompt => {
                let prompt = self.input_value.trim().to_string();
                if prompt.is_empty() || self.in_flight {
                    return Task::none();
                }

                self.input_value.clear();
                self.messages.push(ChatEntry::User {
                    content: prompt.clone(),
                });
                self.in_flight = true;
                self.streaming_buffer = Some(String::new());

                let client = self.client.clone();
                let provider = self.provider;
                return Task::stream(chat_stream(client, prompt, provider));
            }
            Message::ChatChunk(delta) => {
                if self.in_flight {
                    self.streaming_buffer
                        .get_or_insert_with(String::new)
                        .push_str(&delta);
                }
            }
            Message::ChatFinished(result) => {
                self.in_flight = false;
                let streamed = self.streaming_buffer.take();

                match result {
                    Ok(IpcResponse::Chat { response, .. }) => {
                        let content = if response.is_empty() {
                            streamed.unwrap_or_default()
                        } else {
                            response
                        };
                        self.messages.push(ChatEntry::Assistant { content });
                    }
                    Ok(IpcResponse::CodeResult {
                        code,
                        output,
                        success,
                    }) => {
                        if let Some(prefix) = streamed {
                            if !prefix.trim().is_empty() {
                                self.messages.push(ChatEntry::Assistant { content: prefix });
                            }
                        }
                        self.messages.push(ChatEntry::CodeResult {
                            code,
                            output,
                            success,
                        });
                    }
                    Ok(IpcResponse::ConfirmationRequired {
                        id,
                        action,
                        code,
                        reason,
                    }) => {
                        if let Some(prefix) = streamed {
                            if !prefix.trim().is_empty() {
                                self.messages.push(ChatEntry::Assistant { content: prefix });
                            }
                        }
                        self.push_confirmation_card(PendingConfirmationCard {
                            id,
                            action,
                            code,
                            reason,
                            busy: false,
                        });
                    }
                    Ok(IpcResponse::Error { message }) => {
                        self.messages.push(ChatEntry::System {
                            content: message,
                            is_error: true,
                        });
                    }
                    Ok(other) => {
                        self.messages.push(ChatEntry::System {
                            content: format!("Unexpected response: {other:?}"),
                            is_error: true,
                        });
                    }
                    Err(err) => {
                        self.messages.push(ChatEntry::System {
                            content: format!("IPC request failed: {err}"),
                            is_error: true,
                        });
                    }
                }
            }
            Message::DismissBanner => {
                self.banner = None;
            }
            Message::ConfirmDecision { id, approved } => {
                if let Some(card) = self
                    .pending_confirmations
                    .iter_mut()
                    .find(|card| card.id == id)
                {
                    if card.busy {
                        return Task::none();
                    }
                    card.busy = true;
                } else {
                    return Task::none();
                }

                let client = self.client.clone();
                let req_id = id.clone();
                return Task::perform(
                    async move {
                        client
                            .confirm_action(req_id, approved)
                            .await
                            .map_err(|e| e.to_string())
                    },
                    move |result| Message::ConfirmCompleted {
                        id,
                        approved,
                        result,
                    },
                );
            }
            Message::ConfirmCompleted {
                id,
                approved,
                result,
            } => {
                self.pending_confirmations.retain(|card| card.id != id);
                let verb = if approved { "Approved" } else { "Denied" };

                match result {
                    Ok(IpcResponse::Ok { message }) => {
                        self.messages.push(ChatEntry::System {
                            content: format!("{verb} action ({id}): {message}"),
                            is_error: false,
                        });
                    }
                    Ok(IpcResponse::Chat { response, .. }) => {
                        self.messages
                            .push(ChatEntry::Assistant { content: response });
                    }
                    Ok(IpcResponse::CodeResult {
                        code,
                        output,
                        success,
                    }) => {
                        self.messages.push(ChatEntry::CodeResult {
                            code,
                            output,
                            success,
                        });
                    }
                    Ok(IpcResponse::Error { message }) => {
                        self.messages.push(ChatEntry::System {
                            content: format!("{verb} action ({id}) error: {message}"),
                            is_error: true,
                        });
                    }
                    Ok(other) => {
                        self.messages.push(ChatEntry::System {
                            content: format!("{verb} action ({id}): {other:?}"),
                            is_error: false,
                        });
                    }
                    Err(err) => {
                        self.messages.push(ChatEntry::System {
                            content: format!("Failed to confirm action ({id}): {err}"),
                            is_error: true,
                        });
                    }
                }
            }
        }

        Task::none()
    }

    /// Render the dark minimal OS shell UI.
    pub fn view(&self) -> Element<'_, Message> {
        let header = self.view_header();
        let banner = self.view_banner();
        let conversation = self.view_conversation();
        let confirmations = self.view_confirmations();
        let input_bar = self.view_input_bar();

        let mut root_col = Column::new().spacing(10).padding(14).push(header);
        if let Some(banner_el) = banner {
            root_col = root_col.push(banner_el);
        }
        if let Some(drawer_el) = self.view_drawer() {
            let body = row![
                container(conversation)
                    .width(Length::FillPortion(3))
                    .height(Length::Fill),
                container(drawer_el)
                    .width(Length::Fixed(460.0))
                    .height(Length::Fill),
            ]
            .spacing(10)
            .width(Length::Fill)
            .height(Length::Fill);
            root_col = root_col.push(body);
        } else {
            root_col = root_col.push(conversation);
        }
        if let Some(conf_el) = confirmations {
            root_col = root_col.push(conf_el);
        }
        root_col = root_col.push(input_bar);

        container(root_col)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(root_container_style)
            .into()
    }

    fn fetch_status_task(&self) -> Task<Message> {
        let status_client = self.client.clone();
        Task::perform(
            async move {
                status_client
                    .fetch_status()
                    .await
                    .map_err(|e| e.to_string())
            },
            Message::StatusFetched,
        )
    }

    fn fetch_sync_status_task(&self) -> Task<Message> {
        let sync_client = self.client.clone();
        Task::perform(
            async move {
                sync_client
                    .fetch_sync_status()
                    .await
                    .map_err(|e| e.to_string())
            },
            Message::SyncStatusFetched,
        )
    }

    fn fetch_providers_task(&self) -> Task<Message> {
        let providers_client = self.client.clone();
        Task::perform(
            async move {
                providers_client
                    .fetch_providers()
                    .await
                    .map_err(|e| e.to_string())
            },
            Message::ProvidersFetched,
        )
    }

    fn fetch_all_status_tasks(&self) -> Task<Message> {
        Task::batch([
            self.fetch_status_task(),
            self.fetch_sync_status_task(),
            self.fetch_providers_task(),
        ])
    }

    fn handle_system_event(&mut self, event: SystemEvent) {
        let summary = match &event {
            SystemEvent::ExecutionFailed {
                language,
                failure_class,
                exit_code,
                signal,
                timed_out,
                sandbox_mode,
            } => {
                let detail = if *timed_out {
                    "timed out".to_string()
                } else if let Some(sig) = signal {
                    format!("signal {sig}")
                } else if let Some(code) = exit_code {
                    format!("exit {code}")
                } else {
                    "failed".to_string()
                };
                let msg = format!(
                    "Execution failed ({language}, {failure_class}, {detail}) in {sandbox_mode}"
                );
                self.banner = Some(BannerNotice {
                    severity: BannerSeverity::Error,
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::RecoveryDecided {
                decision,
                reason,
                verified,
                attempts,
            } => {
                let status = if *verified { "verified" } else { "unverified" };
                let msg = format!("Recovery: {decision} (attempt {attempts}, {status}) — {reason}");
                self.banner = Some(BannerNotice {
                    severity: if *verified {
                        BannerSeverity::Info
                    } else {
                        BannerSeverity::Warning
                    },
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::SyncStateChanged {
                peers_connected,
                state,
            } => {
                self.mesh_connected_peers = *peers_connected;
                let msg = format!("Mesh sync {state} ({peers_connected} peers connected)");
                self.banner = Some(BannerNotice {
                    severity: BannerSeverity::Info,
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::ProviderHealthChanged {
                provider,
                available,
                detail,
            } => {
                let state = if *available { "healthy" } else { "unavailable" };
                let msg = format!("Provider {provider} {state}: {detail}");
                self.banner = Some(BannerNotice {
                    severity: if *available {
                        BannerSeverity::Success
                    } else {
                        BannerSeverity::Warning
                    },
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::McpServerRestarted { name } => {
                let msg = format!("MCP server '{name}' restarted");
                self.banner = Some(BannerNotice {
                    severity: BannerSeverity::Warning,
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::ToolCalled {
                tool_name,
                server_name,
                success,
                response_time_ms,
            } => {
                let outcome = if *success { "ok" } else { "failed" };
                let msg =
                    format!("Tool {server_name}/{tool_name} {outcome} ({response_time_ms}ms)");
                self.banner = Some(BannerNotice {
                    severity: if *success {
                        BannerSeverity::Info
                    } else {
                        BannerSeverity::Warning
                    },
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::CapabilityCreated { name, language, .. } => {
                let msg = format!("Capability created: {name} ({language})");
                self.banner = Some(BannerNotice {
                    severity: BannerSeverity::Success,
                    summary: msg.clone(),
                });
                msg
            }
            SystemEvent::ConfirmationRequired {
                id,
                session_id,
                command,
                reason,
            } => {
                if session_id == &self.session_id {
                    self.push_confirmation_card(PendingConfirmationCard {
                        id: id.clone(),
                        action: "execute".to_string(),
                        code: command.clone(),
                        reason: reason.clone(),
                        busy: false,
                    });
                }
                format!("ConfirmationRequired ({id}) for session {session_id}")
            }
        };

        self.recent_events.push(summary);
        if self.recent_events.len() > MAX_RECENT_EVENTS {
            let excess = self.recent_events.len() - MAX_RECENT_EVENTS;
            self.recent_events.drain(0..excess);
        }
    }

    fn push_confirmation_card(&mut self, card: PendingConfirmationCard) {
        if !self
            .pending_confirmations
            .iter()
            .any(|existing| existing.id == card.id)
        {
            self.pending_confirmations.push(card);
        }
    }

    fn view_header(&self) -> Element<'_, Message> {
        let (dot_color, conn_label) = match &self.connection {
            ConnectionState::Connected => (ACCENT_EMERALD, "Connected".to_string()),
            ConnectionState::Connecting => (ACCENT_AMBER, "Connecting...".to_string()),
            ConnectionState::Reconnecting(_) => (ACCENT_ROSE, "Reconnecting...".to_string()),
        };

        let status_dot =
            container(Space::new().width(8).height(8)).style(move |_| container::Style {
                background: Some(Background::Color(dot_color)),
                border: Border {
                    radius: 4.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            });

        let title_badge = row![
            text("MYCEL OS")
                .size(15)
                .font(Font::MONOSPACE)
                .color(ACCENT_CYAN),
            status_dot,
            text(conn_label).size(13).color(dot_color),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let model_label = self.active_model.as_deref().unwrap_or("model: —");
        let version_label = self
            .runtime_version
            .as_deref()
            .map(|v| format!("v{v}"))
            .unwrap_or_else(|| "v—".to_string());
        let peers_label = format!(
            "peers: {}/{}",
            self.mesh_connected_peers, self.mesh_trusted_peers
        );

        let provider_label = match self.provider {
            LlmProvider::Auto => "Provider: Auto",
            LlmProvider::Local => "Provider: Local",
            LlmProvider::Cloud => "Provider: Cloud",
        };

        let provider_btn = button(text(provider_label).size(12))
            .padding([5, 10])
            .style(secondary_button_style)
            .on_press(Message::CycleProvider);

        let providers_drawer_btn = button(text("Providers").size(12))
            .padding([5, 10])
            .style(if self.active_drawer == Some(ActiveDrawer::Providers) {
                active_drawer_button_style
            } else {
                secondary_button_style
            })
            .on_press(Message::ToggleDrawer(ActiveDrawer::Providers));

        let mesh_drawer_btn = button(text("Mesh").size(12))
            .padding([5, 10])
            .style(if self.active_drawer == Some(ActiveDrawer::Mesh) {
                active_drawer_button_style
            } else {
                secondary_button_style
            })
            .on_press(Message::ToggleDrawer(ActiveDrawer::Mesh));

        let refresh_btn = button(text("Refresh").size(12))
            .padding([5, 10])
            .style(secondary_button_style)
            .on_press(Message::RefreshStatus);

        let right_controls = row![
            text(version_label).size(12).color(TEXT_MUTED),
            text(model_label).size(12).color(TEXT_PRIMARY),
            text(peers_label).size(12).color(TEXT_MUTED),
            provider_btn,
            providers_drawer_btn,
            mesh_drawer_btn,
            refresh_btn,
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let header_row = row![
            title_badge,
            Space::new().width(Length::Fill),
            right_controls
        ]
        .align_y(Alignment::Center)
        .width(Length::Fill);

        container(header_row)
            .padding([10, 14])
            .width(Length::Fill)
            .style(surface_card_style)
            .into()
    }

    fn view_banner(&self) -> Option<Element<'_, Message>> {
        let notice = self.banner.as_ref()?;
        let accent = match notice.severity {
            BannerSeverity::Info => ACCENT_CYAN,
            BannerSeverity::Warning => ACCENT_AMBER,
            BannerSeverity::Error => ACCENT_ROSE,
            BannerSeverity::Success => ACCENT_EMERALD,
        };

        let content = row![
            text(&notice.summary).size(13).color(accent),
            Space::new().width(Length::Fill),
            button(text("×").size(14))
                .padding([2, 8])
                .style(secondary_button_style)
                .on_press(Message::DismissBanner),
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        Some(
            container(content)
                .padding([8, 12])
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(BG_SURFACE)),
                    border: Border {
                        color: accent,
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..Default::default()
                })
                .into(),
        )
    }

    fn view_conversation(&self) -> Element<'_, Message> {
        let mut items = Column::new().spacing(10).width(Length::Fill);

        if self.messages.is_empty() && self.streaming_buffer.is_none() {
            items = items.push(
                container(
                    text("Ask Mycel OS to run a command, inspect system state, or start a task.")
                        .size(14)
                        .color(TEXT_MUTED),
                )
                .padding(16)
                .width(Length::Fill),
            );
        }

        for entry in &self.messages {
            items = items.push(self.view_chat_entry(entry));
        }

        if let Some(buffer) = &self.streaming_buffer {
            let display = if buffer.is_empty() {
                "Thinking...".to_string()
            } else {
                format!("{buffer}▌")
            };
            items = items.push(
                container(
                    column![
                        text("Assistant (streaming)").size(11).color(ACCENT_CYAN),
                        text(display).size(14).color(TEXT_PRIMARY),
                    ]
                    .spacing(4),
                )
                .padding(12)
                .width(Length::Fill)
                .style(elevated_card_style),
            );
        }

        let scroll = scrollable(items).width(Length::Fill).height(Length::Fill);

        container(scroll)
            .padding(10)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(surface_card_style)
            .into()
    }

    fn view_chat_entry<'a>(&self, entry: &'a ChatEntry) -> Element<'a, Message> {
        match entry {
            ChatEntry::User { content } => container(
                column![
                    text("You").size(11).color(ACCENT_EMERALD),
                    text(content).size(14).color(TEXT_PRIMARY),
                ]
                .spacing(4),
            )
            .padding(12)
            .width(Length::Fill)
            .style(elevated_card_style)
            .into(),
            ChatEntry::Assistant { content } => container(
                column![
                    text("Mycel").size(11).color(ACCENT_CYAN),
                    text(content).size(14).color(TEXT_PRIMARY),
                ]
                .spacing(4),
            )
            .padding(12)
            .width(Length::Fill)
            .style(elevated_card_style)
            .into(),
            ChatEntry::System { content, is_error } => {
                let label_color = if *is_error { ACCENT_ROSE } else { TEXT_MUTED };
                container(
                    column![
                        text(if *is_error { "Error" } else { "System" })
                            .size(11)
                            .color(label_color),
                        text(content).size(13).color(label_color),
                    ]
                    .spacing(4),
                )
                .padding(10)
                .width(Length::Fill)
                .style(move |_| container::Style {
                    background: Some(Background::Color(BG_CODE)),
                    border: Border {
                        color: if *is_error {
                            ACCENT_ROSE
                        } else {
                            BORDER_SUBTLE
                        },
                        width: 1.0,
                        radius: 6.0.into(),
                    },
                    ..Default::default()
                })
                .into()
            }
            ChatEntry::CodeResult {
                code,
                output,
                success,
            } => {
                let badge_color = if *success {
                    ACCENT_EMERALD
                } else {
                    ACCENT_ROSE
                };
                let status_text = if *success {
                    "Code Execution (ok)"
                } else {
                    "Code Execution (failed)"
                };

                container(
                    column![
                        text(status_text).size(11).color(badge_color),
                        container(
                            text(code)
                                .size(13)
                                .font(Font::MONOSPACE)
                                .color(TEXT_PRIMARY)
                        )
                        .padding(8)
                        .width(Length::Fill)
                        .style(code_block_style),
                        text(output)
                            .size(13)
                            .font(Font::MONOSPACE)
                            .color(TEXT_MUTED),
                    ]
                    .spacing(6),
                )
                .padding(12)
                .width(Length::Fill)
                .style(elevated_card_style)
                .into()
            }
        }
    }

    fn view_confirmations(&self) -> Option<Element<'_, Message>> {
        if self.pending_confirmations.is_empty() {
            return None;
        }

        let mut cards = Column::new().spacing(8).width(Length::Fill);
        for card in &self.pending_confirmations {
            let mut approve_btn = button(text("Approve").size(13))
                .padding([6, 14])
                .style(approve_button_style);
            let mut deny_btn = button(text("Deny").size(13))
                .padding([6, 14])
                .style(deny_button_style);

            if !card.busy {
                approve_btn = approve_btn.on_press(Message::ConfirmDecision {
                    id: card.id.clone(),
                    approved: true,
                });
                deny_btn = deny_btn.on_press(Message::ConfirmDecision {
                    id: card.id.clone(),
                    approved: false,
                });
            }

            let actions_row: Row<'_, Message> = row![
                approve_btn,
                deny_btn,
                Space::new().width(Length::Fill),
                text(format!("id: {}", card.id)).size(11).color(TEXT_MUTED),
            ]
            .spacing(10)
            .align_y(Alignment::Center);

            let card_box = container(
                column![
                    text(format!("Confirmation Required — {}", card.action))
                        .size(13)
                        .color(ACCENT_AMBER),
                    text(&card.reason).size(13).color(TEXT_PRIMARY),
                    container(
                        text(&card.code)
                            .size(13)
                            .font(Font::MONOSPACE)
                            .color(TEXT_PRIMARY)
                    )
                    .padding(8)
                    .width(Length::Fill)
                    .style(code_block_style),
                    actions_row,
                ]
                .spacing(8),
            )
            .padding(12)
            .width(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(BG_SURFACE)),
                border: Border {
                    color: ACCENT_AMBER,
                    width: 1.5,
                    radius: 8.0.into(),
                },
                ..Default::default()
            });

            cards = cards.push(card_box);
        }

        Some(cards.into())
    }

    fn view_input_bar(&self) -> Element<'_, Message> {
        let input = text_input("Ask Mycel OS...", &self.input_value)
            .on_input(Message::InputChanged)
            .on_submit(Message::SendPrompt)
            .padding(10)
            .size(14)
            .width(Length::Fill);

        let can_send = !self.in_flight && !self.input_value.trim().is_empty();
        let mut send_btn = button(text(if self.in_flight { "..." } else { "Send" }).size(14))
            .padding([10, 18])
            .style(primary_button_style);

        if can_send {
            send_btn = send_btn.on_press(Message::SendPrompt);
        }

        row![input, send_btn]
            .spacing(10)
            .align_y(Alignment::Center)
            .width(Length::Fill)
            .into()
    }

    fn view_drawer(&self) -> Option<Element<'_, Message>> {
        match self.active_drawer? {
            ActiveDrawer::Providers => Some(self.view_providers_drawer()),
            ActiveDrawer::Mesh => Some(self.view_mesh_drawer()),
        }
    }

    fn view_providers_drawer(&self) -> Element<'_, Message> {
        let title_row = row![
            text("Cloud Providers")
                .size(15)
                .font(Font::MONOSPACE)
                .color(ACCENT_CYAN),
            Space::new().width(Length::Fill),
            button(text("×").size(14))
                .padding([2, 8])
                .style(secondary_button_style)
                .on_press(Message::ToggleDrawer(ActiveDrawer::Providers)),
        ]
        .align_y(Alignment::Center);

        let (active_label, counts_label, sorted_providers) =
            if let Some(snap) = &self.providers_snapshot {
                let active_p = snap.active_provider.as_deref().unwrap_or("none");
                let active_m = snap.active_model.as_deref().unwrap_or("—");
                let configured_count = snap.providers.iter().filter(|p| p.configured).count();
                let total_count = snap.providers.len();
                let mut ordered: Vec<&ProviderSummary> = snap.providers.iter().collect();
                ordered.sort_by_key(|p| std::cmp::Reverse(p.configured));
                (
                    format!("Active: {active_p} ({active_m})"),
                    format!("Configured: {configured_count} / {total_count}"),
                    ordered,
                )
            } else {
                (
                    "Active: —".to_string(),
                    "Configured: 0 / 0".to_string(),
                    Vec::new(),
                )
            };

        let summary_card = container(
            column![
                text(active_label).size(13).color(TEXT_PRIMARY),
                text(counts_label).size(12).color(TEXT_MUTED),
            ]
            .spacing(4),
        )
        .padding(10)
        .width(Length::Fill)
        .style(elevated_card_style);

        let mut list_col = Column::new().spacing(8).width(Length::Fill);
        if sorted_providers.is_empty() {
            list_col = list_col.push(
                text("No provider catalog loaded yet.")
                    .size(12)
                    .color(TEXT_MUTED),
            );
        } else {
            let active_provider_id = self
                .providers_snapshot
                .as_ref()
                .and_then(|s| s.active_provider.as_deref());

            for p in sorted_providers {
                let is_selected = self.selected_provider_id.as_deref() == Some(p.id.as_str());
                let is_active = active_provider_id == Some(p.id.as_str());

                let badge_text = if p.configured {
                    "configured"
                } else {
                    "unconfigured"
                };
                let badge_color = if p.configured {
                    ACCENT_EMERALD
                } else {
                    TEXT_MUTED
                };

                let mut header_bits = row![
                    text(&p.name).size(13).color(TEXT_PRIMARY),
                    text(format!("({})", p.id))
                        .size(12)
                        .font(Font::MONOSPACE)
                        .color(TEXT_MUTED),
                    Space::new().width(Length::Fill),
                ]
                .spacing(6)
                .align_y(Alignment::Center);

                if is_active {
                    header_bits = header_bits.push(text("active").size(11).color(ACCENT_CYAN));
                }
                header_bits = header_bits.push(text(badge_text).size(11).color(badge_color));

                let details = column![
                    header_bits,
                    text(format!("api: {}  |  env: {}", p.api, p.env_var))
                        .size(11)
                        .font(Font::MONOSPACE)
                        .color(TEXT_MUTED),
                    text(format!("default_model: {}", p.default_model))
                        .size(11)
                        .font(Font::MONOSPACE)
                        .color(TEXT_MUTED),
                ]
                .spacing(4);

                let row_btn = button(details)
                    .padding(10)
                    .width(Length::Fill)
                    .style(move |_theme, status| {
                        let bg = if is_selected {
                            Color::from_rgb(0.16, 0.21, 0.29)
                        } else if matches!(
                            status,
                            button::Status::Hovered | button::Status::Pressed
                        ) {
                            Color::from_rgb(0.16, 0.19, 0.25)
                        } else {
                            BG_ELEVATED
                        };
                        let border_color = if is_selected {
                            ACCENT_CYAN
                        } else {
                            BORDER_SUBTLE
                        };
                        button::Style {
                            background: Some(Background::Color(bg)),
                            text_color: TEXT_PRIMARY,
                            border: Border {
                                color: border_color,
                                width: if is_selected { 1.5 } else { 1.0 },
                                radius: 6.0.into(),
                            },
                            ..Default::default()
                        }
                    })
                    .on_press(Message::SelectProvider(p.id.clone()));

                list_col = list_col.push(row_btn);
            }
        }

        let selected_label = match &self.selected_provider_id {
            Some(id) => format!("Selected provider: {id}"),
            None => "Select a provider above to configure".to_string(),
        };

        let model_input = text_input(
            "Optional model override (uses default if empty)",
            &self.provider_model_input,
        )
        .on_input(Message::ProviderModelInputChanged)
        .on_submit(Message::ApplyProvider)
        .padding(8)
        .size(12)
        .width(Length::Fill);

        let can_apply = self.selected_provider_id.is_some() && !self.provider_busy;
        let mut apply_btn = button(
            text(if self.provider_busy {
                "Applying..."
            } else {
                "Apply Provider"
            })
            .size(12),
        )
        .padding([8, 14])
        .style(primary_button_style);

        if can_apply {
            apply_btn = apply_btn.on_press(Message::ApplyProvider);
        }

        let mut controls_col = column![
            text(selected_label).size(12).color(TEXT_PRIMARY),
            model_input,
            row![apply_btn].align_y(Alignment::Center),
        ]
        .spacing(8);

        if let Some((msg, is_err)) = &self.provider_feedback {
            controls_col = controls_col.push(text(msg).size(12).color(if *is_err {
                ACCENT_ROSE
            } else {
                ACCENT_EMERALD
            }));
        }

        let body = column![
            title_row,
            summary_card,
            scrollable(list_col)
                .width(Length::Fill)
                .height(Length::Fill),
            container(controls_col)
                .padding(10)
                .width(Length::Fill)
                .style(elevated_card_style),
        ]
        .spacing(10)
        .width(Length::Fill)
        .height(Length::Fill);

        container(body)
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(surface_card_style)
            .into()
    }

    fn view_mesh_drawer(&self) -> Element<'_, Message> {
        let title_row = row![
            text("Mesh Sync")
                .size(15)
                .font(Font::MONOSPACE)
                .color(ACCENT_CYAN),
            Space::new().width(Length::Fill),
            button(text("×").size(14))
                .padding([2, 8])
                .style(secondary_button_style)
                .on_press(Message::ToggleDrawer(ActiveDrawer::Mesh)),
        ]
        .align_y(Alignment::Center);

        let mut status_lines = Column::new().spacing(4).width(Length::Fill);
        if let Some(sync) = &self.sync_snapshot {
            let enabled_text = if sync.enabled { "enabled" } else { "disabled" };
            let enabled_color = if sync.enabled {
                ACCENT_EMERALD
            } else {
                ACCENT_AMBER
            };
            let iface = sync.interface.as_deref().unwrap_or("—");
            let listen = sync.listen_addr.as_deref().unwrap_or("—");

            status_lines = status_lines
                .push(
                    row![
                        text("Status: ").size(12).color(TEXT_MUTED),
                        text(enabled_text).size(12).color(enabled_color),
                        Space::new().width(Length::Fill),
                        text(format!(
                            "peers: {}/{}",
                            sync.connected_peers, sync.trusted_peers
                        ))
                        .size(12)
                        .color(TEXT_PRIMARY),
                    ]
                    .align_y(Alignment::Center),
                )
                .push(
                    text(format!("device_id: {}", sync.device_id))
                        .size(11)
                        .font(Font::MONOSPACE)
                        .color(TEXT_PRIMARY),
                )
                .push(
                    text(format!(
                        "transport: {}  |  iface: {}",
                        sync.transport, iface
                    ))
                    .size(11)
                    .font(Font::MONOSPACE)
                    .color(TEXT_MUTED),
                )
                .push(
                    text(format!(
                        "listen: {}  |  events: {}",
                        listen, sync.event_log_len
                    ))
                    .size(11)
                    .font(Font::MONOSPACE)
                    .color(TEXT_MUTED),
                )
                .push(
                    text(format!(
                        "Relay: {}",
                        if sync.relay_connected {
                            "Connected"
                        } else {
                            "Disconnected"
                        }
                    ))
                    .size(11)
                    .font(Font::MONOSPACE)
                    .color(if sync.relay_connected {
                        ACCENT_EMERALD
                    } else {
                        TEXT_MUTED
                    }),
                );
            if let Some(ep) = &sync.public_endpoint {
                status_lines = status_lines.push(
                    text(format!("Public Endpoint: {ep}"))
                        .size(11)
                        .font(Font::MONOSPACE)
                        .color(TEXT_MUTED),
                );
            }
        } else {
            status_lines = status_lines.push(
                text("Mesh status not loaded yet.")
                    .size(12)
                    .color(TEXT_MUTED),
            );
        }

        let status_card = container(status_lines)
            .padding(10)
            .width(Length::Fill)
            .style(elevated_card_style);

        let mut peers_col = Column::new().spacing(6).width(Length::Fill);
        peers_col = peers_col.push(text("Trusted Peers").size(12).color(TEXT_PRIMARY));
        match self.sync_snapshot.as_ref() {
            Some(sync) if !sync.peers.is_empty() => {
                for peer in &sync.peers {
                    let (status_str, status_color) = match peer.status {
                        PeerStatus::Connected => ("Connected", ACCENT_EMERALD),
                        PeerStatus::Disconnected => ("Disconnected", TEXT_MUTED),
                        PeerStatus::Pairing => ("Pairing", ACCENT_AMBER),
                    };
                    let addrs = if peer.addresses.is_empty() {
                        "—".to_string()
                    } else {
                        peer.addresses.join(", ")
                    };
                    peers_col = peers_col.push(
                        container(
                            column![
                                row![
                                    text(&peer.name).size(12).color(TEXT_PRIMARY),
                                    Space::new().width(Length::Fill),
                                    text(status_str).size(11).color(status_color),
                                ]
                                .align_y(Alignment::Center),
                                text(format!("id: {}", peer.id))
                                    .size(11)
                                    .font(Font::MONOSPACE)
                                    .color(TEXT_MUTED),
                                text(format!("addr: {addrs}"))
                                    .size(11)
                                    .font(Font::MONOSPACE)
                                    .color(TEXT_MUTED),
                            ]
                            .spacing(3),
                        )
                        .padding(8)
                        .width(Length::Fill)
                        .style(elevated_card_style),
                    );
                }
            }
            _ => {
                peers_col = peers_col.push(
                    text("No trusted peers configured.")
                        .size(12)
                        .color(TEXT_MUTED),
                );
            }
        }

        let endpoint_input = text_input(
            "Optional endpoint (e.g. 10.42.0.1:51820)",
            &self.mesh_endpoint_input,
        )
        .on_input(Message::MeshEndpointInputChanged)
        .on_submit(Message::GeneratePairingCode)
        .padding(8)
        .size(12)
        .width(Length::Fill);

        let mut gen_btn = button(text("Generate Pairing Code").size(12))
            .padding([7, 12])
            .style(secondary_button_style);
        if !self.mesh_busy {
            gen_btn = gen_btn.on_press(Message::GeneratePairingCode);
        }

        let mut sync_btn = button(text("Sync Now").size(12))
            .padding([7, 12])
            .style(primary_button_style);
        if !self.mesh_busy {
            sync_btn = sync_btn.on_press(Message::TriggerSync);
        }

        let mut pairing_section = column![
            endpoint_input,
            row![gen_btn, sync_btn]
                .spacing(8)
                .align_y(Alignment::Center),
        ]
        .spacing(8);

        if let Some(code_snap) = &self.generated_pairing_code {
            let endpoint_info = code_snap.endpoint.as_deref().unwrap_or("auto");
            pairing_section = pairing_section.push(
                container(
                    column![
                        text(format!(
                            "Pairing Code (device: {}, endpoint: {})",
                            code_snap.device_id, endpoint_info
                        ))
                        .size(11)
                        .color(ACCENT_CYAN),
                        text(&code_snap.code)
                            .size(12)
                            .font(Font::MONOSPACE)
                            .color(TEXT_PRIMARY),
                    ]
                    .spacing(4),
                )
                .padding(8)
                .width(Length::Fill)
                .style(code_block_style),
            );
        }

        let pair_input = text_input("Paste mycel-pair-v1:... or peer ID", &self.mesh_pair_input)
            .on_input(Message::MeshPairInputChanged)
            .on_submit(Message::PairPeer)
            .padding(8)
            .size(12)
            .width(Length::Fill);

        let can_pair = !self.mesh_busy && !self.mesh_pair_input.trim().is_empty();
        let mut pair_btn = button(text("Pair").size(12))
            .padding([8, 14])
            .style(primary_button_style);
        if can_pair {
            pair_btn = pair_btn.on_press(Message::PairPeer);
        }

        let mut actions_col = column![
            pairing_section,
            row![pair_input, pair_btn]
                .spacing(8)
                .align_y(Alignment::Center),
        ]
        .spacing(10);

        if let Some((msg, is_err)) = &self.mesh_feedback {
            actions_col = actions_col.push(text(msg).size(12).color(if *is_err {
                ACCENT_ROSE
            } else {
                ACCENT_EMERALD
            }));
        }

        let body = column![
            title_row,
            status_card,
            scrollable(peers_col)
                .width(Length::Fill)
                .height(Length::Fill),
            container(actions_col)
                .padding(10)
                .width(Length::Fill)
                .style(elevated_card_style),
        ]
        .spacing(10)
        .width(Length::Fill)
        .height(Length::Fill);

        container(body)
            .padding(12)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(surface_card_style)
            .into()
    }
}

struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[allow(clippy::ptr_arg)]
fn subscribe_events(socket_path: &String) -> impl futures::Stream<Item = Message> {
    let (rx, handle) = spawn_event_subscriber(
        socket_path.clone(),
        None,
        Duration::from_millis(400),
        Duration::from_secs(10),
    );
    let guard = AbortOnDrop(handle);
    stream::unfold((rx, guard), |(mut rx, guard)| async move {
        rx.recv()
            .await
            .map(|ev| (Message::SubscriberUpdate(ev), (rx, guard)))
    })
}

fn chat_stream(
    client: IpcRequestClient,
    prompt: String,
    provider: LlmProvider,
) -> impl futures::Stream<Item = Message> {
    let (msg_tx, msg_rx) = mpsc::unbounded_channel::<Message>();
    let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel::<String>();

    let forward_tx = msg_tx.clone();
    let forward_handle = tokio::spawn(async move {
        while let Some(delta) = chunk_rx.recv().await {
            if forward_tx.send(Message::ChatChunk(delta)).is_err() {
                break;
            }
        }
    });

    tokio::spawn(async move {
        let result = client
            .send_chat_streaming(prompt, provider, chunk_tx)
            .await
            .map_err(|e| e.to_string());
        // Wait for all buffered chunks to be forwarded before emitting ChatFinished.
        let _ = forward_handle.await;
        let _ = msg_tx.send(Message::ChatFinished(result));
    });

    stream::unfold(msg_rx, |mut rx| async move {
        rx.recv().await.map(|msg| (msg, rx))
    })
}

fn root_container_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_ROOT)),
        text_color: Some(TEXT_PRIMARY),
        ..Default::default()
    }
}

fn surface_card_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_SURFACE)),
        border: Border {
            color: BORDER_SUBTLE,
            width: 1.0,
            radius: 8.0.into(),
        },
        ..Default::default()
    }
}

fn elevated_card_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_ELEVATED)),
        border: Border {
            color: BORDER_SUBTLE,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

fn code_block_style(_theme: &iced::Theme) -> container::Style {
    container::Style {
        background: Some(Background::Color(BG_CODE)),
        border: Border {
            color: BORDER_SUBTLE,
            width: 1.0,
            radius: 4.0.into(),
        },
        ..Default::default()
    }
}

fn secondary_button_style(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => Color::from_rgb(0.18, 0.21, 0.27),
        _ => BG_ELEVATED,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: TEXT_PRIMARY,
        border: Border {
            color: BORDER_SUBTLE,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

fn active_drawer_button_style(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Hovered | button::Status::Pressed => Color::from_rgb(0.18, 0.30, 0.40),
        _ => Color::from_rgb(0.13, 0.24, 0.34),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: ACCENT_CYAN,
        border: Border {
            color: ACCENT_CYAN,
            width: 1.0,
            radius: 6.0.into(),
        },
        ..Default::default()
    }
}

fn primary_button_style(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let (bg, text_color) = match status {
        button::Status::Disabled => (BG_ELEVATED, TEXT_MUTED),
        button::Status::Hovered | button::Status::Pressed => {
            (Color::from_rgb(0.14, 0.62, 0.86), BG_ROOT)
        }
        button::Status::Active => (ACCENT_CYAN, BG_ROOT),
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color,
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn approve_button_style(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Disabled => BG_ELEVATED,
        button::Status::Hovered | button::Status::Pressed => Color::from_rgb(0.16, 0.72, 0.50),
        button::Status::Active => ACCENT_EMERALD,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: BG_ROOT,
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn deny_button_style(_theme: &iced::Theme, status: button::Status) -> button::Style {
    let bg = match status {
        button::Status::Disabled => BG_ELEVATED,
        button::Status::Hovered | button::Status::Pressed => Color::from_rgb(0.86, 0.34, 0.42),
        button::Status::Active => ACCENT_ROSE,
    };
    button::Style {
        background: Some(Background::Color(bg)),
        text_color: BG_ROOT,
        border: Border {
            radius: 6.0.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_app() -> MycelShellApp {
        MycelShellApp::new("/tmp/mycel-test.sock".to_string())
    }

    #[test]
    fn subscriber_connected_and_disconnected_transitions() {
        let mut app = test_app();
        assert_eq!(app.connection, ConnectionState::Connecting);

        let _task = app.update(Message::SubscriberUpdate(SubscriberEvent::Connected));
        assert_eq!(app.connection, ConnectionState::Connected);

        let _task = app.update(Message::SubscriberUpdate(SubscriberEvent::Disconnected(
            "EOF".to_string(),
        )));
        assert_eq!(
            app.connection,
            ConnectionState::Reconnecting("EOF".to_string())
        );

        let _ = app.view();
    }

    #[test]
    fn live_chat_chunk_streaming_and_finalization() {
        let mut app = test_app();
        app.in_flight = true;
        app.streaming_buffer = Some(String::new());
        app.messages.push(ChatEntry::User {
            content: "hello".to_string(),
        });

        let _ = app.update(Message::ChatChunk("Hello ".to_string()));
        let _ = app.update(Message::ChatChunk("world!".to_string()));
        assert_eq!(app.streaming_buffer.as_deref(), Some("Hello world!"));
        let _ = app.view();

        let _ = app.update(Message::ChatFinished(Ok(IpcResponse::Chat {
            response: "Hello world!".to_string(),
            surface: None,
        })));
        assert!(!app.in_flight);
        assert!(app.streaming_buffer.is_none());
        assert_eq!(app.messages.len(), 2);
        assert_eq!(
            app.messages[1],
            ChatEntry::Assistant {
                content: "Hello world!".to_string()
            }
        );
        let _ = app.view();
    }

    #[test]
    fn system_events_update_banner_and_mesh_peer_count() {
        let mut app = test_app();

        let _ = app.update(Message::SubscriberUpdate(SubscriberEvent::Event(
            SystemEvent::ExecutionFailed {
                language: "bash".to_string(),
                failure_class: "non_zero_exit".to_string(),
                exit_code: Some(1),
                signal: None,
                timed_out: false,
                sandbox_mode: "namespace".to_string(),
            },
        )));
        let banner = app.banner.as_ref().unwrap();
        assert_eq!(banner.severity, BannerSeverity::Error);
        assert!(banner.summary.contains("bash"));

        let _ = app.update(Message::SubscriberUpdate(SubscriberEvent::Event(
            SystemEvent::RecoveryDecided {
                decision: "retry".to_string(),
                reason: "transient".to_string(),
                verified: true,
                attempts: 1,
            },
        )));
        assert_eq!(app.banner.as_ref().unwrap().severity, BannerSeverity::Info);
        assert!(app.banner.as_ref().unwrap().summary.contains("retry"));

        let _ = app.update(Message::SubscriberUpdate(SubscriberEvent::Event(
            SystemEvent::ProviderHealthChanged {
                provider: "ollama".to_string(),
                available: false,
                detail: "connection refused".to_string(),
            },
        )));
        assert_eq!(
            app.banner.as_ref().unwrap().severity,
            BannerSeverity::Warning
        );
        assert!(app.banner.as_ref().unwrap().summary.contains("ollama"));

        let _ = app.update(Message::SubscriberUpdate(SubscriberEvent::Event(
            SystemEvent::SyncStateChanged {
                peers_connected: 3,
                state: "connected".to_string(),
            },
        )));
        assert_eq!(app.mesh_connected_peers, 3);
        assert!(app.banner.as_ref().unwrap().summary.contains("3 peers"));

        let _ = app.view();
    }

    #[test]
    fn confirmation_required_deduplicates_filters_foreign_sessions_and_clears_on_complete() {
        let mut app = test_app();
        let own_session = app.session_id.clone();

        // Foreign session event must be ignored.
        let _ = app.update(Message::SubscriberUpdate(SubscriberEvent::Event(
            SystemEvent::ConfirmationRequired {
                id: "conf-foreign".to_string(),
                session_id: "other-session".to_string(),
                command: "rm -rf /tmp/x".to_string(),
                reason: "destructive".to_string(),
            },
        )));
        assert!(app.pending_confirmations.is_empty());

        // Matching session event adds a card.
        let _ = app.update(Message::SubscriberUpdate(SubscriberEvent::Event(
            SystemEvent::ConfirmationRequired {
                id: "conf-1".to_string(),
                session_id: own_session,
                command: "rm -rf /tmp/test".to_string(),
                reason: "destructive".to_string(),
            },
        )));
        assert_eq!(app.pending_confirmations.len(), 1);

        // Direct IpcResponse::ConfirmationRequired with same id must deduplicate.
        app.in_flight = true;
        let _ = app.update(Message::ChatFinished(Ok(
            IpcResponse::ConfirmationRequired {
                id: "conf-1".to_string(),
                action: "execute".to_string(),
                code: "rm -rf /tmp/test".to_string(),
                reason: "destructive".to_string(),
            },
        )));
        assert_eq!(app.pending_confirmations.len(), 1);
        let _ = app.view();

        // ConfirmCompleted clears the card and records result.
        let _ = app.update(Message::ConfirmCompleted {
            id: "conf-1".to_string(),
            approved: true,
            result: Ok(IpcResponse::Ok {
                message: "Executed".to_string(),
            }),
        });
        assert!(app.pending_confirmations.is_empty());
        assert_eq!(app.messages.len(), 1);
        match &app.messages[0] {
            ChatEntry::System { content, is_error } => {
                assert!(!is_error);
                assert!(content.contains("Approved"));
                assert!(content.contains("conf-1"));
            }
            other => panic!("unexpected entry: {other:?}"),
        }
        let _ = app.view();
    }

    #[test]
    fn drawer_toggling_and_provider_state_transitions() {
        let mut app = test_app();
        assert_eq!(app.active_drawer, None);

        let _ = app.update(Message::ToggleDrawer(ActiveDrawer::Providers));
        assert_eq!(app.active_drawer, Some(ActiveDrawer::Providers));

        // Toggle same drawer closes it.
        let _ = app.update(Message::ToggleDrawer(ActiveDrawer::Providers));
        assert_eq!(app.active_drawer, None);

        // Switch from Providers to Mesh directly.
        let _ = app.update(Message::ToggleDrawer(ActiveDrawer::Providers));
        let _ = app.update(Message::ToggleDrawer(ActiveDrawer::Mesh));
        assert_eq!(app.active_drawer, Some(ActiveDrawer::Mesh));

        // ProvidersFetched populates catalog and pre-selects active provider.
        let _ = app.update(Message::ProvidersFetched(Ok(ProvidersSnapshot {
            active_provider: Some("venice".to_string()),
            active_model: Some("llama-3.3-70b".to_string()),
            providers: vec![
                ProviderSummary {
                    id: "openrouter".to_string(),
                    name: "OpenRouter".to_string(),
                    api: "openai-compatible".to_string(),
                    configured: false,
                    env_var: "OPENROUTER_API_KEY".to_string(),
                    default_model: "anthropic/claude-sonnet-4".to_string(),
                    base_url: "https://openrouter.ai/api/v1".to_string(),
                },
                ProviderSummary {
                    id: "venice".to_string(),
                    name: "Venice AI".to_string(),
                    api: "openai-compatible".to_string(),
                    configured: true,
                    env_var: "VENICE_API_KEY".to_string(),
                    default_model: "llama-3.3-70b".to_string(),
                    base_url: "https://api.venice.ai/api/v1".to_string(),
                },
            ],
        })));
        assert_eq!(app.selected_provider_id.as_deref(), Some("venice"));

        // Selecting another provider clears model override and feedback.
        let _ = app.update(Message::ProviderModelInputChanged("custom-m".to_string()));
        let _ = app.update(Message::SelectProvider("openrouter".to_string()));
        assert_eq!(app.selected_provider_id.as_deref(), Some("openrouter"));
        assert!(app.provider_model_input.is_empty());

        let _ = app.update(Message::ToggleDrawer(ActiveDrawer::Providers));
        let _ = app.view();

        app.provider_busy = true;
        let _ = app.update(Message::SetProviderCompleted(Ok(
            "Active cloud provider set to openrouter/anthropic/claude-sonnet-4".to_string(),
        )));
        assert!(!app.provider_busy);
        assert_eq!(
            app.provider_feedback
                .as_ref()
                .map(|(m, err)| (m.as_str(), *err)),
            Some((
                "Active cloud provider set to openrouter/anthropic/claude-sonnet-4",
                false
            ))
        );
        let _ = app.view();
    }

    #[test]
    fn mesh_drawer_state_transitions_and_view_rendering() {
        use crate::protocol::PeerInfo;

        let mut app = test_app();
        let _ = app.update(Message::ToggleDrawer(ActiveDrawer::Mesh));
        assert_eq!(app.active_drawer, Some(ActiveDrawer::Mesh));

        let _ = app.update(Message::SyncStatusFetched(Ok(SyncSnapshot {
            enabled: true,
            device_id: "node-1".to_string(),
            transport: "wireguard".to_string(),
            interface: Some("mycel0".to_string()),
            listen_addr: Some("0.0.0.0:51820".to_string()),
            public_endpoint: Some("203.0.113.5:51820".to_string()),
            relay_connected: true,
            trusted_peers: 1,
            connected_peers: 1,
            event_log_len: 10,
            peers: vec![PeerInfo {
                id: "node-2".to_string(),
                name: "desktop".to_string(),
                status: PeerStatus::Connected,
                addresses: vec!["10.42.0.2:51820".to_string()],
            }],
        })));
        assert!(app.mesh_enabled);
        assert_eq!(app.mesh_connected_peers, 1);
        let snap = app.sync_snapshot.as_ref().unwrap();
        assert_eq!(snap.device_id, "node-1");
        assert_eq!(snap.public_endpoint.as_deref(), Some("203.0.113.5:51820"));
        assert!(snap.relay_connected);
        let _ = app.view();

        app.mesh_busy = true;
        let _ = app.update(Message::PairingCodeGenerated(Ok(PairingCodeSnapshot {
            device_id: "node-1".to_string(),
            code: "mycel-pair-v1:node-1@10.42.0.1:51820".to_string(),
            endpoint: Some("10.42.0.1:51820".to_string()),
        })));
        assert!(!app.mesh_busy);
        assert_eq!(
            app.generated_pairing_code.as_ref().map(|c| c.code.as_str()),
            Some("mycel-pair-v1:node-1@10.42.0.1:51820")
        );
        let _ = app.view();

        let _ = app.update(Message::MeshPairInputChanged(
            "mycel-pair-v1:node-3@10.42.0.3:51820".to_string(),
        ));
        app.mesh_busy = true;
        let _ = app.update(Message::PairPeerCompleted(Ok(PeerAddedSnapshot {
            peer: PeerInfo {
                id: "node-3".to_string(),
                name: "laptop".to_string(),
                status: PeerStatus::Connected,
                addresses: vec!["10.42.0.3:51820".to_string()],
            },
            synced_events: 4,
        })));
        assert!(!app.mesh_busy);
        assert!(app.mesh_pair_input.is_empty());
        assert!(app
            .mesh_feedback
            .as_ref()
            .unwrap()
            .0
            .contains("Paired laptop"));

        app.mesh_busy = true;
        let _ = app.update(Message::TriggerSyncCompleted(Ok(
            "Synced 2 event(s) across mesh".to_string(),
        )));
        assert!(!app.mesh_busy);
        assert_eq!(
            app.mesh_feedback
                .as_ref()
                .map(|(m, err)| (m.as_str(), *err)),
            Some(("Synced 2 event(s) across mesh", false))
        );
        let _ = app.view();
    }
}
