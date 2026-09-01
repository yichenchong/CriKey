//! The launcher's own settings surface, the activation hotkey it binds, and the
//! residency rules that make a dismiss survivable (spec 21.2).
//!
//! # Why these three live together
//!
//! They are one defect. A launcher that exits on dismiss is only usable while
//! its window is on screen; a launcher that stays resident is only usable while
//! its activation hotkey works; and a hotkey the platform refuses is only
//! recoverable if the user can reach a settings surface and choose another
//! chord. Splitting them across modules would let one be changed without the
//! other two, and the first hand test on Windows found exactly that gap: the
//! process ran, took no hotkey, exited on the first Escape, and left the owner
//! with no window, no shortcut and nowhere to look.
//!
//! # What the host owns
//!
//! `crikey-ui` owns the panel and the commands; this module owns the answers.
//! It builds the rows the panel renders from the layered store, writes an
//! accepted edit through the user-global layer that `crikey config` reports,
//! and re-binds the accelerator without a restart.

use std::fmt::Write as _;
use std::process::ExitCode;

use crikey_config::{
    ConfigStore, KEY_ACTIVATION_HOTKEY, KEY_MAX_RESULTS, KEY_PROFILE, KEY_ROUNDED_CORNERS, KEY_SHOW_HINTS,
};
use crikey_platform::Accelerator;
use crikey_ui::{LauncherViewModel, SettingControl, SettingRow, UiEffect};

/// The launcher-wide keys the settings surface offers: the key, the label the
/// panel shows, and whether the value is a boolean.
///
/// Launcher keys only. A plugin's settings live in that plugin's own file
/// (spec 21.2, layer 6) and writing one through the user-global layer would put
/// it in a layer the plugin's own file outranks — a setting that appears to
/// save and then does nothing. The publication-timing keys
/// (`launcher.configuration-*-ms`) are deliberately absent: they are tuning for
/// an operator diagnosing a slow publication rather than choices a user makes,
/// and `crikey config` already reports them.
///
/// The boolean column is what stops the renderer guessing. A free-text setting
/// is allowed to hold the word `true`, so a panel that decided by looking at
/// the value would turn that row into a switch and leave the user no way to
/// type anything else into it.
pub(crate) const LAUNCHER_SETTINGS: &[(&str, &str, bool)] = &[
    (KEY_ACTIVATION_HOTKEY, "Activation hotkey", false),
    (KEY_MAX_RESULTS, "Maximum results", false),
    (KEY_PROFILE, "Configuration profile", false),
    (KEY_SHOW_HINTS, "Show keyboard hints", true),
    (KEY_ROUNDED_CORNERS, "Rounded corners", true),
];

/// What the source column says for a key no layer supplies.
///
/// `launcher.max-results` and `launcher.profile` have no built-in default, so
/// an unconfigured launcher genuinely has no value for them and must say so
/// rather than show an empty cell that reads as an empty string.
const UNSET: &str = "unset";

/// The rows the settings panel renders, in the declared order.
///
/// Values go through [`ConfigStore::display_value`] like every other reader in
/// this crate, so a key a plugin declared secret could never reach the panel in
/// clear (spec 21.3). No launcher key is secret today; this is what keeps that
/// true if one ever becomes so.
pub(crate) fn rows(store: Option<&ConfigStore>) -> Vec<SettingRow> {
    LAUNCHER_SETTINGS
        .iter()
        .map(|(key, label, boolean)| {
            let value = store
                .and_then(|store| store.display_value(key))
                .unwrap_or_default()
                .to_owned();
            let source = store
                .and_then(|store| store.layer_of(key))
                .map_or(UNSET, |layer| layer.as_str())
                .to_owned();
            // A switch is drawn from the same reading of the key that decides
            // the launcher's own behaviour, rather than from the displayed
            // text: the readers treat anything but the exact word `false` as
            // on, so a panel that parsed the text itself would show a switch
            // that disagreed with the window beside it.
            let control = if *boolean {
                SettingControl::Toggle {
                    on: boolean_reader(key)(store),
                }
            } else {
                SettingControl::Text
            };
            SettingRow {
                key: (*key).to_owned(),
                label: (*label).to_owned(),
                value,
                source,
                control,
            }
        })
        .collect()
}

/// The reader that decides what a boolean key means to the launcher.
///
/// Indirection with exactly one purpose: the switch in the panel and the
/// behaviour it controls read the key through the same function, so they
/// cannot drift apart. A key added to [`LAUNCHER_SETTINGS`] as a boolean
/// without a reader here is a compile-time hole this closes by panicking in
/// the one place that can see both lists.
fn boolean_reader(key: &str) -> fn(Option<&ConfigStore>) -> bool {
    match key {
        KEY_SHOW_HINTS => configured_show_hints,
        KEY_ROUNDED_CORNERS => configured_rounded_corners,
        other => unreachable!("`{other}` is declared boolean with no reader to say what it means"),
    }
}

/// The accelerator this launch binds.
///
/// The configured value when a store loaded, otherwise the built-in default
/// read out of the same table the store would have supplied it from. A launch
/// whose configuration file is unreadable still needs a chord, and a second
/// copy of the default spelled out here is how the two would drift.
pub(crate) fn configured_hotkey(store: Option<&ConfigStore>) -> String {
    store
        .and_then(|store| store.get(KEY_ACTIVATION_HOTKEY))
        .map(str::to_owned)
        .unwrap_or_else(|| {
            crikey_config::BUILT_IN_DEFAULTS
                .iter()
                .find(|(key, _)| *key == KEY_ACTIVATION_HOTKEY)
                .map_or_else(String::new, |(_, value)| (*value).to_owned())
        })
}

