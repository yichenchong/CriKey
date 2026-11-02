#[cfg(not(target_os = "linux"))]
compile_error!("crikey-web-host's engine feature requires Linux");

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::path::PathBuf;
#[cfg(target_os = "linux")]
use std::process::ExitCode;

#[cfg(target_os = "linux")]
use crikey_native_protocol::Endpoint;
#[cfg(target_os = "linux")]
use crikey_web_host::engine;

#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    let options = match Options::parse(std::env::args_os()) {
        Ok(ParseResult::Run(options)) => options,
        Ok(ParseResult::Help) => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        Err(error) => {
            eprintln!("crikey-web-host: {error}");
            eprintln!("try `crikey-web-host --help`");
            return ExitCode::from(2);
        }
    };

    match engine::run(options.endpoint, options.storage_dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("crikey-web-host: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct Options {
    endpoint: Endpoint,
    storage_dir: Option<PathBuf>,
}

#[cfg(target_os = "linux")]
enum ParseResult {
    Run(Options),
    Help,
}

#[cfg(target_os = "linux")]
impl Options {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<ParseResult, String> {
        let mut arguments = arguments.into_iter();
        let _program = arguments.next();
        let mut endpoint = None;
        let mut storage_dir = None;

        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--endpoint") => {
                    if endpoint.is_some() {
                        return Err("--endpoint may be specified only once".to_owned());
                    }
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--endpoint requires a value".to_owned())?;
                    let value = value
                        .to_str()
                        .ok_or_else(|| "--endpoint must be valid UTF-8".to_owned())?;
                    endpoint =
                        Some(Endpoint::parse(value).map_err(|error| format!("invalid --endpoint: {error}"))?);
                }
                Some("--storage-dir") => {
                    if storage_dir.is_some() {
                        return Err("--storage-dir may be specified only once".to_owned());
                    }
                    let value = arguments
                        .next()
                        .ok_or_else(|| "--storage-dir requires a value".to_owned())?;
                    if value.is_empty() {
                        return Err("--storage-dir may not be empty".to_owned());
                    }
                    storage_dir = Some(PathBuf::from(value));
                }
                Some("-h") | Some("--help") => {
                    if endpoint.is_some() || storage_dir.is_some() || arguments.next().is_some() {
                        return Err("--help cannot be combined with other arguments".to_owned());
                    }
                    return Ok(ParseResult::Help);
                }
                Some(value) => return Err(format!("unknown argument {value:?}")),
                None => return Err(format!("argument {argument:?} is not valid UTF-8")),
            }
        }

        let endpoint = endpoint.ok_or_else(|| "--endpoint is required".to_owned())?;
        Ok(ParseResult::Run(Self {
            endpoint,
            storage_dir,
        }))
    }
}

#[cfg(target_os = "linux")]
fn print_usage() {
    println!(
        "Usage: crikey-web-host --endpoint unix:/absolute/path [--storage-dir /absolute/path]\n\
         \n\
         Private WPE web-surface worker. It is normally started by the CriKey launcher."
    );
}
