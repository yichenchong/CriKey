//! `crikey-cabi-host <installed-package-directory>`.
//!
//! CriKey starts one of these per `c-abi` package and supervises it exactly as
//! it supervises any other native plugin executable. The library named by that
//! package's manifest is loaded here, never in the launcher (ADR-0015).

use std::process::ExitCode;

fn main() -> ExitCode {
    match crikey_cabi_host::run(std::env::args_os().skip(1)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(failure) => {
            // Standard error is the supervised, bounded diagnostic channel;
            // standard input and output belong to the protocol transport.
            eprintln!("crikey-cabi-host: {failure}");
            ExitCode::from(failure.exit_code())
        }
    }
}
