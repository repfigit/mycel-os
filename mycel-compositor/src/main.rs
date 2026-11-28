//! CLI entrypoint for the Mycel OS standalone Wayland compositor.

use clap::Parser;
use mycel_compositor::compositor::{run_compositor, CompositorConfig};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "mycel-compositor",
    version,
    about = "Mycel OS standalone thin Wayland compositor"
)]
struct Cli {
    /// Run in headless mode without a physical GPU or windowing backend.
    #[arg(long)]
    headless: bool,

    /// Path to the shell executable to launch with WAYLAND_DISPLAY set (or MYCEL_SHELL env).
    #[arg(long)]
    shell: Option<String>,

    /// Wayland socket name to bind (auto-selected if omitted).
    #[arg(long)]
    socket_name: Option<String>,

    /// Output width in pixels.
    #[arg(long, default_value_t = 1280)]
    width: i32,

    /// Output height in pixels.
    #[arg(long, default_value_t = 800)]
    height: i32,

    /// Enable verbose debug logging.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let default_level = if cli.verbose { "debug" } else { "info" };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let shell = cli
        .shell
        .or_else(|| std::env::var("MYCEL_SHELL").ok().filter(|s| !s.is_empty()));

    let config = CompositorConfig {
        socket_name: cli.socket_name,
        shell,
        width: cli.width,
        height: cli.height,
        headless: cli.headless,
    };

    run_compositor(config)
}
