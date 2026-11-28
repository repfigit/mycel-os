//! DRM/KMS + Pixman software rendering + libseat + libinput backend.

use std::{
    fs::OpenOptions,
    os::unix::{io::OwnedFd, net::UnixStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::Context;
use calloop::{
    generic::Generic,
    signals::{Signal, Signals},
    timer::{TimeoutAction, Timer},
    EventLoop, Interest, LoopSignal, Mode as CalloopMode, PostAction,
};
use smithay::{
    backend::{
        allocator::{dumb::DumbAllocator, Fourcc},
        drm::{
            compositor::{DrmCompositor, FrameFlags},
            DrmDevice, DrmDeviceFd, DrmEvent,
        },
        input::{
            AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event as InputEventTrait,
            InputEvent, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
        },
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{pixman::PixmanRenderer, Bind},
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::UdevBackend,
    },
    desktop::{space::space_render_elements, Window},
    input::{
        keyboard::FilterResult,
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    output::{Mode as OutputMode, Scale},
    reexports::{
        drm::control::{connector, crtc, Device as ControlDevice, Mode as DrmMode, ModeTypeFlags},
        input::Libinput,
        wayland_server::Display,
    },
    utils::{DeviceFd, Logical, Point, Size, Transform, SERIAL_COUNTER},
    wayland::socket::ListeningSocketSource,
};
use tracing::{debug, error, info, warn};

use crate::{
    compositor::CompositorConfig,
    shell_proc::ShellProcess,
    state::{ClientState, MycelCompositorState},
};

const CLEAR_COLOR: [f32; 4] = [0.07, 0.08, 0.11, 1.0];
const DEFAULT_DRM_CARD: &str = "/dev/dri/card0";
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct DrmLoopData {
    display: Display<MycelCompositorState>,
    state: MycelCompositorState,
    shell_proc: Option<ShellProcess>,
    loop_signal: LoopSignal,
    drm_device: DrmDevice,
    drm_compositor: DrmCompositor<DumbAllocator, DrmDeviceFd, (), DrmDeviceFd>,
    renderer: PixmanRenderer,
    vblank_pending: bool,
    pointer_location: Point<f64, Logical>,
    output_size: Size<i32, Logical>,
    start_time: Instant,
}

/// Run the graphical DRM/KMS + Pixman compositor event loop.
pub fn run_drm(config: CompositorConfig) -> anyhow::Result<()> {
    let mut event_loop: EventLoop<DrmLoopData> =
        EventLoop::try_new().context("failed to create calloop EventLoop")?;
    let loop_handle = event_loop.handle();
    let loop_signal = event_loop.get_signal();

    let mut display: Display<MycelCompositorState> =
        Display::new().context("failed to create Wayland Display")?;
    let mut state = MycelCompositorState::new(&display, config.width, config.height)?;

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
    info!(socket_name = %socket_name, "Wayland listening socket bound (DRM)");

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
            Generic::new(display_fd, Interest::READ, CalloopMode::Level),
            |_, _, _| Ok(PostAction::Continue),
        )
        .map_err(|err| anyhow::anyhow!("failed to insert display fd source: {err}"))?;

    let signals = Signals::new(&[Signal::SIGINT, Signal::SIGTERM])
        .context("failed to register SIGINT/SIGTERM signal handler")?;
    let sig_loop = loop_signal.clone();
    loop_handle
        .insert_source(signals, move |event, _, _| {
            info!(signal = ?event.signal(), "Received shutdown signal");
            sig_loop.stop();
        })
        .map_err(|err| anyhow::anyhow!("failed to insert signal source: {err}"))?;

    // Try libseat + udev first; fall back to direct /dev/dri/card0 when launched without a VT.
    let mut libseat_session = match LibSeatSession::new() {
        Ok((session, notifier)) => {
            info!(seat = %session.seat(), "Opened libseat session");
            let udev = match UdevBackend::new(session.seat()) {
                Ok(udev) => Some(udev),
                Err(err) => {
                    warn!(error = %err, "Failed to initialize UdevBackend; will probe default DRM card");
                    None
                }
            };
            Some((session, notifier, udev))
        }
        Err(err) => {
            warn!(
                error = %err,
                "LibSeatSession::new() failed (no VT/seatd?); falling back to direct {DEFAULT_DRM_CARD}"
            );
            None
        }
    };

    let (drm_fd, drm_path) = match libseat_session.as_mut() {
        Some((session, _, udev)) => open_drm_via_session(session, udev.as_ref())?,
        None => open_drm_direct(Path::new(DEFAULT_DRM_CARD))?,
    };
    info!(path = %drm_path.display(), "Opened DRM device");

    let (mut drm_device, drm_notifier) =
        DrmDevice::new(drm_fd, false).context("failed to initialize DrmDevice")?;

    let (crtc, connector, kms_mode) =
        select_kms_target(&drm_device).context("failed to select connected KMS output")?;
    let (mode_w, mode_h) = kms_mode.size();
    let refresh_hz = kms_mode.vrefresh().max(1);
    info!(
        width = mode_w,
        height = mode_h,
        refresh_hz,
        ?crtc,
        ?connector,
        "Selected KMS mode"
    );

    let surface = drm_device
        .create_surface(crtc, kms_mode, &[connector])
        .context("failed to create DrmSurface")?;

    let output_mode = OutputMode {
        size: (i32::from(mode_w), i32::from(mode_h)).into(),
        refresh: (refresh_hz as i32) * 1000,
    };
    state.output.change_current_state(
        Some(output_mode),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        Some((0, 0).into()),
    );
    state.output.set_preferred(output_mode);
    state.space.map_output(&state.output, (0, 0));

    let renderer = PixmanRenderer::new().context("failed to create PixmanRenderer")?;
    let renderer_formats =
        Bind::<smithay::backend::allocator::dmabuf::Dmabuf>::supported_formats(&renderer)
            .unwrap_or_default();

    let drm_fd_for_comp = drm_device.device_fd().clone();
    let allocator = DumbAllocator::new(drm_fd_for_comp.clone());
    let drm_compositor = DrmCompositor::new(
        &state.output,
        surface,
        None,
        allocator,
        drm_fd_for_comp,
        [Fourcc::Xrgb8888, Fourcc::Argb8888],
        renderer_formats,
        drm_device.cursor_size(),
        None::<smithay::backend::allocator::gbm::GbmDevice<DrmDeviceFd>>,
    )
    .context("failed to initialize DrmCompositor with DumbAllocator")?;

    loop_handle
        .insert_source(drm_notifier, |event, _, data| match event {
            DrmEvent::VBlank(vblank_crtc) => {
                if vblank_crtc == data.drm_compositor.crtc() {
                    if let Err(err) = data.drm_compositor.frame_submitted() {
                        warn!(?err, "Failed to mark DRM frame as submitted");
                    }
                    data.vblank_pending = false;
                }
            }
            DrmEvent::Error(err) => {
                error!(?err, "DRM device event error");
            }
        })
        .map_err(|err| anyhow::anyhow!("failed to insert DrmDeviceNotifier source: {err}"))?;

    if let Some((session, notifier, _udev)) = libseat_session.take() {
        let mut libinput_ctx =
            Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        let seat_name = session.seat();
        if let Err(err) = libinput_ctx.udev_assign_seat(&seat_name) {
            warn!(?err, seat = %seat_name, "Failed to assign udev seat to libinput");
        } else {
            let libinput_backend = LibinputInputBackend::new(libinput_ctx.clone());
            loop_handle
                .insert_source(libinput_backend, |event, _, data| {
                    handle_input_event(data, event);
                })
                .map_err(|err| anyhow::anyhow!("failed to insert libinput source: {err}"))?;
        }

        let mut libinput_for_session = libinput_ctx;
        loop_handle
            .insert_source(notifier, move |event, _, data| match event {
                SessionEvent::PauseSession => {
                    info!("Pausing session");
                    libinput_for_session.suspend();
                    data.drm_device.pause();
                }
                SessionEvent::ActivateSession => {
                    info!("Activating session");
                    if let Err(err) = data.drm_device.activate(false) {
                        warn!(?err, "Failed to reactivate DRM device");
                    }
                    if libinput_for_session.resume().is_err() {
                        warn!("Failed to resume libinput context");
                    }
                    data.drm_compositor.reset_buffers();
                    data.vblank_pending = false;
                    data.state.needs_redraw = true;
                }
            })
            .map_err(|err| anyhow::anyhow!("failed to insert libseat notifier source: {err}"))?;
    }

    loop_handle
        .insert_source(Timer::from_duration(FRAME_INTERVAL), |_, _, data| {
            data.state.needs_redraw = true;
            TimeoutAction::ToDuration(FRAME_INTERVAL)
        })
        .map_err(|err| anyhow::anyhow!("failed to insert frame timer source: {err}"))?;

    let shell_proc = match config.shell.as_deref() {
        Some(program) => Some(ShellProcess::spawn(program, &socket_name)?),
        None => None,
    };

    let output_size: Size<i32, Logical> = (i32::from(mode_w), i32::from(mode_h)).into();
    let mut loop_data = DrmLoopData {
        display,
        state,
        shell_proc,
        loop_signal,
        drm_device,
        drm_compositor,
        renderer,
        vblank_pending: false,
        pointer_location: (
            f64::from(output_size.w) / 2.0,
            f64::from(output_size.h) / 2.0,
        )
            .into(),
        output_size,
        start_time: Instant::now(),
    };

    // Render initial frame before entering the event loop.
    render_drm_frame(&mut loop_data);

    event_loop
        .run(Some(FRAME_INTERVAL), &mut loop_data, |data| {
            if data.state.needs_redraw && !data.vblank_pending {
                render_drm_frame(data);
            }
            if let Err(err) = data.display.dispatch_clients(&mut data.state) {
                error!(error = %err, "Error dispatching Wayland clients");
            }
            if let Err(err) = data.display.flush_clients() {
                error!(error = %err, "Error flushing Wayland clients");
            }
            if let Some(shell) = data.shell_proc.as_mut() {
                match shell.try_wait() {
                    Ok(Some(status)) => {
                        info!(%status, "Shell process exited; shutting down DRM compositor");
                        data.loop_signal.stop();
                    }
                    Ok(None) => {}
                    Err(err) => {
                        warn!(error = %err, "Error polling shell process");
                    }
                }
            }
        })
        .context("DRM compositor event loop failed")?;

    if let Some(mut shell) = loop_data.shell_proc.take() {
        shell.terminate();
    }

    info!("DRM compositor shut down cleanly");
    Ok(())
}

fn open_drm_via_session(
    session: &mut LibSeatSession,
    udev: Option<&UdevBackend>,
) -> anyhow::Result<(DrmDeviceFd, PathBuf)> {
    use smithay::reexports::rustix::fs::OFlags;
    let flags = OFlags::RDWR | OFlags::CLOEXEC;

    if let Some(udev_backend) = udev {
        for (_dev_id, path) in udev_backend.device_list() {
            match session.open(path, flags) {
                Ok(fd) => return Ok((DrmDeviceFd::new(DeviceFd::from(fd)), path.to_path_buf())),
                Err(err) => {
                    warn!(path = %path.display(), error = %err, "Failed to open udev DRM device via libseat");
                }
            }
        }
    }

    let fallback = Path::new(DEFAULT_DRM_CARD);
    let fd = session
        .open(fallback, flags)
        .with_context(|| format!("failed to open {} via libseat", fallback.display()))?;
    Ok((DrmDeviceFd::new(DeviceFd::from(fd)), fallback.to_path_buf()))
}

fn open_drm_direct(path: &Path) -> anyhow::Result<(DrmDeviceFd, PathBuf)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open DRM device directly at {}", path.display()))?;
    let owned_fd: OwnedFd = file.into();
    Ok((
        DrmDeviceFd::new(DeviceFd::from(owned_fd)),
        path.to_path_buf(),
    ))
}

fn select_kms_target(
    drm: &DrmDevice,
) -> anyhow::Result<(crtc::Handle, connector::Handle, DrmMode)> {
    let res = drm
        .resource_handles()
        .context("failed to query DRM resource handles")?;

    for &conn_handle in res.connectors() {
        let conn = match drm.get_connector(conn_handle, true) {
            Ok(c) => c,
            Err(err) => {
                warn!(?conn_handle, ?err, "Failed to probe DRM connector");
                continue;
            }
        };
        if conn.state() != connector::State::Connected || conn.modes().is_empty() {
            continue;
        }

        let mode = select_mode(conn.modes())
            .ok_or_else(|| anyhow::anyhow!("connected connector had no usable modes"))?;

        if let Some(enc_handle) = conn.current_encoder() {
            if let Ok(enc) = drm.get_encoder(enc_handle) {
                if let Some(crtc_handle) = enc.crtc() {
                    return Ok((crtc_handle, conn_handle, mode));
                }
            }
        }

        for &enc_handle in conn.encoders() {
            if let Ok(enc) = drm.get_encoder(enc_handle) {
                if let Some(crtc_handle) = res.filter_crtcs(enc.possible_crtcs()).into_iter().next()
                {
                    return Ok((crtc_handle, conn_handle, mode));
                }
            }
        }
    }

    anyhow::bail!("no connected DRM connector with a compatible CRTC found")
}

fn select_mode(modes: &[DrmMode]) -> Option<DrmMode> {
    if let Some(preferred) = modes
        .iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
    {
        return Some(*preferred);
    }
    modes
        .iter()
        .min_by_key(|m| {
            let (w, h) = m.size();
            let dw = (i32::from(w) - 1280).abs();
            let dh = (i32::from(h) - 800).abs();
            dw + dh
        })
        .copied()
}

fn render_drm_frame(data: &mut DrmLoopData) {
    let elements = match space_render_elements::<_, Window, _>(
        &mut data.renderer,
        [&data.state.space],
        &data.state.output,
        1.0,
    ) {
        Ok(els) => els,
        Err(err) => {
            warn!(?err, "Failed to collect space render elements");
            return;
        }
    };

    match data.drm_compositor.render_frame(
        &mut data.renderer,
        &elements,
        CLEAR_COLOR,
        FrameFlags::DEFAULT,
    ) {
        Ok(res) => {
            if !res.is_empty {
                if let Err(err) = data.drm_compositor.queue_frame(()) {
                    warn!(?err, "Failed to queue DRM frame");
                } else {
                    data.vblank_pending = true;
                }
            }
        }
        Err(err) => {
            warn!(?err, "DRM render_frame failed");
        }
    }

    let elapsed = data.start_time.elapsed();
    for window in data.state.space.elements() {
        window.send_frame(&data.state.output, elapsed, Some(Duration::ZERO), |_, _| {
            Some(data.state.output.clone())
        });
    }

    data.state.needs_redraw = false;
}

fn handle_input_event(data: &mut DrmLoopData, event: InputEvent<LibinputInputBackend>) {
    match event {
        InputEvent::Keyboard { event } => {
            if let Some(keyboard) = data.state.seat.get_keyboard() {
                let serial = SERIAL_COUNTER.next_serial();
                let time = InputEventTrait::time_msec(&event);
                keyboard.input::<(), _>(
                    &mut data.state,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
        }
        InputEvent::PointerMotion { event } => {
            let delta = event.delta();
            data.pointer_location.x = (data.pointer_location.x + delta.x)
                .clamp(0.0, f64::from(data.output_size.w.max(1) - 1));
            data.pointer_location.y = (data.pointer_location.y + delta.y)
                .clamp(0.0, f64::from(data.output_size.h.max(1) - 1));
            dispatch_pointer_motion(data, InputEventTrait::time_msec(&event));
        }
        InputEvent::PointerMotionAbsolute { event } => {
            data.pointer_location = event.position_transformed(data.output_size);
            dispatch_pointer_motion(data, InputEventTrait::time_msec(&event));
        }
        InputEvent::PointerButton { event } => {
            if let Some(pointer) = data.state.seat.get_pointer() {
                let serial = SERIAL_COUNTER.next_serial();
                let time = InputEventTrait::time_msec(&event);
                if event.state() == ButtonState::Pressed {
                    focus_top_window_under_pointer(data, serial);
                }
                pointer.button(
                    &mut data.state,
                    &ButtonEvent {
                        button: event.button_code(),
                        state: event.state(),
                        serial,
                        time,
                    },
                );
                pointer.frame(&mut data.state);
            }
        }
        InputEvent::PointerAxis { event } => {
            if let Some(pointer) = data.state.seat.get_pointer() {
                let time = InputEventTrait::time_msec(&event);
                let mut frame = AxisFrame::new(time).source(event.source());
                if let Some(amount) = event.amount(Axis::Horizontal) {
                    frame = frame.value(Axis::Horizontal, amount);
                }
                if let Some(amount) = event.amount(Axis::Vertical) {
                    frame = frame.value(Axis::Vertical, amount);
                }
                if event.source() == AxisSource::Wheel {
                    if let Some(v120) = event.amount_v120(Axis::Horizontal) {
                        frame = frame.v120(Axis::Horizontal, v120 as i32);
                    }
                    if let Some(v120) = event.amount_v120(Axis::Vertical) {
                        frame = frame.v120(Axis::Vertical, v120 as i32);
                    }
                }
                pointer.axis(&mut data.state, frame);
                pointer.frame(&mut data.state);
            }
        }
        _ => {
            debug!("Ignoring unhandled libinput event");
        }
    }
}

fn dispatch_pointer_motion(data: &mut DrmLoopData, time: u32) {
    if let Some(pointer) = data.state.seat.get_pointer() {
        let serial = SERIAL_COUNTER.next_serial();
        let under = data
            .state
            .space
            .element_under(data.pointer_location)
            .and_then(|(window, loc)| {
                let surface = window.toplevel()?.wl_surface().clone();
                Some((surface, loc.to_f64()))
            });
        pointer.motion(
            &mut data.state,
            under,
            &MotionEvent {
                location: data.pointer_location,
                serial,
                time,
            },
        );
        pointer.frame(&mut data.state);
    }
}

fn focus_top_window_under_pointer(data: &mut DrmLoopData, serial: smithay::utils::Serial) {
    if let Some((window, _)) = data.state.space.element_under(data.pointer_location) {
        if let Some(toplevel) = window.toplevel() {
            let surface = toplevel.wl_surface().clone();
            if let Some(keyboard) = data.state.seat.get_keyboard() {
                keyboard.set_focus(&mut data.state, Some(surface), serial);
            }
        }
    }
}
