//! Real Wayland socket roundtrip, shell process, and clean shutdown integration tests.

use std::{
    collections::HashSet,
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{mpsc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use mycel_compositor::{
    compositor::{run_headless, CompositorConfig},
    shell_proc::ShellProcess,
};
use wayland_client::{
    protocol::{wl_compositor, wl_registry, wl_surface},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn unique_temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[derive(Default)]
struct ClientTestState {
    globals: HashSet<String>,
    compositor: Option<wl_compositor::WlCompositor>,
    wm_base: Option<xdg_wm_base::XdgWmBase>,
    configure_serials: Vec<u32>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for ClientTestState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            state.globals.insert(interface.clone());
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind::<wl_compositor::WlCompositor, _, _>(
                        name,
                        version.min(6),
                        qh,
                        (),
                    ));
                }
                "xdg_wm_base" => {
                    state.wm_base = Some(registry.bind::<xdg_wm_base::XdgWmBase, _, _>(
                        name,
                        version.min(6),
                        qh,
                        (),
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for ClientTestState {
    fn event(
        _: &mut Self,
        _: &wl_compositor::WlCompositor,
        _: wl_compositor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_surface::WlSurface, ()> for ClientTestState {
    fn event(
        _: &mut Self,
        _: &wl_surface::WlSurface,
        _: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for ClientTestState {
    fn event(
        _: &mut Self,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for ClientTestState {
    fn event(
        state: &mut Self,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.configure_serials.push(serial);
            xdg_surface.ack_configure(serial);
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for ClientTestState {
    fn event(
        _: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        _: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

#[test]
fn wayland_globals_and_xdg_toplevel_configure_roundtrip() {
    let _guard = ENV_LOCK.lock().unwrap();
    let runtime_dir = unique_temp_dir("mycel-comp-test");
    let prev_runtime_dir = std::env::var_os("XDG_RUNTIME_DIR");
    std::env::set_var("XDG_RUNTIME_DIR", &runtime_dir);

    let socket_name = format!("mycel-test-{}", std::process::id());
    let (tx, rx) = mpsc::channel();

    let server_socket = socket_name.clone();
    let server_handle = std::thread::spawn(move || {
        let config = CompositorConfig {
            socket_name: Some(server_socket),
            shell: None,
            width: 1280,
            height: 800,
            headless: true,
        };
        run_headless(config, false, move |bound_name, loop_signal| {
            tx.send((bound_name, loop_signal)).unwrap();
        })
    });

    let (bound_name, loop_signal) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("compositor did not bind socket in time");
    assert_eq!(bound_name, socket_name);

    let socket_path = runtime_dir.join(&bound_name);
    let stream = UnixStream::connect(&socket_path).expect("failed to connect to compositor socket");
    let conn = Connection::from_socket(stream).expect("failed to create wayland Connection");
    let display = conn.display();

    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut client_state = ClientTestState::default();

    let _registry = display.get_registry(&qh, ());
    event_queue
        .roundtrip(&mut client_state)
        .expect("registry roundtrip failed");

    for expected_global in [
        "wl_compositor",
        "wl_shm",
        "xdg_wm_base",
        "wl_seat",
        "wl_output",
        "wl_data_device_manager",
    ] {
        assert!(
            client_state.globals.contains(expected_global),
            "missing expected global `{expected_global}`; got {:?}",
            client_state.globals
        );
    }

    let compositor = client_state
        .compositor
        .as_ref()
        .expect("wl_compositor not bound");
    let wm_base = client_state
        .wm_base
        .as_ref()
        .expect("xdg_wm_base not bound");

    let wl_surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&wl_surface, &qh, ());
    let _xdg_toplevel = xdg_surface.get_toplevel(&qh, ());
    wl_surface.commit();

    event_queue
        .roundtrip(&mut client_state)
        .expect("xdg_surface configure roundtrip failed");

    assert!(
        !client_state.configure_serials.is_empty(),
        "expected at least one xdg_surface.configure event"
    );

    // Flush the ack_configure sent inside Dispatch<XdgSurface>.
    event_queue
        .roundtrip(&mut client_state)
        .expect("ack_configure roundtrip failed");

    drop(event_queue);
    drop(conn);

    // Verify clean shutdown signaling stops the event loop cleanly.
    loop_signal.stop();
    let server_res = server_handle
        .join()
        .expect("compositor server thread panicked");
    assert!(
        server_res.is_ok(),
        "compositor exited with error: {server_res:?}"
    );

    match prev_runtime_dir {
        Some(val) => std::env::set_var("XDG_RUNTIME_DIR", val),
        None => std::env::remove_var("XDG_RUNTIME_DIR"),
    }
    let _ = std::fs::remove_dir_all(&runtime_dir);
}

#[test]
fn shell_process_propagates_wayland_display_and_reaps_cleanly() {
    let mut proc =
        ShellProcess::spawn("/bin/sh", "mycel-shell-test-sock").expect("failed to spawn /bin/sh");
    proc.terminate();

    // Use `env` or `sh -c` via a wrapper? ShellProcess::spawn takes a program path,
    // so let's verify WAYLAND_DISPLAY propagation using `printenv` or `/bin/true`.
    let mut true_proc = ShellProcess::spawn("/bin/true", "mycel-shell-test-sock")
        .expect("failed to spawn /bin/true");

    let mut exited = None;
    for _ in 0..50 {
        if let Some(status) = true_proc.try_wait().expect("try_wait failed") {
            exited = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let status = exited.expect("/bin/true did not exit in time");
    assert!(
        status.success(),
        "expected /bin/true to succeed, got {status}"
    );
    assert!(true_proc.try_wait().unwrap().is_none());

    // Verify WAYLAND_DISPLAY is actually set in the child environment by writing a tiny helper script.
    let temp_dir = unique_temp_dir("mycel-shell-env");
    let script_path = temp_dir.join("check_env.sh");
    std::fs::write(
        &script_path,
        "#!/bin/sh\ntest \"$WAYLAND_DISPLAY\" = \"mycel-env-sock-99\"\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let mut env_proc = ShellProcess::spawn(script_path.to_str().unwrap(), "mycel-env-sock-99")
        .expect("failed to spawn check_env.sh");
    let mut env_exit = None;
    for _ in 0..50 {
        if let Some(status) = env_proc.try_wait().expect("try_wait failed") {
            env_exit = Some(status);
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let env_status = env_exit.expect("check_env.sh did not exit in time");
    assert!(
        env_status.success(),
        "WAYLAND_DISPLAY was not propagated as expected: {env_status}"
    );

    let _ = std::fs::remove_dir_all(&temp_dir);
}
