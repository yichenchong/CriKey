//! Real-worker declaration intake. These are backend regressions, not P4 UI acceptance.
use crikey_app::{
    CatalogBuildResult, DisabledPlugins, ModernDriver, ModernProvider, PipelineConfig, QueryPipeline,
};
use crikey_core::{
    command::{CommandAvailability, ExecutableTarget},
    Generation, PluginId,
};
use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn load(commands: &str) -> (Self, ModernProvider, QueryPipeline) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let root = std::env::temp_dir().join(format!(
            "crikey-modern-commands-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let package = root.join("plugins/fixture");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("crikey.toml"), "manifest-version = 1\n[plugin]\nid = \"fixture\"\nname = \"Fixture\"\nversion = \"1.0.0\"\nruntime = \"python\"\nentrypoint = \"commands_plugin:Fixture\"\n[python]\nrequires-python = \">=3.12\"\n").unwrap();
        fs::write(
            package.join("commands_plugin.py"),
            format!("{SOURCE}\n{commands}\n"),
        )
        .unwrap();
        let mut pipeline = QueryPipeline::new(PipelineConfig::default());
        let provider = ModernProvider::load(
            &mut pipeline,
            &[root.join("plugins")],
            None,
            root.join("cache"),
            &DisabledPlugins::default(),
        );
        assert_eq!(provider.plugins(), &[owner()], "{:?}", provider.unavailable());
        (Self { root }, provider, pipeline)
    }
    fn marker(&self, name: &str) -> PathBuf {
        self.root.join("plugins/fixture").join(name)
    }
    fn events(&self) -> String {
        fs::read_to_string(self.marker("events")).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}
fn owner() -> PluginId {
    PluginId("modern.fixture".into())
}
fn wait_for(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !condition() {
        assert!(Instant::now() < deadline, "real worker did not complete");
        thread::sleep(Duration::from_millis(1));
    }
}
fn drain(provider: &mut ModernProvider) -> CatalogBuildResult {
    let mut outcome = None;
    wait_for(|| {
        outcome = provider.take_catalog_results().into_iter().next();
        outcome.is_some()
    });
    outcome.unwrap()
}
fn build(provider: &mut ModernProvider, instance: u64) -> u64 {
    let request = provider
        .request_catalog_build(&owner(), instance, Generation::ZERO)
        .unwrap();
    assert!(matches!(drain(provider), CatalogBuildResult::Complete(_)));
    request
}
const SOURCE: &str = r#"
import os
import time
from pathlib import Path
from crikey_sdk.plugin import Action, Command, Item, Plugin
ROOT = Path(__file__).parent

def record(event):
    with (ROOT / 'events').open('a') as output:
        output.write(event + ':' + str(os.getpid()) + '\n')

class Fixture(Plugin):
    def start(self):
        self.catalog_built = False
        record('start')

    def build_catalog(self):
        self.catalog_built = True
        record('catalog')
        return [Item(stable_id='installed', label='Catalog item', target='target',
                     actions=[Action(action_id='run', label='Run', execution_policy='plugin'),
                              Action(action_id='host', label='Host', execution_policy='host-mediated'),
                              Action(action_id='wrong-category', label='Wrong category',
                                     execution_policy='plugin', applicable_categories=('url',))])]

    def suggest(self, query, context):
        context.emit(Item(stable_id='transient', label='Transient', target='target',
                          metadata={'command_id': 'not-authority'}))

    def stop(self):
        record('stop')
"#;
const READY: &str = r#"
    def commands(self):
        assert self.catalog_built, 'must use the actual catalog producer'
        record('commands')
        return [Command(command_id='durable', label='Explicit label', item_id='installed', action_id='run')]
"#;