/// Whether this launch draws the footer's navigation hint line.
///
/// Only the exact text `false` hides it, matching
/// [`ConfigStore::plugin_enabled`]: a launcher that read any unrecognised text
/// as "off" would take the hint line away over a typo, and the hint line is
/// where the user would have looked to find out why.
pub(crate) fn configured_show_hints(store: Option<&ConfigStore>) -> bool {
    store.and_then(|store| store.get(KEY_SHOW_HINTS)) != Some("false")
}

/// Whether this launch draws its window with rounded corners.
///
/// Only the exact text `false` squares them off, for the same reason
/// [`configured_show_hints`] is lenient: a launcher that read any unrecognised
/// text as "off" would change the shape of its own window over a typo, and the
/// window is the only place the change shows — nothing on screen would name the
/// misspelled value that caused it.
pub(crate) fn configured_rounded_corners(store: Option<&ConfigStore>) -> bool {
    store.and_then(|store| store.get(KEY_ROUNDED_CORNERS)) != Some("false")
}

/// Refuses a value the launcher could not honour, before it reaches the file.
///
/// Validation happens on the way in rather than on the way out, because the
/// alternative is a configuration file holding a chord no platform will ever
/// register and a launcher that reports the same failure on every start with no
/// hint of which edit caused it.
pub(crate) fn validate(key: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("`{key}` cannot be set to an empty value"));
    }
    match key {
        KEY_ACTIVATION_HOTKEY => Accelerator::parse(value)
            .map(|_| ())
            .map_err(|error| format!("`{value}` is not a valid accelerator: {error}")),
        KEY_MAX_RESULTS => match value.parse::<usize>() {
            Ok(limit) if limit > 0 => Ok(()),
            _ => Err(format!(
                "`{key}` must be a whole number of results above zero, got `{value}`"
            )),
        },
        // A profile names a file inside `config_dir()/profiles`, so a value with
        // a path separator in it would select a file outside the directory the
        // user believes they are choosing from.
        KEY_PROFILE => {
            if value.contains(['/', '\\']) || value == "." || value == ".." {
                Err(format!("`{key}` must be a plain profile name, got `{value}`"))
            } else {
                Ok(())
            }
        }
        // The renderer reads this as a plain boolean, so anything else would be
        // a value the launcher silently treats as "off" — and a user who typed
        // `yes` would be looking at a hidden hint line with a file that says
        // they asked for something else.
        KEY_SHOW_HINTS => {
            if value == "true" || value == "false" {
                Ok(())
            } else {
                Err(format!("`{key}` must be `true` or `false`, got `{value}`"))
            }
        }
        // The window shape is read as a plain boolean too, and this one is
        // worse to get wrong than the hint line: the only report of the value
        // is the shape of the window itself, so `yes` would leave a user
        // looking at square corners with a file that says they asked for round
        // ones and nothing to tell them which text the launcher honours.
        KEY_ROUNDED_CORNERS => {
            if value == "true" || value == "false" {
                Ok(())
            } else {
                Err(format!("`{key}` must be `true` or `false`, got `{value}`"))
            }
        }
        other => Err(format!("`{other}` is not a launcher setting")),
    }
}

/// Writes one accepted setting through the user-global layer and persists it.
///
/// The same layer and the same file `crikey config` reports, so a value changed
/// in the panel is a value the command line agrees about. The returned sentence
/// is what the user is shown: it names the layer that actually wins, because
/// the user-global layer is not the top of the stack — a selected profile or a
/// `crikey run --set` override outranks it, and a panel that reported its own
/// write as effective would be lying about which chord the launcher answers to.
pub(crate) fn persist(store: &mut ConfigStore, key: &str, value: &str) -> Result<String, String> {
    validate(key, value)?;
    store.set_user_global(key, value);
    store
        .save()
        .map_err(|error| format!("`{key}` could not be written: {error}"))?;
    let effective = store.get(key).unwrap_or_default();
    if effective == value {
        Ok(format!("{key} = {value}"))
    } else {
        let layer = store.layer_of(key).map_or(UNSET, |layer| layer.as_str());
        Ok(format!(
            "{key} saved as {value}, but {layer} still supplies {effective}"
        ))
    }
}

// ---------------------------------------------------------------------------
// The activation hotkey
// ---------------------------------------------------------------------------

/// The platform side of one global accelerator.
///
/// A trait rather than a direct call so the rebinding rules — which binding
/// survives a refusal, and in which order the two grabs are taken — are decided
/// by code that can run on a host with no display server at all. Those rules
/// are the part that was wrong; the Win32 and X11 calls underneath are not.
pub(crate) trait HotkeyRegistrar {
    /// Takes `accelerator`, installing the activation handler with it.
    fn register(&mut self, accelerator: &str) -> Result<(), String>;
    /// Releases an accelerator this registrar took.
    fn unregister(&mut self, accelerator: &str) -> Result<(), String>;
}

/// The registrar the launcher actually runs with: the platform backend the
/// search service owns, wired to the renderer's toggle.
///
/// Borrowed rather than owned, and rebuilt at each call site, because the event
/// loop owns the one [`SearchService`] and a registrar that held it for the
/// life of the process would take that borrow away from every query.
///
/// A target with no global-shortcut backend refuses instead of pretending: the
/// launcher then comes up resident with its settings open, which is the honest
/// state of a launcher nothing can raise by keyboard.
///
/// [`SearchService`]: crikey_app::SearchService
pub(crate) struct PlatformHotkeys<'a> {
    pub(crate) search: &'a mut crikey_app::SearchService,
    pub(crate) handle: crikey_ui::NativeLauncherHandle,
}

