use std::process::ExitCode;

#[cfg(target_os = "linux")]
use std::ffi::OsString;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use crikey_native_protocol::Endpoint;
#[cfg(target_os = "linux")]
use crikey_web_host::engine;
#[cfg(target_os = "linux")]
use crikey_web_host::state::validate_storage_dir;

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    eprintln!("crikey-web-host: web surfaces are only supported on Linux");
    ExitCode::FAILURE
}

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
                    let parsed =
                        Endpoint::parse(value).map_err(|error| format!("invalid --endpoint: {error}"))?;
                    if !matches!(&parsed, Endpoint::UnixSocket(path) if path.is_absolute()) {
                        return Err("--endpoint must be an absolute Unix socket path".to_owned());
                    }
                    endpoint = Some(parsed);
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
                    let path = PathBuf::from(value);
                    validate_storage_dir(&path).map_err(|error| error.to_string())?;
                    storage_dir = Some(path);
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use crikey_native_protocol::Endpoint;

    use super::{Options, ParseResult};

    fn parse(arguments: &[&str]) -> Result<ParseResult, String> {
        Options::parse(arguments.iter().map(OsString::from))
    }

    #[test]
    fn only_a_private_absolute_unix_endpoint_is_accepted() {
        for endpoint in ["stdio", "pipe:host", "unix:relative"] {
            assert!(parse(&["host", "--endpoint", endpoint]).is_err());
        }
        let ParseResult::Run(options) = parse(&["host", "--endpoint", "unix:/tmp/host.sock"]).unwrap() else {
            panic!("expected run options");
        };
        assert_eq!(
            options.endpoint,
            Endpoint::UnixSocket(PathBuf::from("/tmp/host.sock"))
        );
    }

    #[test]
    fn malformed_and_ambiguous_options_are_usage_errors() {
        for arguments in [
            vec!["host"],
            vec!["host", "--endpoint"],
            vec!["host", "--endpoint", "unix:/tmp/a", "--endpoint", "unix:/tmp/b"],
            vec!["host", "--endpoint", "unix:/tmp/a", "--storage-dir"],
            vec!["host", "--endpoint", "unix:/tmp/a", "--storage-dir", "relative"],
            vec!["host", "--endpoint", "unix:/tmp/a", "--storage-dir", ""],
            vec!["host", "--help", "--endpoint", "unix:/tmp/a"],
            vec!["host", "--endpoint", "unix:/tmp/a", "--help"],
        ] {
            assert!(parse(&arguments).is_err(), "{arguments:?}");
        }
        assert!(matches!(parse(&["host", "--help"]), Ok(ParseResult::Help)));
    }

    #[test]
    fn storage_directory_preserves_non_utf8_paths_but_rejects_nul() {
        let mut path = b"/tmp/storage-".to_vec();
        path.push(0xff);
        let path = OsString::from_vec(path);
        let ParseResult::Run(options) = Options::parse([
            OsString::from("host"),
            OsString::from("--endpoint"),
            OsString::from("unix:/tmp/a"),
            OsString::from("--storage-dir"),
            path.clone(),
        ])
        .unwrap() else {
            panic!("expected run options");
        };
        assert_eq!(options.storage_dir, Some(PathBuf::from(path)));
        assert!(Options::parse([
            OsString::from("host"),
            OsString::from("--endpoint"),
            OsString::from("unix:/tmp/a"),
            OsString::from("--storage-dir"),
            OsString::from_vec(b"/tmp/nul\0path".to_vec()),
        ])
        .is_err());
    }
}
