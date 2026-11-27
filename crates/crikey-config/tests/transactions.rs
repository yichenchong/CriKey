//! Deterministic optimistic-write cases; no sleeps or timing assertions.
mod support;
use crikey_config::command_preferences::{CommandPreference, CommandPreferences, KEY_COMMAND_PREFERENCES};
use crikey_config::{ConfigError, ConfigLayer, ConfigStore};
use crikey_core::{command::CommandRef, PluginId};
use support::Fixture;

fn preferences() -> CommandPreferences {
    CommandPreferences {
        records: vec![CommandPreference {
            reference: CommandRef {
                provider_id: PluginId("unavailable.owner".into()),
                command_id: "retained".into(),
            },
            hidden: true,
            alias: Some("saved".into()),
            hotkey: None,
        }],
        ..Default::default()
    }
}

#[test]
fn stale_writer_cannot_overwrite_unrelated_external_key() {
    let fixture = Fixture::new("transaction-external");
    fixture.user_global("[launcher]\nmax-results = 10\n");
    let mut stale = fixture.load().unwrap();
    let expected = stale.revision();
    fixture.user_global("[launcher]\nmax-results = 20\nexternal-key = \"keep\"\n");
    assert!(matches!(
        stale.commit_command_preferences_if_revision(&preferences(), false, &expected),
        Err(ConfigError::Conflict)
    ));
    assert_eq!(stale.get("launcher.max-results"), Some("10"));
    let fresh = fixture.load().unwrap();
    assert_eq!(fresh.get("launcher.external-key"), Some("keep"));
    assert_eq!(fresh.get(KEY_COMMAND_PREFERENCES), None);
}

#[test]
fn two_admitted_writers_have_exactly_one_winner() {
    let fixture = Fixture::new("transaction-writers");
    let first = fixture.load().unwrap();
    let second = fixture.load().unwrap();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, mut store)| {
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let expected = store.revision();
                barrier.wait();
                store.commit_user_global("launcher.writer", &index.to_string(), &expected)
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ConfigError::Conflict)))
            .count(),
        1
    );
}

#[test]
fn creation_deletion_and_atomic_replacement_conflict() {
    let fixture = Fixture::new("transaction-identities");
    let mut before_create = fixture.load().unwrap();
    fixture.user_global("[launcher]\nvalue = 1\n");
    assert!(matches!(
        before_create.commit_user_global("launcher.edit", "one", &before_create.revision()),
        Err(ConfigError::Conflict)
    ));
    let mut before_delete = fixture.load().unwrap();
    fixture.remove_user_global();
    assert!(matches!(
        before_delete.commit_user_global("launcher.edit", "two", &before_delete.revision()),
        Err(ConfigError::Conflict)
    ));
    fixture.user_global("[launcher]\nvalue = 1\n");
    let mut before_replace = fixture.load().unwrap();
    // NamedTempFile uses portable replace-existing semantics on Windows too.
    let mut replacement = tempfile::NamedTempFile::new_in(fixture.config_dir()).unwrap();
    std::io::Write::write_all(&mut replacement, b"[launcher]\nvalue = 2\n").unwrap();
    replacement.persist(before_replace.config_path()).unwrap();
    assert!(matches!(
        before_replace.commit_user_global("launcher.edit", "three", &before_replace.revision()),
        Err(ConfigError::Conflict)
    ));
}

#[test]
fn failed_lock_acquisition_preserves_published_state_and_disk() {
    let fixture = Fixture::new("transaction-failure");
    fixture.user_global("[launcher]\nvalue = \"original\"\n");
    let mut store = fixture.load().unwrap();
    let revision = store.revision();
    let original = std::fs::read(store.config_path()).unwrap();
    std::fs::create_dir(fixture.config_dir().join(".configuration-writer.lock")).unwrap();
    assert!(matches!(
        store.commit_user_global("launcher.value", "replacement", &revision),
        Err(ConfigError::Write { .. })
    ));
    assert_eq!(store.get("launcher.value"), Some("original"));
    assert_eq!(store.revision(), revision);
    assert_eq!(std::fs::read(store.config_path()).unwrap(), original);
}