impl HotkeyRegistrar for PlatformHotkeys<'_> {
    fn register(&mut self, accelerator: &str) -> Result<(), String> {
        #[cfg(any(windows, target_os = "linux"))]
        {
            // A fresh handle per registration: the handler outlives this call
            // and the backend owns it until the next `set_activation_handler`.
            let handle = self.handle.clone();
            self.search
                .register_activation_hotkey(
                    accelerator,
                    Box::new(move |_| {
                        let _ = handle.request_toggle();
                    }),
                )
                .map_err(|error| error.to_string())
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = (&mut self.search, &self.handle);
            Err(unsupported_platform(accelerator))
        }
    }

    fn unregister(&mut self, accelerator: &str) -> Result<(), String> {
        #[cfg(any(windows, target_os = "linux"))]
        {
            self.search
                .unregister_activation_hotkey(accelerator)
                .map_err(|error| error.to_string())
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _ = &mut self.search;
            Err(unsupported_platform(accelerator))
        }
    }
}

/// Why a target with no `Capability::GlobalHotkeys` backend can take no chord.
#[cfg(not(any(windows, target_os = "linux")))]
fn unsupported_platform(accelerator: &str) -> String {
    format!("this build has no global-shortcut backend, so {accelerator} cannot be bound")
}

/// The accelerator this launch currently holds, and the accelerator it last
/// tried to hold.
///
/// Both are needed. `bound` is what must be released when a new chord is taken;
/// `attempted` is what stops a configuration reload from re-attempting, on
/// every poll of the file watcher for the life of the process, a chord the
/// platform has already refused.
#[derive(Debug, Default)]
pub(crate) struct ActivationHotkey {
    bound: Option<String>,
    attempted: Option<String>,
}

impl ActivationHotkey {
    /// The accelerator that is live, or `None` when the launcher holds none.
    pub(crate) fn bound(&self) -> Option<&str> {
        self.bound.as_deref()
    }

    /// Whether `accelerator` is what this launch last tried to bind, refused or
    /// not.
    pub(crate) fn is_current(&self, accelerator: &str) -> bool {
        self.attempted.as_deref() == Some(accelerator)
    }

    /// Takes `accelerator`, keeping whatever is live if the platform refuses.
    ///
    /// The new grab is taken BEFORE the old one is released, and that order is
    /// the whole point: a chord another application already owns must cost the
    /// user nothing, and releasing first would open a window in which a typo in
    /// the settings panel leaves a resident launcher that no key combination
    /// reaches. Register-first has no such window — at worst both chords are
    /// live for the few microseconds between the two calls — so a refusal needs
    /// no recovery path, it simply changes nothing.
    ///
    /// Re-binding the chord that is already live does nothing at all rather
    /// than churning the grab, so a configuration reload that changed some
    /// other key never briefly drops the hotkey.
    ///
    /// `Ok(Some(..))` carries the one non-fatal outcome: the new chord is live
    /// and the old one could not be handed back. That is worth saying, because
    /// the stranded accelerator goes on opening the launcher without appearing
    /// anywhere in its configuration.
    pub(crate) fn bind(
        &mut self,
        registrar: &mut dyn HotkeyRegistrar,
        accelerator: &str,
    ) -> Result<Option<String>, String> {
        self.attempted = Some(accelerator.to_owned());
        if self.bound.as_deref() == Some(accelerator) {
            return Ok(None);
        }
        registrar.register(accelerator)?;
        let Some(previous) = self.bound.replace(accelerator.to_owned()) else {
            return Ok(None);
        };
        match registrar.unregister(&previous) {
            Ok(()) => Ok(None),
            Err(error) => Ok(Some(format!(
                "{accelerator} is now the activation hotkey, but {previous} could not be released \
                 ({error}); it keeps opening CriKey until the launcher restarts"
            ))),
        }
    }
}

/// Applies one `SetSetting` the settings surface asked the host to persist, and
/// re-binds the accelerator when that is the key that changed.
///
/// One function for both halves because they must never disagree: a chord
/// written to disk that the running process is not listening for is the same
/// trap as a chord the process holds that no file records.
pub(crate) fn apply_setting(
    store: Option<&mut ConfigStore>,
    hotkey: &mut ActivationHotkey,
    registrar: &mut dyn HotkeyRegistrar,
    key: &str,
    value: &str,
) -> String {
    let Some(store) = store else {
        return format!(
            "{key} was not saved: this launch could not read its configuration and is running on \
             built-in defaults"
        );
    };
    let saved = match persist(store, key, value) {
        Ok(report) => report,
        Err(refusal) => return refusal,
    };
    if key != KEY_ACTIVATION_HOTKEY {
        return saved;
    }
    // The value that WINS, not the value that was written: a profile may still
    // outrank the user's file, and the launcher must listen for the chord it
    // reports rather than the one the panel typed.
    let effective = configured_hotkey(Some(store));
    match hotkey.bind(registrar, &effective) {
        Ok(None) => saved,
        Ok(Some(warning)) => format!("{saved}; {warning}"),
        Err(reason) => format!(
            "{saved}, but {effective} could not be registered ({reason}); {} stays in force",
            hotkey.bound().unwrap_or("no activation hotkey")
        ),
    }
}

