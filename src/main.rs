#![windows_subsystem = "windows"]

mod character;
mod codex_usage;
mod diagnose;
mod localization;
mod models;
mod native_interop;
mod overlay_visibility;
mod poller;
mod quota_layout;
mod theme;
mod tray_icon;
mod tray_status;
mod updater;
mod widget_pixels;
mod window;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let diagnose_enabled =
        args.iter().any(|arg| arg == "--diagnose") || env!("CARGO_PKG_VERSION").contains("-local.");
    if diagnose_enabled {
        match diagnose::init() {
            Ok(path) => diagnose::log(format!(
                "startup version={} pid={} log_path={}",
                env!("CARGO_PKG_VERSION"),
                std::process::id(),
                path.display()
            )),
            Err(error) => {
                // Logging may not be available yet, but keep startup behavior unchanged.
                let _ = error;
            }
        }
    }

    if let Some(exit_code) = updater::handle_cli_mode(&args) {
        if diagnose_enabled {
            diagnose::log(format!("cli mode exited with code {exit_code}"));
        }
        std::process::exit(exit_code);
    }

    if diagnose_enabled {
        diagnose::log("entering window::run");
    }
    window::run();
}