#[test]
fn profile_session_and_shadowed_document_changes_are_retained() {
    let fixture = Fixture::new("transaction-shadowing");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    fixture.profile("work", "[launcher]\nvalue = \"profile\"\n");
    fixture.policy("[launcher]\npolicy-only = \"keep\"\n");
    let overrides = vec![("launcher.value".into(), "session".into())];
    let mut store =
        ConfigStore::load_with_overrides(&fixture.directories(), Some(&fixture.policy_path()), &overrides)
            .unwrap();
    store
        .commit_user_global("launcher.value", "user", &store.revision())
        .unwrap();
    assert_eq!(store.get("launcher.value"), Some("session"));
    assert_eq!(
        store.layer_of("launcher.value"),
        Some(ConfigLayer::SessionOverride)
    );
    assert_eq!(store.get("launcher.policy-only"), Some("keep"));
    fixture.profile("work", "[launcher]\nvalue = \"changed-but-shadowed\"\n");
    assert!(matches!(
        store.commit_command_preferences_if_revision(&preferences(), false, &store.revision()),
        Err(ConfigError::Conflict)
    ));
}

#[test]
fn session_preferences_never_touch_disk_and_unavailable_refs_remain_editable() {
    let fixture = Fixture::new("transaction-session");
    let mut store = fixture.load().unwrap();
    let document = preferences();
    store
        .commit_command_preferences_if_revision(&document, true, &store.revision())
        .unwrap();
    assert!(!store.config_path().exists());
    assert!(!fixture.config_dir().join(".configuration-writer.lock").exists());
    assert_eq!(
        store
            .command_preferences_at(ConfigLayer::SessionOverride)
            .unwrap(),
        Some(document)
    );
}

#[test]
fn revisions_and_preference_parse_errors_never_expose_source_values() {
    let fixture = Fixture::new("transaction-redaction");
    fixture.user_global("[plugins.modern.example.settings]\nsecret = \"private-token\"\n");
    let store = fixture.load().unwrap();
    assert!(!format!("{:?}", store.revision()).contains("private-token"));
    let error = CommandPreferences::decode("private-token = [invalid").unwrap_err();
    assert!(!error.contains("private-token"));
}

#[test]
fn subprocess_transaction_writer() {
    let Some(root) = std::env::var_os("CRIKEY_TRANSACTION_CHILD_ROOT") else {
        return;
    };
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
    .fold(
        crikey_platform::DirectoryEnvironment::new(),
        |environment, key| environment.set(key, &root),
    );
    let directories = crikey_platform::StandardDirectories::resolve(
        crikey_platform::DirectoryConvention::current(),
        &environment,
    )
    .unwrap();
    let mut store = ConfigStore::load_with_policy(&directories, None).unwrap();
    let expected = store.revision();
    println!("\nTRANSACTION_CHILD_READY");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    let mut command = String::new();
    std::io::stdin().read_line(&mut command).unwrap();
    assert_eq!(command.trim(), "commit");
    assert!(matches!(
        store.commit_user_global("launcher.child", "stale", &expected),
        Err(ConfigError::Conflict)
    ));
}

#[test]
fn a_separate_process_cannot_overwrite_the_committed_revision() {
    use std::io::{BufRead, Write};
    let fixture = Fixture::new("transaction-process");
    let mut store = fixture.load_without_policy().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "subprocess_transaction_writer", "--nocapture"])
        .env("CRIKEY_TRANSACTION_CHILD_ROOT", fixture.config_dir())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut output = std::io::BufReader::new(child.stdout.take().unwrap());
    loop {
        let mut line = String::new();
        assert_ne!(
            output.read_line(&mut line).unwrap(),
            0,
            "child exited before loading its snapshot"
        );
        if line.contains("TRANSACTION_CHILD_READY") {
            break;
        }
    }
    store
        .commit_user_global("launcher.parent", "committed", &store.revision())
        .unwrap();
    child.stdin.take().unwrap().write_all(b"commit\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert_eq!(
        fixture.load_without_policy().unwrap().get("launcher.parent"),
        Some("committed")
    );
    assert_eq!(fixture.load_without_policy().unwrap().get("launcher.child"), None);
}

