//! Compositor event loop, socket binding, and headless runner.

use std::{os::unix::net::UnixStream, sync::Arc, time::Duration};

use anyhow::Context;
use calloop::{
    generic::Generic,
    signals::{Signal, Signals},
    EventLoop, Interest, LoopSignal, Mode, PostAction,
};
use smithay::{reexports::wayland_server::Display, wayland::socket::ListeningSocketSource};
use tracing::{debug, error, info, warn};

use crate::{
    shell_proc::ShellProcess,
    state::{ClientState, MycelCompositorState},
};

/// Configuration for starting the Mycel Wayland compositor.
#[derive(Debug, Clone)]
pub struct CompositorConfig {
    pub socket_name: Option<String>,
    pub shell: Option<String>,
    pub width: i32,
    pub height: i32,
    pub headless: bool,
}

impl Default for CompositorConfig {
    fn default() -> Self {
        Self {
            socket_name: None,
            shell: None,
            width: 1280,
            height: 800,
            headless: true,
        }
    }
}

struct LoopData {
    display: Display<MycelCompositorState>,
    state: MycelCompositorState,
    shell_proc: Option<ShellProcess>,
}

/// Run the compositor according to `config`.
pub fn run_compositor(config: CompositorConfig) -> anyhow::Result<()> {
    if !config.headless {
        #[cfg(feature = "drm")]
        {
            return crate::drm_backend::run_drm(config);
        }
        #[cfg(all(feature = "winit", not(feature = "drm")))]
        {
            warn!(
                "Winit backend feature enabled, but only headless/DRM modes are wired; \
                 falling back to headless mode."
            );
        }
        #[cfg(not(any(feature = "winit", feature = "drm")))]
        {
            anyhow::bail!(
                "No graphical backend compiled in (rebuild with `--features winit` or `--features drm`), \
                 or pass `--headless` to run in headless mode."
            );
        }
    }

    run_headless(config, true, |_, _| {})
}

/// Run the headless compositor event loop, invoking `on_ready` once the Wayland socket is bound.
pub fn run_headless<F>(
    config: CompositorConfig,
    handle_signals: bool,
    on_ready: F,
) -> anyhow::Result<()>
where
    F: FnOnce(String, LoopSignal),
{
    let mut event_loop: EventLoop<LoopData> =
        EventLoop::try_new().context("failed to create calloop EventLoop")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    let mut display: Display<MycelCompositorState> =
        Display::new().context("failed to create Wayland Display")?;
    let state = MycelCompositorState::new(&display, config.width, config.height)?;

    let listening_socket = match config.socket_name.as_deref() {
        Some(name) => ListeningSocketSource::with_name(name)
            .with_context(|| format!("failed to bind Wayland socket `{name}`"))?,
        None => {
            ListeningSocketSource::new_auto().context("failed to bind automatic Wayland socket")?
        }
    };
    let socket_name = listening_socket
        .socket_name()
        .to_string_lossy()
        .into_owned();
    info!(socket_name = %socket_name, "Wayland listening socket bound");

    loop_handle
        .insert_source(listening_socket, |stream: UnixStream, _, data| {
            if let Err(err) = data
                .display
                .handle()
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                warn!(error = %err, "Failed to insert Wayland client");
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to insert listening socket source: {err}"))?;

    let display_fd = display
        .backend()
        .poll_fd()
        .try_clone_to_owned()
        .context("failed to clone Wayland display poll fd")?;
    loop_handle
        .insert_source(
            Generic::new(display_fd, Interest::READ, Mode::Level),
            |_, _, _data| Ok(PostAction::Continue),
        )
        .map_err(|err| anyhow::anyhow!("failed to insert display fd source: {err}"))?;

    if handle_signals {
        let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
            .context("failed to register SIGINT/SIGTERM signal handler")?;
        let sig_loop = loop_signal.clone();
        loop_handle
            .insert_source(signals, move |event, _, _| {
                info!(signal = ?event.signal(), "Received shutdown signal");
                sig_loop.stop();
            })
            .map_err(|err| anyhow::anyhow!("failed to insert signal source: {err}"))?;
    }

    let shell_proc = match config.shell.as_deref() {
        Some(program) => Some(ShellProcess::spawn(program, &socket_name)?),
        None => None,
    };

    let mut loop_data = LoopData {
        display,
        state,
        shell_proc,
    };

    on_ready(socket_name, loop_signal);

    event_loop
        .run(Some(Duration::from_millis(25)), &mut loop_data, |data| {
            if let Err(err) = data.display.dispatch_clients(&mut data.state) {
                error!(error = %err, "Error dispatching Wayland clients");
            }
            if let Err(err) = data.display.flush_clients() {
                error!(error = %err, "Error flushing Wayland clients");
            }
            if let Some(shell) = data.shell_proc.as_mut() {
                match shell.try_wait() {
                    Ok(Some(status)) => {
                        debug!(%status, "Observed shell process exit");
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "Error polling shell process");
                    }
                }
            }
        })
        .context("compositor event loop failed")?;

    if let Some(mut shell) = loop_data.shell_proc.take() {
        shell.terminate();
    }

    info!("Compositor shut down cleanly");
    Ok(())
}
