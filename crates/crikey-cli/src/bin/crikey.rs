//! `crikey`: the command line (spec 28).
//!
//! A console-subsystem program on every platform, and that is the whole reason
//! it is not also the graphical entry point. Every subcommand's answer — usage
//! text, a plugin listing, a benchmark report, an error — is written to stdout
//! or stderr, and a Windows GUI-subsystem process has no console attached to
//! write to, so making this binary a GUI application would silently discard
//! the output of every command it has. `crikey-launcher` is the GUI half; the
//! two share one implementation in the crate library.

use std::process::ExitCode;

fn main() -> ExitCode {
    crikey_cli::cli_main()
}