#[test]
fn explicit_stale_revision_is_rejected_even_after_local_publication() {
    let fixture = Fixture::new("transaction-local-revision");
    let mut store = fixture.load().unwrap();
    let old_revision = store.revision();
    store
        .commit_command_preferences_if_revision(&preferences(), false, &old_revision)
        .unwrap();
    assert_eq!(
        store.command_preferences_at(ConfigLayer::UserGlobal).unwrap(),
        Some(preferences())
    );
    assert!(matches!(
        store.commit_user_global("launcher.value", "stale", &old_revision),
        Err(ConfigError::Conflict)
    ));
    assert_eq!(store.get("launcher.value"), None);
}

#[test]
fn selecting_a_new_profile_publishes_its_actual_effective_values() {
    let fixture = Fixture::new("transaction-profile-selection");
    fixture.profile("work", "[launcher]\nvalue = \"work\"\n");
    fixture.profile("home", "[launcher]\nvalue = \"home\"\n");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    let mut store = fixture.load().unwrap();
    store
        .commit_user_global("launcher.profile", "home", &store.revision())
        .unwrap();
    assert_eq!(store.get("launcher.value"), Some("home"));
    assert_eq!(store.layer_of("launcher.value"), Some(ConfigLayer::Profile));
}

#[test]
fn invalid_new_profile_is_rejected_before_persisting_or_publishing() {
    let fixture = Fixture::new("transaction-profile-invalid");
    fixture.profile("broken", "[launcher]\nvalue = [invalid\n");
    let mut store = fixture.load().unwrap();
    let revision = store.revision();
    assert!(matches!(
        store.commit_user_global("launcher.profile", "broken", &revision),
        Err(ConfigError::Parse { .. })
    ));
    assert_eq!(store.revision(), revision);
    assert!(!store.config_path().exists());
    assert_eq!(store.get("launcher.profile"), None);
}

#[test]
fn session_preferences_reject_external_changes_to_a_shadowed_profile() {
    let fixture = Fixture::new("transaction-session-shadowed-source");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    fixture.profile("work", "[launcher]\nvalue = \"profile\"\n");
    let overrides = vec![("launcher.value".to_owned(), "session".to_owned())];
    let mut store =
        ConfigStore::load_with_overrides(&fixture.directories(), Some(&fixture.policy_path()), &overrides)
            .unwrap();
    let original = preferences();
    store
        .commit_command_preferences_if_revision(&original, true, &store.revision())
        .unwrap();
    let expected = store.revision();
    fixture.profile("work", "[launcher]\nvalue = \"external-but-shadowed\"\n");
    let disk_before = std::fs::read(store.config_path()).unwrap();
    assert!(matches!(
        store.commit_command_preferences_if_revision(&CommandPreferences::default(), true, &expected),
        Err(ConfigError::Conflict)
    ));
    assert_eq!(store.revision(), expected);
    assert_eq!(store.get("launcher.value"), Some("session"));
    assert_eq!(
        store
            .command_preferences_at(ConfigLayer::SessionOverride)
            .unwrap(),
        Some(original)
    );
    assert_eq!(std::fs::read(store.config_path()).unwrap(), disk_before);
    assert!(!fixture.config_dir().join(".configuration-writer.lock").exists());
}

#[test]
fn directories_are_rejected_at_every_configuration_document_path() {
    for kind in ["global", "profile", "policy", "plugin"] {
        let fixture = Fixture::new(&format!("transaction-directory-document-{kind}"));
        let path = match kind {
            "global" => fixture.config_dir().join("config.toml"),
            "profile" => {
                fixture.user_global("[launcher]\nprofile = \"work\"\n");
                fixture.config_dir().join("profiles").join("work.toml")
            }
            "policy" => fixture.policy_path(),
            "plugin" => fixture.config_dir().join("plugins").join("modern.example.toml"),
            _ => unreachable!(),
        };
        std::fs::create_dir_all(&path).unwrap();
        assert!(
            matches!(fixture.load(), Err(ConfigError::Read { path: actual, .. }) if actual == path),
            "{kind} directory must not be treated as an absent optional document"
        );
    }
}

#[test]
fn empty_plugin_directory_remains_a_valid_watched_directory() {
    let fixture = Fixture::new("transaction-empty-plugin-directory");
    std::fs::create_dir(fixture.config_dir().join("plugins")).unwrap();
    let mut store = fixture.load().unwrap();
    let expected = store.revision();
    fixture.plugin_settings("modern.example", "[settings]\nvalue = \"created\"\n");
    assert!(matches!(
        store.commit_command_preferences_if_revision(&preferences(), true, &expected),
        Err(ConfigError::Conflict)
    ));
}

