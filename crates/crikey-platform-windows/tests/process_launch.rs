//! Public-API contract for Windows process launch and URI opening (spec 18.2).
//!
//! This is the launch half of the M1 "run what discovery found" deliverable.
//! `ShellExecuteExW` itself is not pinned here -- it needs a Windows session,
//! and a test that actually launched something would leave a process behind --
//! but everything the shell is *told* is, because that is where a wrong answer
//! would be silent rather than loud.
//!
//! The load-bearing case is the command line. `DiscoveredApplication` records
//! arguments one per slot, `ShellExecuteEx` takes a single `lpParameters`
//! string, and the program on the far end splits that string again with
//! `CommandLineToArgvW`. Two encodings therefore have to agree exactly, and the
//! symptom of their disagreeing is not a crash: it is a program launched with
//! silently different arguments. This crate already owns the forward direction
//! in [`split_arguments`], so the inverse is checked against it -- by hand for
//! the cases a reader should recognise, and exhaustively over every short
//! string in the alphabet that makes quoting hard.
//!
//! The refusals are pinned for the same reason. A NUL truncates a `PCWSTR`
//! instead of failing it, and a schemeless string handed to the shell as a URI
//! is run as a local file, so both are refused before dispatch on every host.

use crikey_core::{CoreError, PlatformPath};
use crikey_platform::ProcessLauncher;
use crikey_platform_windows::{quote_arguments, split_arguments, ShellLauncher, WindowsBackend};

/// The argument vector `args` names, as [`ProcessLauncher::launch`] takes it.
fn arguments(args: &[&str]) -> Vec<String> {
    args.iter().map(|argument| (*argument).to_owned()).collect()
}

