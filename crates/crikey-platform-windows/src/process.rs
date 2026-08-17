//! Launching an application and opening a URI, both through the shell's
//! execute verb (spec 18.2).
//!
//! One dispatch serves both kinds of launch target this backend discovers. A
//! Start Menu shortcut resolves to an executable path; a packaged application
//! has no path at all and is named `shell:AppsFolder\<AppUserModelID>`. Those
//! are not two mechanisms with a branch between them -- they are two strings
//! the shell parses, and `ShellExecuteExW` is the documented entry point for
//! both. Special-casing the packaged case would mean a second code path that
//! only Windows-with-packaged-apps ever exercises, which is the path most
//! likely to be wrong and least likely to be noticed.
//!
//! What is *not* target gated is the part with rules: turning an argument
//! vector back into the single string `lpParameters` takes. Joining arguments
//! with spaces is the classic way to break every path with a space in it, so
//! this module implements the documented inverse of `CommandLineToArgvW` and
//! the test suite round-trips it against [`split_arguments`], this crate's
//! forward parser, on every host.
//!
//! [`split_arguments`]: crate::split_arguments

use std::ffi::OsStr;
use std::path::Path;

use crikey_core::{CoreError, PlatformPath, Result};
use crikey_platform::{FileOpener, ProcessLauncher};

#[cfg(target_os = "windows")]
mod win32;

/// Process launch and URI opening over `ShellExecuteExW` (spec 18.2).
///
/// The shell rather than `CreateProcess`, for two reasons a launcher cannot do
/// without: a packaged application has no executable to create a process from,
/// and a target the user picked may be a document or a registered handler
/// rather than a program. Both are the shell's job to resolve, and asking it is
/// also what makes the elevation prompt, the "how do you want to open this"
/// dialog and the App Paths lookup behave the way the rest of Windows does.
///
/// Launching does not wait for the child. The call returns once the shell has
/// dispatched it, which is the point at which success or failure is known; what
/// the launched program does afterwards is not this backend's business and
/// blocking on it would pin a launcher thread for the lifetime of the program
/// the user just opened.
///
/// Off target every call is refused. Nothing is faked: a launch that cannot
/// reach the shell did not happen, and the caller is told.
#[derive(Debug, Clone, Copy)]
pub struct ShellLauncher;

impl ShellLauncher {
    /// Builds the launcher. It owns nothing: the shell holds all the state.
    pub const fn new() -> Self {
        Self
    }
}

impl Default for ShellLauncher {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessLauncher for ShellLauncher {
    /// Runs `target` with `args`, packaged moniker or executable path alike.
    ///
    /// The arguments are quoted, not joined: `args` is the vector the caller
    /// means the program to receive, and [`quote_arguments`] is what makes the
    /// shell's own re-split give it back unchanged.
    fn launch(&self, target: &PlatformPath, args: &[String]) -> Result<()> {
        self.launch_with_directory(target, args, None)
    }

    /// Runs `target` with an optional shortcut working directory.
    ///
    /// `ShellExecuteExW` accepts the directory separately from the target, so
    /// relative paths and configuration files resolve the same way as they do
    /// when the user opens the original shortcut.
    fn launch_in(
        &self,
        target: &PlatformPath,
        args: &[String],
        working_directory: Option<&PlatformPath>,
    ) -> Result<()> {
        self.launch_with_directory(target, args, working_directory)
    }