#[test]
fn negotiated_declarations_use_catalog_producer_and_survive_healthy_static_release() {
    let (fixture, mut provider, pipeline) = Fixture::load(READY);
    let initial = provider.command_snapshot();
    let request = build(&mut provider, 7);
    let installed = provider.command_snapshot();
    let source = &installed.owners[&owner()];
    assert_eq!(format!("{:?}", source.status), "Ready");
    assert_eq!(source.request_id, request);
    assert_eq!(source.instance, 7);
    assert_eq!(source.generation, Generation::ZERO);
    assert_ne!(source.incarnation, 0);
    let command = &source.declarations[0];
    assert_eq!(command.reference.provider_id, owner());
    assert_eq!(command.reference.command_id, "durable");
    assert_eq!(command.label, "Explicit label");
    assert_eq!(command.availability, CommandAvailability::Available);
    assert!(
        matches!(&command.executable_target, ExecutableTarget::Item { item_id, action_id: Some(action) } if item_id.0 == "installed" && action.0 == "run")
    );
    assert!(
        initial.owners.is_empty(),
        "previous snapshots must stay immutable"
    );
    assert!(provider.command_changes_since(installed.revision).is_none());
    assert_eq!(
        provider.command_changes_since(initial.revision).unwrap().revision,
        installed.revision
    );
    assert!(provider.failed_workers().is_empty());
    assert!(provider.dispatch_failures().is_empty());
    let events = fixture.events();
    let lines: Vec<_> = events.lines().collect();
    assert_eq!(
        lines.len(),
        4,
        "one child, one catalog, one commands callback and healthy release: {events}"
    );
    let pid = lines[0].split(':').nth(1).unwrap();
    assert!(lines.iter().all(|line| line.split(':').nth(1) == Some(pid)));
    assert!(
        lines[3].starts_with("stop:"),
        "static producer was released before publication"
    );
    let driver = ModernDriver::spawn(provider, pipeline, |_| {});
    assert_eq!(driver.command_snapshot().revision, installed.revision);
    assert!(driver.command_changes_since(installed.revision).is_none());
    assert!(driver.invalidate_command_source(&owner(), request));
    assert!(driver.command_snapshot().owners[&owner()].declarations.is_empty());
    assert_eq!(installed.owners[&owner()].declarations.len(), 1);
}

#[test]
fn asynchronous_and_empty_negotiated_declarations_are_supported() {
    for (callback, count) in [
        ("    async def commands(self):\n        assert self.catalog_built\n        return [Command(command_id='default', label='Default', item_id='installed')]\n", 1),
        ("    def commands(self):\n        return []\n", 0),
    ] {
        let (_fixture, mut provider, _pipeline) = Fixture::load(callback);
        build(&mut provider, 1);
        let snapshot = provider.command_snapshot();
        assert_eq!(format!("{:?}", snapshot.owners[&owner()].status), "Ready");
        assert_eq!(snapshot.owners[&owner()].declarations.len(), count);
    }
}

#[test]
fn unsupported_is_explicit_and_catalog_metadata_does_not_declare_commands() {
    let (_fixture, mut provider, _pipeline) = Fixture::load("");
    build(&mut provider, 1);
    let snapshot = provider.command_snapshot();
    assert_eq!(format!("{:?}", snapshot.owners[&owner()].status), "Unsupported");
    assert!(snapshot.owners[&owner()].declarations.is_empty());
    assert!(provider.failed_workers().is_empty());
    assert!(provider.dispatch_failures().is_empty());
}

#[test]
fn exact_catalog_target_action_and_argument_policy_are_required() {
    for (item, action, policy) in [
        ("transient", "None", "forbidden"),
        ("other-owner-item", "None", "forbidden"),
        ("installed", "'missing'", "forbidden"),
        ("installed", "'host'", "forbidden"),
        ("installed", "'wrong-category'", "forbidden"),
        ("installed", "'run'", "required"),
    ] {
        let callback = format!("    def commands(self):\n        return [Command(command_id='durable', label='Explicit', item_id='{item}', action_id={action}, argument_policy='{policy}')]\n");
        let (_fixture, mut provider, _pipeline) = Fixture::load(&callback);
        build(&mut provider, 1);
        let snapshot = provider.command_snapshot();
        let source = &snapshot.owners[&owner()];
        assert!(
            format!("{:?}", source.status).starts_with("Invalid("),
            "{:?}",
            source.status
        );
        assert!(source.declarations.is_empty());
        assert!(
            provider.failed_workers().is_empty(),
            "invalid declarations do not invalidate ordinary catalog data"
        );
    }
}