/// The reason a refusal gives, or a panic naming what came back instead.
fn refusal(outcome: Result<(), CoreError>) -> String {
    match outcome {
        Err(CoreError::Invalid(reason)) => reason,
        other => panic!("expected a typed refusal, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Quoting: the inverse of `split_arguments`
// ---------------------------------------------------------------------------

#[test]
fn an_argument_vector_becomes_the_command_line_a_reader_would_write() {
    // Pinned by hand so a regression shows up as a diff someone can read,
    // rather than only as a round trip that stopped closing.
    let cases: [(&[&str], &str); 9] = [
        // Nothing to quote: the common line stays legible.
        (&["plain"], "plain"),
        (&["one", "two"], "one two"),
        // A space is the whole reason quoting exists.
        (&["a b"], r#""a b""#),
        // An empty argument is a position, and must not vanish.
        (&[""], r#""""#),
        (&["a b", ""], r#""a b" """#),
        // A quote is escaped, not doubled: `\"` is what the parser reads back.
        (&[r#"a"b"#], r#""a\"b""#),
        (&[r#"say "hi""#], r#""say \"hi\"""#),
        // Backslashes are ordinary until a quote follows them, so an unquoted
        // path keeps exactly the backslashes it was given ...
        (&[r"C:\path\"], r"C:\path\"),
        // ... while the ones that would run into a closing quote are doubled.
        (&[r"C:\my path\"], r#""C:\my path\\""#),
    ];

    for (args, expected) in cases {
        assert_eq!(
            quote_arguments(&arguments(args)),
            expected,
            "{args:?} was not quoted as expected"
        );
    }
}

#[test]
fn no_arguments_is_an_empty_command_line() {
    // Not `""`, which would be one empty argument, and not a stray space.
    assert!(quote_arguments(&[]).is_empty());
}

#[test]
fn quoting_is_not_a_space_join() {
    // The failure this whole encoding exists to prevent: joined with spaces,
    // one argument holding a space would arrive as two.
    let args = arguments(&["C:\\Program Files\\app.exe", "--out", "my file.txt"]);
    let line = quote_arguments(&args);

    assert_ne!(line, args.join(" "));
    assert_eq!(split_arguments(&line), args);
}

#[test]
fn every_short_argument_survives_the_round_trip() {
    for argument in corpus(3) {
        let args = vec![argument];
        let line = quote_arguments(&args);
        assert_eq!(split_arguments(&line), args, "{args:?} was quoted {line:?}");
    }
}

#[test]
fn every_short_argument_pair_survives_the_round_trip() {
    // Pairs, not just single arguments: the separator is where an encoding
    // that quotes each argument correctly can still lose the boundary between
    // them -- an empty argument next to a quoted one, most of all.
    let corpus = corpus(2);
    for left in &corpus {
        for right in &corpus {
            let args = vec![left.clone(), right.clone()];
            let line = quote_arguments(&args);
            assert_eq!(split_arguments(&line), args, "{args:?} was quoted {line:?}");
        }
    }
}

/// Every string of up to `limit` characters over the alphabet that makes
/// command-line quoting hard.
///
/// Exhaustive rather than random: the interesting inputs are short and the
/// alphabet is tiny, so there is no reason to sample a space that can be
/// covered whole, and a failing case is the same one on every run.
fn corpus(limit: usize) -> Vec<String> {
    const ALPHABET: [char; 5] = ['a', ' ', '\t', '"', '\\'];

    let mut all = vec![String::new()];
    let mut frontier = vec![String::new()];
    for _ in 0..limit {
        let mut longer = Vec::with_capacity(frontier.len() * ALPHABET.len());
        for prefix in &frontier {
            for character in ALPHABET {
                let mut candidate = prefix.clone();
                candidate.push(character);
                longer.push(candidate);
            }
        }
        all.extend_from_slice(&longer);
        frontier = longer;
    }
    all
}

// ---------------------------------------------------------------------------
// What never reaches the shell
// ---------------------------------------------------------------------------

#[test]
fn an_empty_target_is_refused() {
    let reason = refusal(ShellLauncher::new().launch(&PlatformPath::new(""), &[]));
    assert!(
        reason.contains("empty target"),
        "the refusal should say why: {reason}"
    );
}

#[test]
fn a_nul_is_refused_rather_than_silently_truncated() {
    let launcher = ShellLauncher::new();

    // A `PCWSTR` ends at its first NUL, so each of these would otherwise reach
    // the shell as a shorter -- and perfectly plausible -- string.
    let cases = [
        launcher.launch(&PlatformPath::new("C:\\Windows\\notepad.exe\0.evil"), &[]),
        launcher.launch(
            &PlatformPath::new("C:\\Windows\\notepad.exe"),
            &arguments(&["--safe\0--dangerous"]),
        ),
        launcher.open_uri("https://example.com/\0"),
    ];

    for outcome in cases {
        let reason = refusal(outcome);
        assert!(reason.contains("NUL"), "the refusal should say why: {reason}");
    }
}

#[test]
fn a_string_that_names_no_scheme_is_not_opened_as_a_uri() {
    // Every one of these is something `ShellExecuteEx` would happily open --
    // as a file to run, which is not what a URI opener was asked for.
    for candidate in [
        "",
        "evil.exe",
        "C:\\Windows\\System32\\cmd.exe",
        "\\\\server\\share\\evil.exe",
        "://example.com",
        "1nvalid://example.com",
        "not a scheme:x",
    ] {
        let reason = refusal(ShellLauncher::new().open_uri(candidate));
        assert!(
            reason.contains("scheme"),
            "{candidate:?} should be refused for naming no scheme, got: {reason}"
        );
    }
}

// ---------------------------------------------------------------------------
// Off target
// ---------------------------------------------------------------------------

#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_launching_fails_instead_of_pretending() {
    // The point of the crate compiling here is that its logic is testable, not
    // that it works. A launch nothing can dispatch must be reported as refused.
    let target = PlatformPath::new("C:\\Windows\\notepad.exe");
    let reason = refusal(ShellLauncher::new().launch(&target, &arguments(&["a b"])));

    assert!(
        reason.contains("does not target Windows"),
        "the refusal should say why: {reason}"
    );
    assert!(
        reason.contains("notepad.exe"),
        "the refusal should name what it would not launch: {reason}"
    );
}

#[cfg(not(target_os = "windows"))]
#[test]
fn off_target_a_well_formed_uri_reaches_the_dispatch_and_is_refused_there() {
    // Reaching the off-target refusal is how a host without Windows can see
    // that the scheme was accepted rather than rejected on the way in.
    for uri in [
        "https://example.com/a?b=c",
        "mailto:someone@example.com",
        "ms-settings:display",
        "shell:AppsFolder",
    ] {
        let reason = refusal(ShellLauncher::new().open_uri(uri));
        assert!(
            reason.contains("does not target Windows"),
            "{uri:?} should have reached the dispatch, got: {reason}"
        );
        assert!(
            reason.contains(uri),
            "the refusal should name what it would not open: {reason}"
        );
    }
}

#[cfg(not(target_os = "windows"))]
#[test]
fn the_backend_hands_out_the_launcher_it_refuses_with() {
    // The accessor is the only way `crikey-app` reaches this service, so it is
    // worth one test that it is wired to something live.
    let backend = WindowsBackend::new();
    let reason = refusal(
        backend
            .process_launcher()
            .launch(&PlatformPath::new("C:\\Windows\\notepad.exe"), &[]),
    );

    assert!(
        reason.contains("does not target Windows"),
        "the refusal should say why: {reason}"
    );
}

#[cfg(target_os = "windows")]
#[test]
fn the_backend_hands_out_a_launcher() {
    // On target the same accessor is checked without dispatching anything: a
    // test that actually launched a program would leave one running.
    let backend = WindowsBackend::new();
    let reason = refusal(backend.process_launcher().open_uri("evil.exe"));

    assert!(reason.contains("scheme"), "the refusal should say why: {reason}");
}