fn plugin_owner(fixture: &Fixture) -> crikey_config::DiscoveredSchema {
    crikey_config::DiscoveredSchema {
        plugin: PluginId("modern.example".into()),
        package: fixture.config_dir().join("installed-example"),
        section: support::schema(
            "[[configuration.field]]\nname = \"limit\"\ntype = \"integer\"\n\
             default = 10\nminimum = 1\nmaximum = 100\n",
        ),
    }
}

#[test]
fn plugin_transaction_writes_the_consumer_file_and_preserves_raw_unrelated_fields() {
    let fixture = Fixture::new("plugin-transaction-consumer");
    fixture.user_global("[plugins.modern.example.settings]\nlimit = 2\n");
    fixture.plugin_settings(
        "modern.example",
        "enabled = false\n[settings]\nlimit = 3\nunknown = \"keep\"\n",
    );
    fixture.plugin_settings("modern.other", "[settings]\nvalue = \"other\"\n");
    let global = std::fs::read(fixture.config_dir().join("config.toml")).unwrap();
    let other = std::fs::read(fixture.config_dir().join("plugins/modern.other.toml")).unwrap();
    let owner = plugin_owner(&fixture);
    let mut store = fixture.load().unwrap();
    store.register_discovered_schema(&owner);
    store
        .commit_plugin_user(&owner, "limit", "42", &store.revision())
        .unwrap();
    let mut consumer = fixture.load().unwrap();
    consumer.register_discovered_schema(&owner);
    let key = "plugins.modern.example.settings.limit";
    assert_eq!(consumer.get(key), Some("42"));
    assert_eq!(consumer.layer_of(key), Some(ConfigLayer::UserPlugin));
    assert_eq!(store.get(key), consumer.get(key));
    assert_eq!(consumer.get("plugins.modern.example.enabled"), Some("false"));
    assert_eq!(
        consumer.get("plugins.modern.example.settings.unknown"),
        Some("keep")
    );
    assert_eq!(
        std::fs::read(fixture.config_dir().join("config.toml")).unwrap(),
        global
    );
    assert_eq!(
        std::fs::read(fixture.config_dir().join("plugins/modern.other.toml")).unwrap(),
        other
    );
}

#[test]
fn plugin_transaction_retains_policy_profile_schema_and_session_shadowing() {
    let fixture = Fixture::new("plugin-transaction-shadowing");
    fixture.user_global("[launcher]\nprofile = \"work\"\n");
    fixture.profile("work", "[launcher]\nvalue = \"profile\"\n");
    fixture.policy("[launcher]\npolicy-only = \"policy\"\n");
    let key = "plugins.modern.example.settings.limit";
    let overrides = vec![(key.to_owned(), "99".to_owned())];
    let mut store =
        ConfigStore::load_with_overrides(&fixture.directories(), Some(&fixture.policy_path()), &overrides)
            .unwrap();
    let owner = plugin_owner(&fixture);
    assert!(store.register_discovered_schema(&owner).is_empty());
    store
        .commit_plugin_user(&owner, "limit", "42", &store.revision())
        .unwrap();
    assert_eq!(store.get(key), Some("99"));
    assert_eq!(store.layer_of(key), Some(ConfigLayer::SessionOverride));
    assert_eq!(store.get("launcher.value"), Some("profile"));
    assert_eq!(store.get("launcher.policy-only"), Some("policy"));
    let restarted = store.reload_without_session_overrides().unwrap();
    assert_eq!(restarted.get(key), Some("42"));
    assert_eq!(restarted.layer_of(key), Some(ConfigLayer::UserPlugin));
    fixture.profile("work", "[launcher]\nvalue = \"external\"\n");
    assert!(matches!(
        store.commit_plugin_user(&owner, "limit", "43", &store.revision()),
        Err(ConfigError::Conflict)
    ));
}

