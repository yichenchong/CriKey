//! `crikey` command-line entrypoint (spec 28).

use std::process::ExitCode;

use crikey_app::App;

const USAGE: &str = "\
crikey - a fast, keyboard-driven application launcher

USAGE:
    crikey <COMMAND> [ARGS]

COMMANDS:
    run                             Start the launcher
    plugin list|install|remove|enable|disable|doctor|scheduling-profile
    dev   run|test|benchmark|trace-query|simulate-typing|inspect-protocol|test-legacy-compat
    package build|verify|inspect|migrate-keypirinha
    version                         Print version information
    help                            Print this message
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("help") | Some("-h") | Some("--help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("version") | Some("-V") | Some("--version") => {
            println!(
                "crikey {} ({} backend)",
                env!("CARGO_PKG_VERSION"),
                App::platform_backend_name()
            );
            ExitCode::SUCCESS
        }
        Some(command @ ("run" | "plugin" | "dev" | "package")) => {
            eprintln!("crikey: `{command}` is not implemented yet");
            ExitCode::from(69) // EX_UNAVAILABLE
        }
        Some(other) => {
            eprintln!("crikey: unknown command `{other}`\n\n{USAGE}");
            ExitCode::from(64) // EX_USAGE
        }
    }
}
