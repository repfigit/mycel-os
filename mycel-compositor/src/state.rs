//! Wayland protocol state and Smithay handler implementations.

use std::os::unix::io::OwnedFd;

use anyhow::Context;
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_data_device, delegate_output, delegate_seat, delegate_shm,
    delegate_xdg_shell,
    desktop::{Space, Window},
    input::{keyboard::XkbConfig, pointer::CursorImageStatus, Seat, SeatHandler, SeatState},
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_seat, wl_surface::WlSurface},
            Client, Display, DisplayHandle,
        },
    },
    utils::{Serial, Transform},
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        output::{OutputHandler, OutputManagerState},
        selection::{
            data_device::{
                ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
            },
            SelectionHandler,
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        },
        shm::{ShmHandler, ShmState},
    },
};
use tracing::{debug, info};

/// Per-client state stored on each connected Wayland client.
#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, client_id: ClientId) {
        debug!(?client_id, "Wayland client initialized");
    }

    fn disconnected(&self, client_id: ClientId, reason: DisconnectReason) {
        debug!(?client_id, ?reason, "Wayland client disconnected");
    }
}

/// Core compositor state holding all Smithay protocol managers and the desktop space.
pub struct MycelCompositorState {
    pub display_handle: DisplayHandle,
    pub space: Space<Window>,
    pub output: Output,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Self>,
    pub data_device_state: DataDeviceState,
    pub seat: Seat<Self>,
}

impl MycelCompositorState {
    /// Initialize all core Wayland globals, seat capabilities, and the primary output.
    pub fn new(display: &Display<Self>, width: i32, height: i32) -> anyhow::Result<Self> {
        let dh = display.handle();

        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);

        let mut seat_state = SeatState::new();
        let mut seat = seat_state.new_wl_seat(&dh, "mycel-seat");
        seat.add_keyboard(XkbConfig::default(), 200, 25)
            .context("failed to add keyboard capability to wl_seat")?;
        seat.add_pointer();

        let output = Output::new(
            "mycel-0".into(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "Mycel".into(),
                model: "Virtual".into(),
            },
        );
        let _output_global = output.create_global::<Self>(&dh);
        let mode = Mode {
            size: (width, height).into(),
            refresh: 60_000,
        };
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        output.set_preferred(mode);

        let mut space = Space::default();
        space.map_output(&output, (0, 0));

        info!(width, height, "Initialized MycelCompositorState");

        Ok(Self {
            display_handle: dh,
            space,
            output,
            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            seat_state,
            data_device_state,
            seat,
        })
    }
}

impl BufferHandler for MycelCompositorState {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl CompositorHandler for MycelCompositorState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<ClientState>()
            .expect("ClientState missing from Wayland client")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
    }
}

impl ShmHandler for MycelCompositorState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl XdgShellHandler for MycelCompositorState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        let window = Window::new_wayland_window(surface.clone());
        self.space.map_element(window, (0, 0), true);
        surface.send_configure();
        debug!("Mapped new XDG toplevel and sent initial configure");
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let target = self
            .space
            .elements()
            .find(|w| w.toplevel().is_some_and(|t| t == &surface))
            .cloned();
        if let Some(window) = target {
            self.space.unmap_elem(&window);
            debug!("Unmapped destroyed XDG toplevel");
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        let _ = surface.send_configure();
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        _positioner: PositionerState,
        token: u32,
    ) {
        surface.send_repositioned(token);
    }
}

impl SeatHandler for MycelCompositorState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }

    fn focus_changed(&mut self, _seat: &Seat<Self>, _focused: Option<&WlSurface>) {}

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

impl OutputHandler for MycelCompositorState {}

impl SelectionHandler for MycelCompositorState {
    type SelectionUserData = ();
}

impl DataDeviceHandler for MycelCompositorState {
    fn data_device_state(&self) -> &DataDeviceState {
        &self.data_device_state
    }
}

impl ClientDndGrabHandler for MycelCompositorState {}

impl ServerDndGrabHandler for MycelCompositorState {
    fn send(&mut self, _mime_type: String, _fd: OwnedFd, _seat: Seat<Self>) {}
}

delegate_compositor!(MycelCompositorState);
delegate_shm!(MycelCompositorState);
delegate_xdg_shell!(MycelCompositorState);
delegate_seat!(MycelCompositorState);
delegate_output!(MycelCompositorState);
delegate_data_device!(MycelCompositorState);