#[test]
fn omitted_action_validates_primary_but_preserves_actionless_plugin_callbacks() {
    for (actions, available) in [
        ("[Action(action_id='host', label='Host')]", false),
        ("[Action(action_id='run', label='Run', execution_policy='plugin', applicable_categories=('url',))]", false),
        ("[Action(action_id='run', label='Run', execution_policy='plugin'), Action(action_id='run', label='Duplicate', execution_policy='plugin')]", false),
        ("[Action(action_id='run', label='Run', execution_policy='plugin'), Action(action_id='host', label='Host')]", true),
        ("[]", true),
    ] {
        let callback = format!(r#"
    def build_catalog(self):
        record('catalog')
        return [Item(stable_id='installed', label='Catalog item', target='target', actions={actions})]

    def commands(self):
        return [Command(command_id='default', label='Default', item_id='installed')]
"#);
        let (_fixture, mut provider, _pipeline) = Fixture::load(&callback);
        build(&mut provider, 1);
        let snapshot = provider.command_snapshot();
        let source = &snapshot.owners[&owner()];
        if available {
            assert_eq!(format!("{:?}", source.status), "Ready");
            assert!(matches!(&source.declarations[0].executable_target,
                ExecutableTarget::Item { action_id: None, .. }));
        } else {
            assert!(format!("{:?}", source.status).starts_with("Invalid("));
            assert!(source.declarations.is_empty());
        }
        assert!(provider.failed_workers().is_empty());
    }
}

#[test]
fn repeated_target_discovery_and_invocation_reject_any_duplicate_action_id() {
    for (duplicate, expected_count) in [("none", 32), ("run", 0), ("unused", 0)] {
        let callback = format!(
            r#"
    def build_catalog(self):
        record('catalog')
        actions = [Action(action_id='run', label='Run', execution_policy='plugin'),
                   Action(action_id='unused', label='Unused', execution_policy='plugin')]
        if '{duplicate}' != 'none':
            actions.append(Action(action_id='{duplicate}', label='Duplicate', execution_policy='plugin'))
        return [Item(stable_id='installed', label='Catalog item', target='target', actions=actions)]

    def commands(self):
        return [Command(command_id='durable-' + str(index), label='Explicit ' + str(index),
                        item_id='installed', action_id='run') for index in range(32)]

    def execute(self, item, action_id, argument):
        assert item.stable_id == 'installed' and action_id == 'run' and argument is None
        written = (ROOT / 'executed').write_text('installed:run', encoding='utf-8')
        assert written == len('installed:run')
"#
        );
        let (fixture, mut provider, pipeline) = Fixture::load(&callback);
        provider
            .request_catalog_build(&owner(), 1, Generation::ZERO)
            .unwrap();
        let CatalogBuildResult::Complete(catalog) = drain(&mut provider) else {
            panic!("duplicate command actions must not discard the ordinary catalog");
        };
        assert_eq!(catalog.items.len(), 1);
        assert_eq!(catalog.items[0].stable_id.0, "installed");
        let snapshot = provider.command_snapshot();
        let source = &snapshot.owners[&owner()];
        if expected_count == 0 {
            assert!(format!("{:?}", source.status).starts_with("Invalid("));
        } else {
            assert_eq!(format!("{:?}", source.status), "Ready");
        }
        assert_eq!(source.declarations.len(), expected_count);
        assert!(provider.failed_workers().is_empty());
        let declaration = source.declarations.first().cloned().unwrap_or_else(|| {
            crikey_core::command::CommandDeclaration {
                reference: crikey_core::command::CommandRef {
                    provider_id: owner(),
                    command_id: "durable-0".into(),
                },
                label: "Explicit 0".into(),
                executable_target: ExecutableTarget::Item {
                    item_id: crikey_core::ItemId("installed".into()),
                    action_id: Some(crikey_core::ActionId("run".into())),
                },
                availability: CommandAvailability::Available,
                argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            }
        });
        let pinned_source = crikey_app::DeclaredCommandSource {
            instance: source.instance,
            generation: source.generation,
            producer: source.incarnation,
            publication: source.request_id,
        };
        let driver = ModernDriver::spawn(provider, pipeline, |_| {});
        let endpoint = driver.action_executor();
        let submission = endpoint.submit_declared_command_with_receipt(&declaration, &pinned_source, None);
        if expected_count != 0 {
            let receipt = submission.expect("a discovered valid target must pass canonical admission");
            let mut completion = None;
            wait_for(|| {
                completion = endpoint.poll_plugin_actions().into_iter().next();
                completion.is_some()
            });
            let completion = completion.unwrap();
            assert_eq!(completion.request_id, receipt.request_id);
            assert_eq!(completion.outcome.unwrap(), crikey_app::ActionEffect::Completed);
            assert_eq!(
                fs::read_to_string(fixture.marker("executed")).unwrap(),
                "installed:run"
            );
        } else {
            assert!(
                submission.is_err(),
                "invalid discovery cannot be revived by a forged declaration: {duplicate}"
            );
            assert!(endpoint.poll_plugin_actions().is_empty());
            assert!(!fixture.marker("executed").exists());
        }
    }
}

#[test]
fn command_errors_do_not_discard_successful_ordinary_catalogs() {
    for callback in [
        "    def commands(self):\n        raise RuntimeError('callback failed')\n",
        "    def commands(self):\n        return [Command(command_id='duplicate', label='One', item_id='installed'), Command(command_id='duplicate', label='Two', item_id='installed')]\n",
    ] {
        let (_fixture, mut provider, _pipeline) = Fixture::load(callback);
        build(&mut provider, 1);
        let snapshot = provider.command_snapshot();
        let source = &snapshot.owners[&owner()];
        assert!(format!("{:?}", source.status).starts_with("Failed("));
        assert!(source.declarations.is_empty());
        assert!(provider.failed_workers().is_empty());
        assert!(provider.dispatch_failures().is_empty());
    }
}

#[test]
fn hostile_wire_authority_fields_are_rejected_without_losing_catalog_rows() {
    let callback = r#"
    def commands(self):
        import json
        import sys
        worker = sys.modules['__main__']
        original = worker._emit_line
        def inject(line):
            frame = json.loads(line)
            if frame.get('kind') == 'commands_result':
                frame['declarations'][0]['provider_id'] = 'crikey.host'
                frame['declarations'][0]['availability'] = 'available'
                frame['declarations'][0]['executable_target'] = {'host': 'open-settings'}
                line = json.dumps(frame) + '\n'
            original(line)
        worker._emit_line = inject
        return [Command(command_id='spoof', label='Spoof', item_id='installed')]
"#;
    let (_fixture, mut provider, _pipeline) = Fixture::load(callback);
    build(&mut provider, 1);
    let snapshot = provider.command_snapshot();
    let source = &snapshot.owners[&owner()];
    assert!(format!("{:?}", source.status).starts_with("Failed("));
    assert!(source.declarations.is_empty());
}

#[test]
fn retired_inflight_source_cannot_publish_and_old_invalidation_cannot_revoke_new_source() {
    let callback = r#"
    def commands(self):
        (ROOT / 'entered').touch()
        deadline = time.monotonic() + 20
        while not (ROOT / 'release').exists():
            if time.monotonic() > deadline:
                raise RuntimeError('test release not delivered')
            time.sleep(0.001)
        return [Command(command_id='durable', label='Explicit', item_id='installed')]
"#;
    let (fixture, mut provider, _pipeline) = Fixture::load(callback);
    let old = provider
        .request_catalog_build(&owner(), 1, Generation::ZERO)
        .unwrap();
    wait_for(|| fixture.marker("entered").exists());
    assert!(
        provider
            .request_catalog_build(&owner(), 1, Generation::ZERO)
            .is_err(),
        "CatalogBusy must refuse a second producer"
    );
    assert!(provider.invalidate_command_source(&owner(), old));
    let retired_revision = provider.command_snapshot().revision;
    fs::write(fixture.marker("release"), "").unwrap();
    assert!(matches!(drain(&mut provider), CatalogBuildResult::Complete(_)));
    assert_eq!(
        provider.command_snapshot().revision,
        retired_revision,
        "late publication was rejected"
    );
    assert!(provider.command_snapshot().owners[&owner()]
        .declarations
        .is_empty());
    let new = build(&mut provider, 2);
    assert!(new > old);
    assert!(!provider.invalidate_command_source(&owner(), old));
    assert_eq!(provider.command_snapshot().owners[&owner()].declarations.len(), 1);
    assert!(
        provider
            .request_catalog_build(&owner(), 1, Generation::ZERO)
            .is_err(),
        "old catalog instance must not become a new command source"
    );
    assert_eq!(
        fixture
            .events()
            .lines()
            .filter(|line| line.starts_with("start:"))
            .count(),
        2,
        "one producer per accepted build, never a parallel replacement"
    );
}

#[test]
fn released_static_command_keeps_canonical_target_argument_and_source_authority() {
    let callback = format!(
        r#"{READY}

    def execute(self, item, action_id, argument):
        assert item.stable_id == 'installed'
        assert item.target == 'target'
        assert action_id == 'run' and argument is None
        record('execute-canonical')
"#
    );
    let (fixture, mut provider, pipeline) = Fixture::load(&callback);
    let publication = build(&mut provider, 9);
    let snapshot = provider.command_snapshot();
    let current = &snapshot.owners[&owner()];
    let declaration = current.declarations[0].clone();
    let source = crikey_app::DeclaredCommandSource {
        instance: current.instance,
        generation: current.generation,
        producer: current.incarnation,
        publication,
    };
    assert!(fixture.events().lines().any(|line| line.starts_with("stop:")));
    let driver = ModernDriver::spawn(provider, pipeline, |_| {});
    let endpoint = driver.action_executor();
    let receipt = endpoint
        .submit_declared_command_with_receipt(&declaration, &source, None)
        .unwrap();
    let mut completions = Vec::new();
    wait_for(|| {
        completions.extend(endpoint.poll_plugin_actions());
        !completions.is_empty()
    });
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].request_id, receipt.request_id);
    assert!(completions[0].outcome.is_ok());
    assert_eq!(
        fixture
            .events()
            .lines()
            .filter(|line| line.starts_with("execute-canonical:"))
            .count(),
        1
    );
    assert!(driver.invalidate_command_source(&owner(), publication));
    assert!(endpoint
        .submit_declared_command_with_receipt(&declaration, &source, None)
        .is_err());
    assert!(endpoint.poll_plugin_actions().is_empty());
}
