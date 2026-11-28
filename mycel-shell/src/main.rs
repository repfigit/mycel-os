use clap::Parser;
use iced::Theme;
use tracing_subscriber::EnvFilter;

use mycel_shell::app::MycelShellApp;
use mycel_shell::ipc_client::default_socket_path;

#[derive(Parser, Debug)]
#[command(name = "mycel-shell", about = "Mycel OS GUI Shell")]
struct Cli {
    /// Path to the mycel-runtime Unix domain socket.
    #[arg(long)]
    socket: Option<String>,

    /// Enable verbose debug logging.
    #[arg(short, long)]
    verbose: bool,
}

fn main() -> iced::Result {
    let cli = Cli::parse();

    let default_filter = if cli.verbose {
        "mycel_shell=debug,info"
    } else {
        "mycel_shell=info,warn"
    };
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(env_filter).init();

    let socket_path = cli.socket.unwrap_or_else(default_socket_path);

    iced::application(
        move || MycelShellApp::boot(socket_path.clone()),
        MycelShellApp::update,
        MycelShellApp::view,
    )
    .title("Mycel OS")
    .theme(|_: &MycelShellApp| Theme::Dark)
    .window_size((960.0, 720.0))
    .subscription(MycelShellApp::subscription)
    .run()
}
