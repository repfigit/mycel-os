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
    spawn_event_subscriber, IpcClientConfig, IpcRequestClient, StatusSnapshot, SubscriberEvent,
    SyncSnapshot,
};
use crate::protocol::{IpcResponse, LlmProvider, SystemEvent};

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
    RefreshStatus,
    CycleProvider,
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
                self.mesh_transport = Some(sync.transport);
            }
            Message::SyncStatusFetched(Err(_err)) => {
                // Mesh sync may be disabled or unavailable during startup; keep header clean.
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
        root_col = root_col.push(conversation);
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

    fn fetch_all_status_tasks(&self) -> Task<Message> {
        let status_client = self.client.clone();
        let sync_client = self.client.clone();

        let status_task = Task::perform(
            async move {
                status_client
                    .fetch_status()
                    .await
                    .map_err(|e| e.to_string())
            },
            Message::StatusFetched,
        );
        let sync_task = Task::perform(
            async move {
                sync_client
                    .fetch_sync_status()
                    .await
                    .map_err(|e| e.to_string())
            },
            Message::SyncStatusFetched,
        );

        Task::batch([status_task, sync_task])
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

        let refresh_btn = button(text("Refresh").size(12))
            .padding([5, 10])
            .style(secondary_button_style)
            .on_press(Message::RefreshStatus);

        let right_controls = row![
            text(version_label).size(12).color(TEXT_MUTED),
            text(model_label).size(12).color(TEXT_PRIMARY),
            text(peers_label).size(12).color(TEXT_MUTED),
            provider_btn,
            refresh_btn,
        ]
        .spacing(12)
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
}