    /// Hands `uri` to the shell's registered handler for its scheme.
    ///
    /// The same dispatch as [`launch`](Self::launch), because to the shell this
    /// is the same operation: `ShellExecuteEx` resolves a scheme through the
    /// registry exactly as it resolves a path through the filesystem.
    fn open_uri(&self, uri: &str) -> Result<()> {
        if !has_scheme(uri) {
            return Err(CoreError::Invalid(format!(
                "the Windows backend will not open {uri:?} as a URI: it names no scheme, \
                 and the shell would run it as a local file instead -- launch a path through \
                 `ProcessLauncher::launch`"
            )));
        }
        let uri = OsStr::new(uri);
        carriable("URI", uri)?;

        dispatch("open", uri, None, None)
    }
}

impl ShellLauncher {
    fn launch_with_directory(
        &self,
        target: &PlatformPath,
        args: &[String],
        working_directory: Option<&PlatformPath>,
    ) -> Result<()> {
        let target = target.as_os_str();
        if target.is_empty() {
            return Err(CoreError::Invalid(
                "the Windows backend cannot launch an empty target".to_owned(),
            ));
        }
        carriable("launch target", target)?;
        for argument in args {
            carriable("launch argument", OsStr::new(argument.as_str()))?;
        }

        let directory = working_directory.map(PlatformPath::as_os_str);
        if let Some(directory) = directory {
            if directory.is_empty() {
                return Err(CoreError::Invalid(
                    "the Windows backend cannot use an empty working directory".to_owned(),
                ));
            }
            carriable("working directory", directory)?;
        }

        let parameters = quote_arguments(args);
        dispatch(
            "launch",
            target,
            (!parameters.is_empty()).then(|| OsStr::new(parameters.as_str())),
            directory,
        )
    }
}

/// Opening a file or folder with its registered handler (spec 18.2).
///
/// The same `ShellExecuteExW` dispatch as a launch, with the shell's default
/// verb, because to the shell this *is* the same operation: it resolves a
/// document's association through the registry exactly as it resolves an
/// executable through the filesystem.
///
/// The path travels as `lpFile`, a single wide-string argument, and never
/// through a command line. That matters here more than it does for a launch:
/// the path came from a file search rather than from a desktop entry this
/// workspace parsed, so it is whatever the user happens to have on disk. A
/// file named `a&b.txt` or `x^y.txt` is an ordinary file, and building a
/// command string out of it would let `cmd.exe`'s metacharacters mean
/// something. Nothing here builds one.
impl FileOpener for ShellLauncher {
    fn open_path(&self, path: &PlatformPath) -> Result<()> {
        let file = openable("path", path)?;

        dispatch("open", file, None, None)
    }

    /// Opens the *containing folder* in Explorer.
    ///
    /// Not a true reveal: selecting the item needs `SHOpenFolderAndSelectItems`
    /// and the shell item list it takes, which is a COM surface this backend
    /// does not carry. The alternative within `ShellExecuteExW` -- passing the
    /// file itself -- would launch its registered application, which is
    /// [`Self::open_path`] under another name and the one outcome a user asking
    /// to reveal it does not want. `explorer.exe /select,<path>` is not the
    /// answer either: it takes a *command line*, so a path containing a comma
    /// arrives as a different path, and this seam refuses to corrupt one.
    fn reveal_path(&self, path: &PlatformPath) -> Result<()> {
        let file = openable("path", path)?;
        let directory = containing_directory(Path::new(file));

        dispatch("reveal", directory.as_os_str(), None, None)
    }
}

/// The path as a string the shell can carry whole, or the refusal saying why
/// it cannot.
fn openable<'a>(role: &str, path: &'a PlatformPath) -> Result<&'a OsStr> {
    let path = path.as_os_str();
    if path.is_empty() {
        return Err(CoreError::Invalid(format!(
            "the Windows backend cannot open an empty {role}"
        )));
    }
    carriable(role, path)?;
    Ok(path)
}

/// The directory a path lives in.
fn containing_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        // A relative bare name lives in the process's own directory; `""` is
        // not a directory and the shell would refuse it.
        Some(_) => Path::new("."),
        // A root has no parent and contains itself.
        None => path,
    }
}

/// The shell call, or the refusal that stands in for it off target.
///
/// `verb` completes both messages a failure can produce -- "Windows would not
/// {verb} {target}" and "the Windows backend cannot {verb} {target}" -- so it
/// is a bare verb: `"launch"`, `"open"`.
#[cfg(target_os = "windows")]
fn dispatch(verb: &str, file: &OsStr, parameters: Option<&OsStr>, directory: Option<&OsStr>) -> Result<()> {
    win32::execute(verb, file, parameters, directory)
}

#[cfg(not(target_os = "windows"))]
fn dispatch(verb: &str, file: &OsStr, _parameters: Option<&OsStr>, _directory: Option<&OsStr>) -> Result<()> {
    Err(crate::off_target(&format!("{verb} {}", file.to_string_lossy())))
}