#[test]
fn plugin_writers_share_global_writer_authority() {
    let fixture = Fixture::new("plugin-transaction-writers");
    let owner = plugin_owner(&fixture);
    let mut first = fixture.load().unwrap();
    let mut second = fixture.load().unwrap();
    first.register_discovered_schema(&owner);
    second.register_discovered_schema(&owner);
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let handles: Vec<_> = [first, second]
        .into_iter()
        .enumerate()
        .map(|(index, mut store)| {
            let barrier = barrier.clone();
            let owner = owner.clone();
            std::thread::spawn(move || {
                let expected = store.revision();
                barrier.wait();
                if index == 0 {
                    store.commit_plugin_user(&owner, "limit", "42", &expected)
                } else {
                    store.commit_user_global("launcher.writer", "other", &expected)
                }
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|handle| handle.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ConfigError::Conflict)))
            .count(),
        1
    );
}

#[test]
fn plugin_owner_schema_and_validation_are_admission_authority() {
    let fixture = Fixture::new("plugin-transaction-schema");
    let mut owner = plugin_owner(&fixture);
    owner.section.fields[0].secret = true;
    let mut store = fixture.load().unwrap();
    store.register_plugin_schema(&owner.plugin, &owner.section);
    assert!(matches!(
        store.commit_plugin_user(&owner, "limit", "42", &store.revision()),
        Err(ConfigError::Setting { .. })
    ));
    store.register_discovered_schema(&owner);
    let expected = store.revision();
    for (field, value) in [("missing", "42"), ("limit", "0"), ("limit", "private-token")] {
        let error = store
            .commit_plugin_user(&owner, field, value, &expected)
            .unwrap_err();
        assert!(!format!("{error:?}").contains("private-token"));
        assert!(!error.to_string().contains("private-token"));
    }
    let mut replacement = owner.clone();
    replacement.section =
        support::schema("[[configuration.field]]\nname = \"limit\"\ntype = \"integer\"\ndefault = 20\n");
    store.register_discovered_schema(&replacement);
    assert_ne!(store.revision(), expected);
    assert!(matches!(
        store.commit_plugin_user(&replacement, "limit", "42", &expected),
        Err(ConfigError::Conflict)
    ));
    assert!(matches!(
        store.commit_plugin_user(&owner, "limit", "42", &store.revision()),
        Err(ConfigError::Conflict)
    ));
    let mut moved = replacement.clone();
    moved.package = fixture.config_dir().join("another-package");
    assert!(matches!(
        store.commit_plugin_user(&moved, "limit", "42", &store.revision()),
        Err(ConfigError::Conflict)
    ));
    assert!(!fixture.config_dir().join("plugins").exists());
}

#[test]
fn plugin_transaction_rejects_portably_unsafe_ids_without_creating_paths() {
    let fixture = Fixture::new("plugin-transaction-paths");
    for id in [
        "../outside",
        "modern...escape",
        "modern.a/b",
        "modern.a\\b",
        "modern.a:b",
        "modern.a.",
        "bare",
        "CON.example",
        "lPt1.example",
    ] {
        let mut owner = plugin_owner(&fixture);
        owner.plugin = PluginId(id.into());
        let mut store = fixture.load().unwrap();
        store.register_discovered_schema(&owner);
        assert!(matches!(
            store.commit_plugin_user(&owner, "limit", "42", &store.revision()),
            Err(ConfigError::Setting { .. })
        ));
    }
    assert!(!fixture.config_dir().join("plugins").exists());
}