/// Arms the launcher to come up with the settings panel open on the hotkey row,
/// and returns the diagnostic the caller must also record.
///
/// A resident launcher with no working hotkey is the trap the first Windows
/// test fell into: the process is alive, nothing raises it, and nothing on
/// screen says so. Opening the panel on the row that has to change is the only
/// report the user can act on — the stderr line is discarded for a
/// GUI-subsystem process, and the startup log is only found by someone who
/// already suspects the problem.
pub(crate) fn surface_hotkey_failure(
    view_model: &mut LauncherViewModel,
    accelerator: &str,
    reason: &str,
) -> String {
    view_model.open_settings(Some(KEY_ACTIVATION_HOTKEY));
    format!(
        "crikey: the activation hotkey {accelerator} could not be registered: {reason}; CriKey \
         stays running and has opened its settings so another chord can be chosen"
    )
}

// ---------------------------------------------------------------------------
// Residency
// ---------------------------------------------------------------------------

/// What one UI effect does to the window and to the process.
///
/// Only a deliberate quit ends the process. Before this existed the dismiss and
/// execute paths exited whenever the activation hotkey had failed to register,
/// which turned a shortcut conflict on the user's desktop into a launcher that
/// disappeared on the first Escape and could only be started again from a
/// terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Residency {
    /// Hide the window; the process keeps waiting for an activation.
    Hide,
    /// Tear the launcher down: stop the workers, release the launcher lock and
    /// leave.
    Exit,
}

/// The residency consequence of `effect`, or `None` when it has none.
///
/// Deliberately not a function of whether the activation hotkey is live. That
/// input is what produced the defect: it made the launcher's lifetime depend on
/// a condition the user could not see, so one Escape keystroke either hid the
/// window or ended the session depending on what else happened to be running on
/// their desktop.
pub(crate) fn residency(effect: &UiEffect) -> Option<Residency> {
    match effect {
        UiEffect::Dismissed => Some(Residency::Hide),
        UiEffect::Quit => Some(Residency::Exit),
        // A page keeps the launcher exactly where it is. Closing one returns
        // the user to their results, which is a change of surface and not a
        // reason to hide the window or end the process.
        UiEffect::Query(_)
        | UiEffect::Execute { .. }
        | UiEffect::SetSetting { .. }
        | UiEffect::PageInput(_)
        | UiEffect::ResizePage { .. }
        | UiEffect::ClosePage => None,
    }
}

// ---------------------------------------------------------------------------
// `crikey settings`
// ---------------------------------------------------------------------------

/// A completed operation that found nothing wrong.
const EX_OK: u8 = 0;
/// A completed operation that could not read the configuration.
const EX_INVALID: u8 = 1;
/// An argument list this module could not parse.
const EX_USAGE: u8 = 64;

pub(crate) const SETTINGS_USAGE: &str = "\
crikey settings - report and change the launcher's own settings

USAGE:
    crikey settings
    crikey settings set <KEY> <VALUE>
    crikey settings --help

Reports every setting the launcher's settings panel offers: its key, the label
the panel shows, the layer that supplied the winning value, and that value. A
setting no layer supplies is reported with layer=unset.

`set` writes through the same user-global layer the panel writes, so a value
changed here and a value changed in the panel are the same value. A running
launcher picks the change up on its next configuration reload. If a higher
layer - a selected profile, or a `crikey run --set` override - still supplies
a different value, that is said rather than hidden.
";

/// `crikey settings` — the panel's rows, on a terminal.
///
/// The same rows and the same effective values the panel shows, in the
/// whitespace-separated `key=value` shape every other CriKey command prints, so
/// the settings surface is reachable on a machine where the graphical one is
/// exactly what is broken.
pub(crate) fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        None => {}
        Some("-h" | "--help") if args.len() == 1 => {
            print!("{SETTINGS_USAGE}");
            return ExitCode::from(EX_OK);
        }
        Some("set") => return set(&args[1..]),
        Some(other) => {
            eprintln!("crikey: `settings` takes no arguments, got `{other}`\n\n{SETTINGS_USAGE}");
            return ExitCode::from(EX_USAGE);
        }
    }
    let store = match crate::config_commands::load() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    for row in rows(Some(&store)) {
        println!(
            "key={} label={} layer={} value={}",
            encode(&row.key),
            encode(&row.label),
            encode(&row.source),
            encode(&row.value)
        );
    }
    ExitCode::from(EX_OK)
}

/// `crikey settings set <KEY> <VALUE>` — the panel's edit, on a terminal.
///
/// It refuses an unknown key rather than writing it, because a launcher setting
/// nothing reads is indistinguishable from a typo, and a typo silently accepted
/// here is a hotkey the user believes they changed.
fn set(args: &[String]) -> ExitCode {
    let [key, value] = args else {
        eprintln!("crikey: `settings set` takes exactly a key and a value\n\n{SETTINGS_USAGE}");
        return ExitCode::from(EX_USAGE);
    };
    let mut store = match crate::config_commands::load() {
        Ok(store) => store,
        Err(message) => {
            eprintln!("crikey: {message}");
            return ExitCode::from(EX_INVALID);
        }
    };
    match persist(&mut store, key, value) {
        Ok(message) => {
            println!("{message}");
            ExitCode::from(EX_OK)
        }
        Err(message) => {
            eprintln!("crikey: {message}");
            ExitCode::from(EX_INVALID)
        }
    }
}

