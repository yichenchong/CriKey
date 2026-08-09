//! Live evidence that a confined child is actually confined.
//!
//! Every test here spawns a real process and observes what the kernel let it
//! do. Nothing is asserted by construction: a sandbox that silently failed to
//! install would pass a test that only inspected the policy object, and that
//! is exactly the defect worth catching.
//!
//! Linux only, because Landlock is. A missing Landlock kernel is a NAMED
//! FAILURE, never a skip — an unenforced sandbox that reports itself as
//! enforced is the bug these tests exist to prevent, so a run that proves
//! nothing must not look like a run that proved something.

#![cfg(target_os = "linux")]

use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crikey_sandbox::{plugin_policy, SandboxMode, SandboxPolicy};

/// A directory removed when the guard drops, including on panic.
struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Self {
        // A shell command names these paths, so the name must contain nothing
        // a shell would parse: no spaces, no parentheses, no quotes.
        static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let unique = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("crikey-sandbox-{name}-{}-{unique}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("the scratch directory is created");
        Self { path }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Runs `/bin/sh -c program` under `policy` and returns (success, stderr).
fn run_confined(policy: &SandboxPolicy, program: &str) -> (bool, String) {
    let sandbox = policy.prepare();
    assert!(
        sandbox.is_active(),
        "this kernel did not install the sandbox, so the test proves nothing: {}",
        sandbox.report()
    );
    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(program)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    sandbox.install(&mut command);
    let output = command.output().expect("the confined child starts");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// The property the whole crate exists for: a plugin cannot write outside the
/// directories the host named for it.
#[test]
fn a_confined_child_cannot_write_outside_its_allowlist() {
    let allowed = Scratch::new("allowed");
    let forbidden = Scratch::new("forbidden");
    // The forbidden directory is created by the parent and is writable by this
    // user: only the sandbox stands between the child and it.
    let target = forbidden.path.join("written.txt");
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(&allowed.path);

    let (succeeded, _) = run_confined(&policy, &format!("echo denied > {}", target.display()));
    assert!(!succeeded, "the child wrote outside its allowlist");
    assert!(
        !target.exists(),
        "the file exists, so the write was refused only in the exit status"
    );

    // A parent write to the same path still works, proving the refusal came
    // from the child's confinement rather than from directory permissions.
    fs::write(&target, b"parent").expect("the parent can write there");
    assert!(target.exists());
}

/// The complement: the allowlist is not decorative.
#[test]
fn a_confined_child_writes_inside_its_allowlist() {
    let allowed = Scratch::new("inside");
    let target = allowed.path.join("written.txt");
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(&allowed.path);

    let (succeeded, stderr) = run_confined(&policy, &format!("echo allowed > {}", target.display()));
    assert!(
        succeeded,
        "the child could not write where it was allowed: {stderr}"
    );
    let written = fs::read_to_string(&target).expect("the child's file is readable");
    assert_eq!(written.trim(), "allowed");
}

/// Deleting and renaming are writes too. A policy that only stopped `open` for
/// writing would leave a plugin able to delete the user's files.
#[test]
fn a_confined_child_cannot_delete_or_rename_outside_its_allowlist() {
    let allowed = Scratch::new("mutate-allowed");
    let forbidden = Scratch::new("mutate-forbidden");
    let victim = forbidden.path.join("victim.txt");
    fs::write(&victim, b"original").expect("the parent creates the victim file");
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(&allowed.path);

    let (removed, _) = run_confined(&policy, &format!("rm -f {}", victim.display()));
    assert!(!removed, "the child deleted a file outside its allowlist");
    assert!(victim.exists(), "the victim file was removed");

    let renamed_to = forbidden.path.join("renamed.txt");
    let (renamed, _) = run_confined(
        &policy,
        &format!("mv {} {}", victim.display(), renamed_to.display()),
    );
    assert!(!renamed, "the child renamed a file outside its allowlist");
    assert!(victim.exists() && !renamed_to.exists());
}

/// Reads are deliberately unrestricted, and the documentation says so. If this
/// ever fails, either the crate started restricting reads — in which case the
/// module documentation and every capability report are now lying in the other
/// direction — or the handled rights leaked into the read path.
#[test]
fn a_confined_child_still_reads_everything_the_user_can_read() {
    let allowed = Scratch::new("read-allowed");
    let elsewhere = Scratch::new("read-elsewhere");
    let secret = elsewhere.path.join("secret.txt");
    fs::write(&secret, b"readable").expect("the parent writes the file");
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(&allowed.path);

    // The copy lands inside the allowlist: this test is about reads, and a
    // bare policy does not include the `/dev/null` baseline, so redirecting
    // there would measure the write side by accident.
    let copy = allowed.path.join("copy.txt");
    let (succeeded, stderr) =
        run_confined(&policy, &format!("cat {} > {}", secret.display(), copy.display()));
    assert!(
        succeeded,
        "a confined child could not read a file the user can read: {stderr}"
    );
    assert_eq!(
        fs::read_to_string(&copy).expect("the copy exists").trim(),
        "readable"
    );
}

/// `/dev/null` is in the baseline allowlist because redirecting output there is
/// ordinary. A plugin runtime that cannot open it fails in ways that look like
/// a CriKey bug rather than a policy decision.
#[test]
fn the_baseline_policy_leaves_the_usual_device_files_writable() {
    let policy = plugin_policy(Vec::<PathBuf>::new(), false).with_mode(SandboxMode::Enforce);
    let (succeeded, stderr) = run_confined(&policy, "echo discarded > /dev/null");
    assert!(succeeded, "the baseline policy broke /dev/null: {stderr}");
}

/// The temporary directory is granted for the same reason, and the report says
/// which paths were actually granted rather than which were asked for.
#[test]
fn the_report_names_the_paths_the_kernel_accepted_and_the_ones_it_skipped() {
    let allowed = Scratch::new("report");
    let missing = std::env::temp_dir().join("crikey-sandbox-does-not-exist-ever");
    let _ = fs::remove_dir_all(&missing);
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(&allowed.path)
        .allow_write(&missing);
    let sandbox = policy.prepare();
    let report = sandbox.report();

    assert!(report.filesystem_write.is_enforced(), "{report}");
    assert!(
        report.writable.contains(&allowed.path),
        "the granted path is missing from the report: {report:?}"
    );
    assert!(
        report.skipped.contains(&missing),
        "a path that does not exist must be reported as skipped, not as granted: {report:?}"
    );
    assert!(
        !report.writable.contains(&missing),
        "a path the kernel never saw must not be reported as writable"
    );
}

/// The operator override must actually disable enforcement, and must say that
/// it did: a report claiming confinement on an unconfined child is worse than
/// no report at all.
#[test]
fn the_operator_override_disables_enforcement_and_reports_it() {
    let forbidden = Scratch::new("override");
    let target = forbidden.path.join("written.txt");
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Off)
        .allow_write(std::env::temp_dir());
    let sandbox = policy.prepare();

    assert!(!sandbox.is_active(), "the override left the sandbox active");
    assert!(
        !sandbox.report().filesystem_write.is_enforced(),
        "an unconfined child must not be reported as confined"
    );

    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(format!("echo unconfined > {}", target.display()))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    sandbox.install(&mut command);
    let status = command.status().expect("the unconfined child starts");
    assert!(
        status.success() && target.exists(),
        "the override did not disable confinement"
    );
}

/// `SandboxMode` fails closed: a misspelled override confines the child.
#[test]
fn an_unrecognised_override_value_still_confines() {
    assert_eq!(SandboxMode::from_value(Some("off")), SandboxMode::Off);
    assert_eq!(SandboxMode::from_value(Some("  OFF  ")), SandboxMode::Off);
    assert_eq!(SandboxMode::from_value(Some("of")), SandboxMode::Enforce);
    assert_eq!(SandboxMode::from_value(Some("")), SandboxMode::Enforce);
    assert_eq!(SandboxMode::from_value(None), SandboxMode::Enforce);
}

/// TCP denial is a separate mechanism with a separate report field, and it is
/// only claimed when the kernel implements Landlock ABI 4 or later.
#[test]
fn a_child_denied_tcp_cannot_connect_while_one_that_was_not_can() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("the test binds a loopback listener");
    let port = listener.local_addr().expect("the listener has an address").port();
    std::thread::spawn(move || {
        // One accepted connection per successful attempt; the thread ends with
        // the test process.
        while let Ok((mut stream, _)) = listener.accept() {
            let mut discard = [0_u8; 16];
            let _ = stream.read(&mut discard);
            let _ = stream.write_all(b"ok");
        }
    });

    let permitted = plugin_policy(Vec::<PathBuf>::new(), false).with_mode(SandboxMode::Enforce);
    let permitted_sandbox = permitted.prepare();
    assert_eq!(
        permitted_sandbox.report().tcp_network,
        crikey_sandbox::Enforcement::NotRequested,
        "a policy that did not ask for TCP denial must not report one"
    );

    let denied = plugin_policy(Vec::<PathBuf>::new(), true).with_mode(SandboxMode::Enforce);
    let denied_sandbox = denied.prepare();
    let tcp = denied_sandbox.report().tcp_network.clone();
    if !tcp.is_enforced() {
        panic!("this kernel cannot enforce the TCP denial this test measures: {tcp}");
    }

    // `/bin/sh` here is dash, which has no `/dev/tcp`, so the client is a real
    // one: CPython opening a real socket. python3 is already a hard
    // requirement of this workspace's test suite.
    let connect = format!(
        "python3 -c \"import socket,sys; \
         s=socket.socket(); s.settimeout(5); \
         sys.exit(0 if s.connect_ex(('127.0.0.1',{port}))==0 else 1)\""
    );
    let (allowed_connected, allowed_stderr) = run_confined(&permitted, &connect);
    let (denied_connected, _) = run_confined(&denied, &connect);
    assert!(
        allowed_connected,
        "the unrestricted child could not reach the listener, so the denial below \
         proves nothing: {allowed_stderr}"
    );
    assert!(!denied_connected, "the confined child opened a TCP connection");

    // And the parent can still reach it, so the listener was alive throughout.
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("the parent connects");
    stream.write_all(b"parent").expect("the parent writes");
}

/// The confinement survives `exec` into another program, which is what makes
/// it useful for an interpreter that spawns helpers: a plugin cannot escape by
/// running something else.
#[test]
fn the_confinement_is_inherited_by_a_grandchild() {
    let allowed = Scratch::new("inherit-allowed");
    let forbidden = Scratch::new("inherit-forbidden");
    let target = forbidden.path.join("grandchild.txt");
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(&allowed.path);

    let (succeeded, _) = run_confined(
        &policy,
        &format!("/bin/sh -c 'echo grandchild > {}'", target.display()),
    );
    assert!(!succeeded, "a grandchild escaped the confinement");
    assert!(!target.exists());
}

/// A path the host names but that does not exist is not an error: the plugin's
/// cache directory may simply not have been created yet.
#[test]
fn a_missing_allowlist_path_does_not_fail_the_spawn() {
    let missing = Path::new("/nonexistent-crikey-sandbox-path");
    assert!(!missing.exists());
    let policy = SandboxPolicy::default()
        .with_mode(SandboxMode::Enforce)
        .allow_write(std::env::temp_dir())
        .allow_write(missing);
    let (succeeded, stderr) = run_confined(&policy, "true");
    assert!(succeeded, "a missing allowlist path broke the spawn: {stderr}");
}