/// Refuses a string Win32 cannot carry whole.
///
/// Every string the shell takes is a `PCWSTR`, which ends at its first NUL. A
/// target or argument containing one would arrive truncated -- a different
/// program, or a different argument, than the caller asked for -- and the call
/// would then most likely *succeed*. Silent corruption is what spec 18.3 exists
/// to prevent, so this is a refusal rather than a truncation.
///
/// The check is exact on every host and needs no Win32: an [`OsStr`]'s encoded
/// bytes are UTF-8 or WTF-8, and in both a zero byte occurs only where the
/// string genuinely holds U+0000.
///
/// `role` names the offending string in the refusal.
fn carriable(role: &str, text: &OsStr) -> Result<()> {
    if text.as_encoded_bytes().contains(&0) {
        return Err(CoreError::Invalid(format!(
            "the Windows backend will not dispatch a {role} containing a NUL character: \
             Win32 would silently truncate it there"
        )));
    }
    Ok(())
}

/// Whether a string opens with a URI scheme, as RFC 3986 defines one.
///
/// With one Windows-specific narrowing: a single-letter scheme is rejected,
/// because on this platform `C:\Users` is a path and not a `c:` URI, and the
/// shell agrees -- handing it to `ShellExecuteEx` opens the folder. A caller
/// that means a path has [`ProcessLauncher::launch`] for it.
fn has_scheme(uri: &str) -> bool {
    let Some(colon) = uri.find(':') else {
        return false;
    };

    let scheme = &uri.as_bytes()[..colon];
    let [first, rest @ ..] = scheme else {
        return false;
    };
    !rest.is_empty()
        && first.is_ascii_alphabetic()
        && rest
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
}

/// Joins an argument vector into the one string `lpParameters` takes.
///
/// This is the inverse of [`split_arguments`](crate::split_arguments), which is
/// the same thing as saying it is the inverse of `CommandLineToArgvW`: whatever
/// the shell hands the program, its C runtime or `GetCommandLineW` caller
/// splits back, and that must be the vector this launcher was given. Joining
/// with spaces would not be -- one argument holding a space would arrive as
/// two, and an empty argument would vanish entirely.
///
/// The encoding rules follow from the parser's:
///
/// * an argument that is empty or holds a space, a tab or a quote is wrapped in
///   quotes, and anything else is emitted bare, which keeps ordinary command
///   lines readable;
/// * a quote inside an argument is written `\"`, and the backslashes that
///   precede it are doubled first, so the parser's "backslashes are literal
///   unless they precede a quote" rule reproduces them;
/// * backslashes that end a quoted argument are doubled too, because the
///   closing quote would otherwise be the quote they escape.
///
/// A backslash that precedes nothing in particular is left alone: doubling
/// every backslash would turn `C:\dir` into `C:\\dir` on any command line a
/// human then reads.
pub fn quote_arguments(arguments: &[String]) -> String {
    // Enough for the common line, where nothing needs quoting: every argument
    // plus its separator. Quoting grows it, so this is a floor, not a promise.
    let expected = arguments.iter().map(|argument| argument.len() + 1).sum();
    let mut line = String::with_capacity(expected);

    for (index, argument) in arguments.iter().enumerate() {
        if index > 0 {
            line.push(' ');
        }
        quote(argument, &mut line);
    }
    line
}

/// Appends one argument to a command line under the rules above.
fn quote(argument: &str, line: &mut String) {
    // `CommandLineToArgvW` separates on space and tab only, so only those two
    // force quoting -- a newline inside an argument is an ordinary character to
    // it and stays one here.
    if !argument.is_empty() && !argument.contains([' ', '\t', '"']) {
        line.push_str(argument);
        return;
    }

    line.push('"');
    // Held back: what a run of backslashes must become depends on whether a
    // quote follows it, exactly as it does when parsing.
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                line.extend(std::iter::repeat_n('\\', 2 * backslashes + 1));
                line.push('"');
                backslashes = 0;
            }
            character => {
                line.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                line.push(character);
            }
        }
    }
    line.extend(std::iter::repeat_n('\\', 2 * backslashes));
    line.push('"');
}