/// Mirrors `config_commands::encode` exactly (spec §28 output contract).
fn encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crikey_config::ConfigLayer;
    use crikey_platform::{DirectoryConvention, DirectoryEnvironment, StandardDirectories};
    use crikey_ui::UiCommand;

    /// A registrar that records what it was asked to do and refuses whatever it
    /// was told to refuse, standing in for a desktop where another application
    /// already owns the chord.
    #[derive(Debug, Default)]
    struct FakeRegistrar {
        refuse: Vec<String>,
        refuse_release: bool,
        log: Vec<String>,
    }

    impl FakeRegistrar {
        fn refusing(accelerator: &str) -> Self {
            Self {
                refuse: vec![accelerator.to_owned()],
                ..Self::default()
            }
        }

        /// The accelerators the platform is still holding, in the order they
        /// were taken, which is what "exactly one chord is live" is read from.
        fn live(&self) -> Vec<&str> {
            let mut live: Vec<&str> = Vec::new();
            for entry in &self.log {
                let Some((verb, accelerator)) = entry.split_once(' ') else {
                    continue;
                };
                match verb {
                    "register" if !self.refuse.iter().any(|refused| refused == accelerator) => {
                        if !live.contains(&accelerator) {
                            live.push(accelerator);
                        }
                    }
                    "unregister" if !self.refuse_release => live.retain(|held| *held != accelerator),
                    _ => {}
                }
            }
            live
        }
    }

    impl HotkeyRegistrar for FakeRegistrar {
        fn register(&mut self, accelerator: &str) -> Result<(), String> {
            self.log.push(format!("register {accelerator}"));
            if self.refuse.iter().any(|refused| refused == accelerator) {
                return Err("the chord is already owned by another application".to_owned());
            }
            Ok(())
        }

        fn unregister(&mut self, accelerator: &str) -> Result<(), String> {
            self.log.push(format!("unregister {accelerator}"));
            if self.refuse_release {
                return Err("the shortcut service is gone".to_owned());
            }
            Ok(())
        }
    }

    /// A private configuration directory, removed on drop, so a test writes a
    /// real `config.toml` through the real store rather than through a stub.
    struct ConfigDir {
        root: std::path::PathBuf,
    }

    impl ConfigDir {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "crikey-settings-{name}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).expect("a temporary directory can be created");
            Self { root }
        }

        fn store(&self) -> ConfigStore {
            // Every variable any convention consults, pinned to one scratch
            // directory: the Windows base is resolved from `APPDATA` and
            // `LOCALAPPDATA` before the `CRIKEY_*_DIR` overrides are applied,
            // so a fixture that pins only the overrides fails on Windows with
            // a missing variable rather than testing anything about settings.
            let environment = [
                "HOME",
                "APPDATA",
                "LOCALAPPDATA",
                "CRIKEY_CONFIG_DIR",
                "CRIKEY_DATA_DIR",
                "CRIKEY_CACHE_DIR",
                "CRIKEY_STATE_DIR",
            ]
            .into_iter()
            .fold(DirectoryEnvironment::new(), |environment, variable| {
                environment.set(variable, self.root.as_os_str())
            });
            // The host's own convention, not a fixed one: `temp_dir()` answers
            // `C:\...` on Windows, which the XDG rule rightly refuses as not
            // absolute, and the fixture would fail for a reason that has
            // nothing to do with settings.
            let directories = StandardDirectories::resolve(DirectoryConvention::current(), &environment)
                .expect("every directory is pinned by an override");
            ConfigStore::load_with_policy(&directories, None).expect("an empty tree loads")
        }
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_dismiss_hides_the_launcher_and_a_later_activation_shows_it_again() {
        // The defect this closes: dismissing used to end the process whenever
        // the hotkey had not registered, so the owner's only route back to the
        // launcher was a terminal.
        let mut view_model = LauncherViewModel::new();
        view_model.activate();
        let effect = view_model
            .apply(UiCommand::Cancel)
            .expect("cancelling a launcher with an empty query dismisses it");
        assert_eq!(effect, UiEffect::Dismissed);
        assert_eq!(residency(&effect), Some(Residency::Hide));

        view_model.activate();
        assert!(
            view_model.is_visible(),
            "the process survived the dismiss, so the same launcher can be raised again"
        );
    }

    #[test]
    fn only_a_quit_ends_the_process() {
        assert_eq!(residency(&UiEffect::Quit), Some(Residency::Exit));
        assert_eq!(residency(&UiEffect::Query("x".to_owned())), None);
        assert_eq!(
            residency(&UiEffect::SetSetting {
                key: KEY_MAX_RESULTS.to_owned(),
                value: "5".to_owned(),
            }),
            None
        );
    }

    #[test]
    fn a_refused_hotkey_leaves_the_launcher_running_and_opens_the_row_that_fixes_it() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::refusing("Ctrl+Alt+Space");
        let reason = hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect_err("the desktop already owns the chord");
        assert_eq!(hotkey.bound(), None);

        let mut view_model = LauncherViewModel::new();
        let diagnostic = surface_hotkey_failure(&mut view_model, "Ctrl+Alt+Space", &reason);
        assert!(diagnostic.contains("Ctrl+Alt+Space"), "{diagnostic}");
        assert!(diagnostic.contains("already owned"), "{diagnostic}");
        assert!(diagnostic.contains("stays running"), "{diagnostic}");

        view_model.activate();
        let frame = view_model.frame().expect("the launcher comes up visible");
        assert!(frame.settings_open, "the user needs somewhere to fix it");
        assert_eq!(frame.settings_focus.as_deref(), Some(KEY_ACTIVATION_HOTKEY));

        // And it is still resident: an Escape hides rather than exits.
        assert_eq!(residency(&UiEffect::Dismissed), Some(Residency::Hide));
    }

    #[test]
    fn a_refused_rebind_keeps_the_accelerator_that_already_works() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::refusing("Ctrl+Shift+P");
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect("the first chord is free");

        let reason = hotkey
            .bind(&mut registrar, "Ctrl+Shift+P")
            .expect_err("the second chord is taken");
        assert!(reason.contains("already owned"), "{reason}");
        assert_eq!(
            hotkey.bound(),
            Some("Ctrl+Alt+Space"),
            "a refused chord must never cost the user the one that works"
        );
        assert_eq!(
            registrar.live(),
            vec!["Ctrl+Alt+Space"],
            "the live grab is never released on behalf of a chord that was refused"
        );
    }

    #[test]
    fn an_accepted_rebind_leaves_exactly_one_chord_live() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::default();
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect("the first chord is free");
        let warning = hotkey
            .bind(&mut registrar, "Ctrl+Shift+P")
            .expect("the second chord is free");

        assert_eq!(warning, None);
        assert_eq!(hotkey.bound(), Some("Ctrl+Shift+P"));
        assert_eq!(
            registrar.live(),
            vec!["Ctrl+Shift+P"],
            "the old grab is handed back, so one chord opens the launcher and not two"
        );
        assert_eq!(
            registrar.log,
            vec![
                "register Ctrl+Alt+Space".to_owned(),
                "register Ctrl+Shift+P".to_owned(),
                "unregister Ctrl+Alt+Space".to_owned(),
            ],
            "the new chord is taken before the old one is released"
        );
    }

    #[test]
    fn re_binding_the_live_chord_does_not_touch_the_grab() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::default();
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect("the chord is free");
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect("re-binding what is already live is not a failure");

        assert_eq!(registrar.log, vec!["register Ctrl+Alt+Space".to_owned()]);
    }

    #[test]
    fn an_old_chord_that_cannot_be_released_is_reported_rather_than_forgotten() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar {
            refuse_release: true,
            ..FakeRegistrar::default()
        };
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect("the first chord is free");
        let warning = hotkey
            .bind(&mut registrar, "Ctrl+Shift+P")
            .expect("the second chord is free")
            .expect("the stranded grab is worth a sentence");

        assert!(warning.contains("Ctrl+Alt+Space"), "{warning}");
        assert!(warning.contains("could not be released"), "{warning}");
        assert_eq!(hotkey.bound(), Some("Ctrl+Shift+P"));
    }

    #[test]
    fn setting_the_activation_hotkey_persists_to_the_user_global_layer_and_rebinds_live() {
        let directory = ConfigDir::new("hotkey-set");
        let mut store = directory.store();
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::default();
        hotkey
            .bind(&mut registrar, &configured_hotkey(Some(&store)))
            .expect("the default chord is free");
        assert_eq!(hotkey.bound(), Some("Ctrl+Alt+Space"));

        let report = apply_setting(
            Some(&mut store),
            &mut hotkey,
            &mut registrar,
            KEY_ACTIVATION_HOTKEY,
            "Ctrl+Shift+P",
        );

        assert_eq!(report, "launcher.activation-hotkey = Ctrl+Shift+P");
        assert_eq!(
            hotkey.bound(),
            Some("Ctrl+Shift+P"),
            "the running process must listen for the chord it just saved"
        );
        let reloaded = directory.store();
        assert_eq!(reloaded.get(KEY_ACTIVATION_HOTKEY), Some("Ctrl+Shift+P"));
        assert_eq!(
            reloaded.layer_of(KEY_ACTIVATION_HOTKEY),
            Some(ConfigLayer::UserGlobal),
            "the panel writes the same layer `crikey config` reports"
        );
    }

    #[test]
    fn a_hotkey_the_platform_refuses_leaves_the_working_one_in_force() {
        let directory = ConfigDir::new("hotkey-refused");
        let mut store = directory.store();
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::refusing("Ctrl+Shift+P");
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+Space")
            .expect("the default chord is free");

        let report = apply_setting(
            Some(&mut store),
            &mut hotkey,
            &mut registrar,
            KEY_ACTIVATION_HOTKEY,
            "Ctrl+Shift+P",
        );

        assert!(report.contains("could not be registered"), "{report}");
        assert!(report.contains("Ctrl+Alt+Space stays in force"), "{report}");
        assert_eq!(hotkey.bound(), Some("Ctrl+Alt+Space"));
        assert_eq!(registrar.live(), vec!["Ctrl+Alt+Space"]);
    }

    #[test]
    fn an_unparseable_accelerator_never_reaches_the_configuration_file() {
        let directory = ConfigDir::new("hotkey-garbage");
        let mut store = directory.store();
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::default();

        let report = apply_setting(
            Some(&mut store),
            &mut hotkey,
            &mut registrar,
            KEY_ACTIVATION_HOTKEY,
            "Ctrl+",
        );

        assert!(report.contains("not a valid accelerator"), "{report}");
        assert!(registrar.log.is_empty(), "{:?}", registrar.log);
        let reloaded = directory.store();
        assert_eq!(
            reloaded.get(KEY_ACTIVATION_HOTKEY),
            Some("Ctrl+Alt+Space"),
            "the refused edit left the built-in default in place"
        );
    }

    #[test]
    fn a_setting_a_higher_layer_outranks_is_saved_and_reported_as_outranked() {
        let directory = ConfigDir::new("outranked");
        std::fs::write(
            directory.root.join("config.toml"),
            "[launcher]\nprofile = \"work\"\n",
        )
        .expect("the user file can be written");
        std::fs::create_dir_all(directory.root.join("profiles")).expect("profiles directory");
        std::fs::write(
            directory.root.join("profiles").join("work.toml"),
            "[launcher]\nactivation-hotkey = \"Ctrl+Alt+K\"\n",
        )
        .expect("the profile can be written");
        let mut store = directory.store();
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::default();
        hotkey
            .bind(&mut registrar, "Ctrl+Alt+K")
            .expect("the profile's chord is free");

        let report = apply_setting(
            Some(&mut store),
            &mut hotkey,
            &mut registrar,
            KEY_ACTIVATION_HOTKEY,
            "Ctrl+Shift+P",
        );

        assert!(report.contains("still supplies Ctrl+Alt+K"), "{report}");
        assert_eq!(
            hotkey.bound(),
            Some("Ctrl+Alt+K"),
            "the launcher listens for the chord that wins, not the one that was typed"
        );
    }

    #[test]
    fn the_rows_name_every_launcher_setting_and_where_its_value_came_from() {
        let directory = ConfigDir::new("rows");
        let store = directory.store();
        let rows = rows(Some(&store));

        let keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                KEY_ACTIVATION_HOTKEY,
                KEY_MAX_RESULTS,
                KEY_PROFILE,
                KEY_SHOW_HINTS,
                KEY_ROUNDED_CORNERS
            ]
        );
        assert_eq!(rows[0].value, "Ctrl+Alt+Space");
        assert_eq!(rows[0].source, ConfigLayer::BuiltInDefaults.as_str());
        assert_eq!(
            rows[1].source, UNSET,
            "a key no layer supplies must say so rather than show an empty value"
        );
    }

    /// The hint line's setting has to survive the whole way round: the panel
    /// offers it, a write lands in the layer `crikey config` reports, and the
    /// panel then reads back what was written. A key the renderer honours but
    /// the surface never lists is a setting only someone who read the source
    /// could find.
    #[test]
    fn the_hint_line_setting_round_trips_through_the_settings_rows() {
        let directory = ConfigDir::new("show-hints");
        let mut store = directory.store();

        let default = rows(Some(&store))
            .into_iter()
            .find(|row| row.key == KEY_SHOW_HINTS)
            .expect("the panel offers the hint line");
        assert_eq!(default.label, "Show keyboard hints");
        assert_eq!(default.value, "true", "an unconfigured launcher shows its hints");
        assert_eq!(default.source, ConfigLayer::BuiltInDefaults.as_str());
        assert!(configured_show_hints(Some(&store)));

        let report = persist(&mut store, KEY_SHOW_HINTS, "false").expect("the key is a setting");
        assert_eq!(report, "launcher.show-hints = false");

        let reloaded = directory.store();
        let saved = rows(Some(&reloaded))
            .into_iter()
            .find(|row| row.key == KEY_SHOW_HINTS)
            .expect("the panel still offers the hint line");
        assert_eq!(saved.value, "false");
        assert_eq!(saved.source, ConfigLayer::UserGlobal.as_str());
        assert!(
            !configured_show_hints(Some(&reloaded)),
            "the launcher reads back the choice the panel wrote"
        );
    }

    /// The renderer takes this as a plain boolean, so a value it cannot read is
    /// refused on the way in rather than silently treated as "off" -- a user who
    /// typed `yes` would otherwise be looking at a hidden hint line and a
    /// configuration file that says they asked for something else.
    #[test]
    fn a_hint_line_value_that_is_not_a_boolean_is_refused_before_it_is_written() {
        for accepted in ["true", "false"] {
            assert!(
                validate(KEY_SHOW_HINTS, accepted).is_ok(),
                "{accepted} is one of the two values the renderer honours"
            );
        }

        for refused in ["yes", "no", "1", "0", "True", "off"] {
            let error = validate(KEY_SHOW_HINTS, refused).expect_err("only `true` and `false` are honoured");
            assert!(error.contains("must be `true` or `false`"), "{error}");
            assert!(error.contains(refused), "the message names the value: {error}");
        }

        let directory = ConfigDir::new("show-hints-refused");
        let mut store = directory.store();
        assert!(persist(&mut store, KEY_SHOW_HINTS, "yes").is_err());
        assert!(
            configured_show_hints(Some(&directory.store())),
            "a refused edit leaves the launcher showing what it showed before"
        );
    }

    /// The corner setting has to survive the whole way round for the same
    /// reason the hint line does: the panel offers it, the write lands in the
    /// layer `crikey config` reports, and the panel reads back what was
    /// written. A key the renderer honours but the surface never lists is a
    /// setting only someone who read the source could find.
    #[test]
    fn the_rounded_corner_setting_round_trips_through_the_settings_rows() {
        let directory = ConfigDir::new("rounded-corners");
        let mut store = directory.store();

        let default = rows(Some(&store))
            .into_iter()
            .find(|row| row.key == KEY_ROUNDED_CORNERS)
            .expect("the panel offers the window's corners");
        assert_eq!(default.label, "Rounded corners");
        assert_eq!(
            default.value, "true",
            "an unconfigured launcher rounds its corners"
        );
        assert_eq!(default.source, ConfigLayer::BuiltInDefaults.as_str());
        assert!(configured_rounded_corners(Some(&store)));

        let report = persist(&mut store, KEY_ROUNDED_CORNERS, "false").expect("the key is a setting");
        assert_eq!(report, "launcher.rounded-corners = false");

        let reloaded = directory.store();
        let saved = rows(Some(&reloaded))
            .into_iter()
            .find(|row| row.key == KEY_ROUNDED_CORNERS)
            .expect("the panel still offers the window's corners");
        assert_eq!(saved.value, "false");
        assert_eq!(saved.source, ConfigLayer::UserGlobal.as_str());
        assert!(
            !configured_rounded_corners(Some(&reloaded)),
            "the launcher reads back the choice the panel wrote"
        );
    }

    /// The window shape is a plain boolean, so a value the renderer cannot read
    /// is refused on the way in rather than silently squaring the corners off:
    /// the shape of the window is the only report of this setting, so a user who
    /// typed `yes` would have nothing on screen naming the value at fault.
    #[test]
    fn a_rounded_corner_value_that_is_not_a_boolean_is_refused_before_it_is_written() {
        for accepted in ["true", "false"] {
            assert!(
                validate(KEY_ROUNDED_CORNERS, accepted).is_ok(),
                "{accepted} is one of the two values the renderer honours"
            );
        }

        for refused in ["yes", "no", "1", "0", "True", "off"] {
            let error =
                validate(KEY_ROUNDED_CORNERS, refused).expect_err("only `true` and `false` are honoured");
            assert!(error.contains("must be `true` or `false`"), "{error}");
            assert!(error.contains(refused), "the message names the value: {error}");
        }

        let directory = ConfigDir::new("rounded-corners-refused");
        let mut store = directory.store();
        assert!(persist(&mut store, KEY_ROUNDED_CORNERS, "yes").is_err());
        assert!(
            configured_rounded_corners(Some(&directory.store())),
            "a refused edit leaves the window the shape it already had"
        );
    }

    /// A hand-edited configuration file is the one place an unrecognised value
    /// can reach the reader, because [`validate`] never saw it. Only the exact
    /// text `false` may square the corners off: any other text is a typo, and
    /// changing the shape of the window over one would leave the user with no
    /// way to tell a rejected edit from an applied one.
    #[test]
    fn only_the_exact_text_false_squares_the_launchers_corners_off() {
        let directory = ConfigDir::new("rounded-corners-lenient");

        assert!(
            configured_rounded_corners(None),
            "a launch whose configuration would not load still draws a window"
        );

        let mut store = directory.store();
        assert!(persist(&mut store, KEY_ROUNDED_CORNERS, "false").is_ok());
        assert!(!configured_rounded_corners(Some(&directory.store())));

        for typo in ["False", "FALSE", "no", "0", "off", " false"] {
            let mut store = directory.store();
            store.set_user_global(KEY_ROUNDED_CORNERS, typo);
            assert!(
                configured_rounded_corners(Some(&store)),
                "`{typo}` is not `false`, so the corners stay round"
            );
        }
    }

    /// The command line and the panel must be one settings surface, not two:
    /// a value written here is the value the panel then shows, and an unknown
    /// key is refused rather than stored where nothing will ever read it.
    #[test]
    fn a_command_line_write_lands_in_the_same_layer_the_panel_writes() {
        let directory = ConfigDir::new("cli-write");
        let mut store = directory.store();

        let report = persist(&mut store, KEY_ACTIVATION_HOTKEY, "Ctrl+Alt+K").expect("the key is a setting");
        assert_eq!(report, "launcher.activation-hotkey = Ctrl+Alt+K");
        assert_eq!(
            store.layer_of(KEY_ACTIVATION_HOTKEY),
            Some(ConfigLayer::UserGlobal)
        );

        let reloaded = directory.store();
        let row = rows(Some(&reloaded))
            .into_iter()
            .find(|row| row.key == KEY_ACTIVATION_HOTKEY)
            .expect("the panel offers the hotkey");
        assert_eq!(
            row.value, "Ctrl+Alt+K",
            "the panel reads back what the command line wrote"
        );
        assert_eq!(row.source, ConfigLayer::UserGlobal.as_str());

        let refused = persist(&mut store, "launcher.nope", "x").expect_err("an unknown key is not a setting");
        assert!(refused.contains("is not a launcher setting"), "{refused}");
    }

    #[test]
    fn a_configuration_that_could_not_be_loaded_refuses_the_write_instead_of_dropping_it() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::default();
        let report = apply_setting(
            None,
            &mut hotkey,
            &mut registrar,
            KEY_ACTIVATION_HOTKEY,
            "Ctrl+Shift+P",
        );
        assert!(report.contains("was not saved"), "{report}");
        assert!(registrar.log.is_empty(), "{:?}", registrar.log);
    }

    #[test]
    fn a_reload_does_not_re_attempt_a_chord_this_launch_already_tried() {
        let mut hotkey = ActivationHotkey::default();
        let mut registrar = FakeRegistrar::refusing("Ctrl+Alt+Space");
        let _ = hotkey.bind(&mut registrar, "Ctrl+Alt+Space");
        assert!(
            hotkey.is_current("Ctrl+Alt+Space"),
            "a refused chord is still the one this launch is configured for"
        );
        assert!(!hotkey.is_current("Ctrl+Shift+P"));
    }
}
