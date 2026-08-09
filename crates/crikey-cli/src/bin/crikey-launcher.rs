//! `crikey-launcher`: the graphical entry point (spec 28).
//!
//! WHY A SECOND BINARY
//! ===================
//! Two launch paths cannot pass arguments and have no manifest key for
//! supplying any: a macOS bundle opened through Launch Services, and an MSIX
//! tile. Both start the declared executable bare. `crikey` answers a bare
//! invocation with usage text and exit 0, so a double click or a tile click
//! produced nothing a user could see. Separately, `crikey` is a
//! console-subsystem program, so launching it from a Windows shortcut also put
//! a console window beside the launcher.
//!
//! Those two pull in opposite directions inside one executable, which is the
//! actual design problem. A GUI-subsystem process has no console attached, so
//! declaring `windows_subsystem = "windows"` on the single binary would fix
//! the console window and throw away the output of every subcommand on
//! Windows.
//!
//! WHY NOT THE ALTERNATIVES
//! ========================
//! Making bare `crikey` start the launcher would silently change a documented
//! CLI contract — and it would still leave the console window unfixable,
//! because the one binary must stay console-subsystem to print anything.
//! Sniffing whether stdout is a terminal makes what the program *does* depend
//! on how it was invoked: unpredictable for the user, and untestable in any
//! way that resembles the real launch paths.
//!
//! So: two binaries from one crate. `crikey` is unchanged, console-subsystem,
//! and remains what a terminal and `PATH` reach. `crikey-launcher` is
//! GUI-subsystem on Windows, takes no arguments, and starts the launcher
//! directly. It is a thin entry point over `crikey_cli::start_launcher`, not a
//! second copy of the run path: both binaries reach the same composition root.

// GUI subsystem on Windows only. On macOS and Linux the attribute does not
// exist and is not needed -- neither platform attaches a terminal to a process
// started by the desktop.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::process::ExitCode;

const USAGE: &str = "\
crikey-launcher - start the CriKey launcher

USAGE:
    crikey-launcher

This is the graphical entry point, for a desktop shortcut, an application
bundle or a tile. It takes no arguments. For the command line -- including
`crikey run`, which starts the same launcher and does accept options -- use
`crikey`.
";

fn main() -> ExitCode {
    // Refused, not ignored. Everything that starts this binary starts it bare,
    // so an argument means a person typed it, most likely reaching for a
    // `crikey` subcommand. Discarding it would start the launcher and look
    // exactly like the subcommand had run and done nothing.
    //
    // On Windows this particular message goes nowhere, because a GUI-subsystem
    // process has no console: the exit code is the observable part there. That
    // is the price of not showing a console window on every normal launch, and
    // it is paid only here, on a path no packaged launch takes. A failure of
    // the launch itself is not left to the exit code: `start_launcher` records
    // it in the per-user `startup.log` and shows it, because that is the path
    // every packaged launch does take.
    if let Some(unexpected) = std::env::args().nth(1) {
        eprintln!("crikey-launcher: unexpected argument `{unexpected}`\n\n{USAGE}");
        return ExitCode::from(64); // EX_USAGE
    }

    crikey_cli::start_launcher()
}