#[test]
fn plugin_target_creation_deletion_and_replacement_are_conflicts() {
    let fixture = Fixture::new("plugin-transaction-external");
    let owner = plugin_owner(&fixture);
    for operation in ["create", "delete", "replace"] {
        if operation == "create" {
            fixture.remove_plugin_settings("modern.example");
        } else {
            fixture.plugin_settings("modern.example", "[settings]\nlimit = 1\n");
        }
        let mut store = fixture.load().unwrap();
        store.register_discovered_schema(&owner);
        let expected = store.revision();
        match operation {
            "create" => fixture.plugin_settings("modern.example", "[settings]\nlimit = 2\n"),
            "delete" => fixture.remove_plugin_settings("modern.example"),
            "replace" => {
                let mut replacement = tempfile::NamedTempFile::new_in(fixture.config_dir()).unwrap();
                std::io::Write::write_all(&mut replacement, b"[settings]\nlimit = 2\n").unwrap();
                replacement
                    .persist(fixture.config_dir().join("plugins/modern.example.toml"))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(matches!(
            store.commit_plugin_user(&owner, "limit", "42", &expected),
            Err(ConfigError::Conflict)
        ));
        assert_eq!(store.revision(), expected);
    }
}

#[cfg(unix)]
#[test]
fn plugin_transaction_refuses_symlinks_and_keeps_files_private() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let fixture = Fixture::new("plugin-transaction-symlinks");
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(
        outside.path().join("modern.example.toml"),
        "[settings]\nlimit = 1\n",
    )
    .unwrap();
    symlink(outside.path(), fixture.config_dir().join("plugins")).unwrap();
    let owner = plugin_owner(&fixture);
    let mut store = fixture.load().unwrap();
    store.register_discovered_schema(&owner);
    assert!(matches!(
        store.commit_plugin_user(&owner, "limit", "42", &store.revision()),
        Err(ConfigError::Write { .. })
    ));
    std::fs::remove_file(fixture.config_dir().join("plugins")).unwrap();
    std::fs::create_dir(fixture.config_dir().join("plugins")).unwrap();
    symlink(
        outside.path().join("modern.example.toml"),
        fixture.config_dir().join("plugins/modern.example.toml"),
    )
    .unwrap();
    let mut store = fixture.load().unwrap();
    store.register_discovered_schema(&owner);
    assert!(matches!(
        store.commit_plugin_user(&owner, "limit", "42", &store.revision()),
        Err(ConfigError::Write { .. })
    ));
    assert_eq!(
        std::fs::read_to_string(outside.path().join("modern.example.toml")).unwrap(),
        "[settings]\nlimit = 1\n"
    );
    std::fs::remove_file(fixture.config_dir().join("plugins/modern.example.toml")).unwrap();
    let mut store = fixture.load().unwrap();
    store.register_discovered_schema(&owner);
    store
        .commit_plugin_user(&owner, "limit", "42", &store.revision())
        .unwrap();
    let path = fixture.config_dir().join("plugins/modern.example.toml");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut store = store.reload().unwrap();
    store
        .commit_plugin_user(&owner, "limit", "43", &store.revision())
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn plugin_transaction_rejects_a_declared_but_inapplicable_field() {
    let fixture = Fixture::new("plugin-transaction-platform");
    let mut owner = plugin_owner(&fixture);
    let elsewhere = if std::env::consts::OS == "windows" {
        "linux"
    } else {
        "windows"
    };
    owner.section = support::schema(&format!(
        "[[configuration.field]]\nname = \"limit\"\nplatforms = [\"{elsewhere}\"]\n",
    ));
    let mut store = fixture.load().unwrap();
    assert!(store.register_discovered_schema(&owner).is_empty());
    let expected = store.revision();
    assert!(matches!(
        store.commit_plugin_user(&owner, "limit", "42", &expected),
        Err(ConfigError::Setting { .. })
    ));
    assert_eq!(store.revision(), expected);
    assert!(!fixture.config_dir().join("plugins").exists());
}

#[test]
fn plugin_enable_transaction_fresh_reads_concurrent_settings_and_retains_session() {
    let fixture = Fixture::new("plugin-enable-fresh-settings");
    fixture.user_global("[launcher]\nshow-hints = true\n");
    let mut package = fixture.load().unwrap();
    package.set_session_override("launcher.show-hints", "false");
    let mut settings = fixture.load().unwrap();
    settings
        .commit_user_global("launcher.command-home", "favorites", &settings.revision())
        .unwrap();
    package
        .commit_plugin_enabled(&PluginId("modern.example".into()), false)
        .unwrap();
    assert_eq!(package.get("launcher.command-home"), Some("favorites"));
    assert_eq!(package.get("launcher.show-hints"), Some("false"));
    assert_eq!(package.get("plugins.modern.example.enabled"), Some("false"));
    let disk = fixture.load().unwrap();
    assert_eq!(disk.get("launcher.command-home"), Some("favorites"));
    assert_eq!(disk.get("launcher.show-hints"), Some("true"));
    assert_eq!(disk.get("plugins.modern.example.enabled"), Some("false"));
}
