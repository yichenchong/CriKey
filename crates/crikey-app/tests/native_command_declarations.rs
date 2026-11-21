//! Real SDK children exercise declaration intake independently of registry/UI composition.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::thread;
use std::time::{Duration, Instant};

use crikey_app::{CatalogBuildResult, DisabledPlugins, NativeProvider, PipelineConfig, QueryPipeline};
use crikey_app::{NativeCommandSnapshot, NativeCommandStatus};
use crikey_core::command::{CommandAvailability, ExecutableTarget};
use crikey_core::{Generation, PluginId};
use crikey_plugin_supervisor::BudgetKind;

include!("support/native_command_fixture.rs");
fn await_catalog_retirement(pipeline: &QueryPipeline, owner: &PluginId) {
    wait_for(|| {
        (pipeline
            .plugin_budget(owner)
            .unwrap()
            .in_flight(BudgetKind::Catalog)
            == 0)
            .then_some(())
    });
}

#[test]
fn canonical_commands_execute_exact_and_actionless_callbacks_on_the_action_queue() {
    for mode in ["action", "primary-actionless"] {
        let (scratch, pipeline, mut provider, owner) = setup(mode);
        provider
            .request_catalog_build(&owner, 7, Generation::from_raw(1))
            .unwrap();
        assert_catalog(complete(&mut provider));
        let commands = snapshot(&provider, &owner);
        assert_eq!(commands.status, NativeCommandStatus::Ready);
        let source = crikey_app::DeclaredCommandSource {
            instance: commands.instance,
            generation: commands.generation,
            producer: commands.producer,
            publication: commands.revision,
        };
        let driver = crikey_app::NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
        let endpoint = driver.action_executor();
        let declaration = &commands.declarations[0];
        let mut stale = source.clone();
        stale.publication += 1;
        assert!(endpoint
            .submit_declared_command_with_receipt(declaration, &stale, None)
            .is_err());
        assert!(endpoint
            .submit_declared_command_with_receipt(declaration, &source, Some("forbidden"))
            .is_err());
        let receipt = endpoint
            .submit_declared_command_with_receipt(declaration, &source, None)
            .unwrap();
        assert!(receipt.query_history_allowed);
        let completion = wait_for(|| endpoint.poll_plugin_actions().into_iter().next());
        assert_eq!(completion.request_id, receipt.request_id);
        assert_eq!(completion.plugin, owner);
        assert_eq!(completion.item_id.0, "installed");
        assert_eq!(
            completion.action_id.as_ref().map(|id| id.0.as_str()),
            if mode == "primary-actionless" {
                None
            } else {
                Some("alternate")
            }
        );
        assert_eq!(completion.outcome.unwrap(), crikey_app::ActionEffect::Completed);
        assert_eq!(
            fs::read_to_string(scratch.0.join("commands/executed")).unwrap(),
            if mode == "primary-actionless" {
                "installed:<default>"
            } else {
                "installed:alternate"
            }
        );
        assert!(
            endpoint.poll_plugin_actions().is_empty(),
            "one terminal completion per admission"
        );
    }
}

#[test]
fn negotiated_exact_catalog_worker_binds_host_owned_identity_and_survives_healthy_release() {
    let (_scratch, mut pipeline, mut provider, owner) = setup("action");
    provider
        .request_catalog_build(&owner, 7, Generation::from_raw(1))
        .unwrap();
    let catalog_pid = assert_catalog(complete(&mut provider));
    let commands = snapshot(&provider, &owner);
    assert_eq!(commands.status, NativeCommandStatus::Ready);
    assert_eq!(
        (commands.instance, commands.generation),
        (7, Generation::from_raw(1))
    );
    assert_ne!(commands.producer, 0);
    let declaration = &commands.declarations[0];
    assert_eq!(
        declaration.label, catalog_pid,
        "declarations must run in the catalog-producing child"
    );
    assert_eq!(declaration.reference.provider_id, owner);
    assert_eq!(declaration.availability, CommandAvailability::Available);
    let ExecutableTarget::Item { item_id, action_id } = &declaration.executable_target else {
        panic!("plugin gained host-target authority");
    };
    assert_eq!(item_id.0, "installed");
    assert_eq!(action_id.as_ref().unwrap().0, "alternate");
    let frame = provider.drive_query(&mut pipeline, "new worker", 10).unwrap();
    assert!(
        frame.rows.iter().any(|row| row.label != catalog_pid),
        "static healthy producer must have been released"
    );
    let after = snapshot(&provider, &owner);
    assert!(
        Arc::ptr_eq(&commands, &after),
        "preview-worker replacement must not invalidate an installed command"
    );
    assert!(provider.dispatch_failures().is_empty());
}

#[test]
fn unsupported_invalid_and_terminal_callback_outcomes_preserve_ordinary_catalogs() {
    for mode in [
        "unsupported",
        "bad-target",
        "bad-action",
        "host-action",
        "bad-arguments",
        "error",
        "cancelled",
        "deadline",
    ] {
        let (_scratch, _pipeline, mut provider, owner) = setup(mode);
        provider
            .request_catalog_build(&owner, 1, Generation::from_raw(1))
            .unwrap();
        assert_catalog(complete(&mut provider));
        let commands = snapshot(&provider, &owner);
        match mode {
            "unsupported" => assert_eq!(commands.status, NativeCommandStatus::Unsupported),
            "bad-target" | "bad-action" | "host-action" | "bad-arguments" => {
                assert!(matches!(commands.status, NativeCommandStatus::Invalid(_)))
            }
            "cancelled" => assert_eq!(commands.status, NativeCommandStatus::Cancelled),
            _ => assert!(
                matches!(commands.status, NativeCommandStatus::Failed(_)),
                "{mode}: {:?}",
                commands.status
            ),
        }
        assert!(commands.declarations.is_empty());
    }
}

#[test]
fn undrained_reply_from_a_replaced_worker_cannot_publish_declarations() {
    let (_scratch, mut pipeline, mut provider, owner) = setup("action");
    provider
        .request_catalog_build(&owner, 1, Generation::from_raw(1))
        .unwrap();
    await_catalog_retirement(&pipeline, &owner);
    // Retirement proves the first source replied and released before this query
    // starts a replacement; no scheduler timing assumption or source-text test.
    provider.drive_query(&mut pipeline, "replacement", 1).unwrap();
    assert_catalog(complete(&mut provider));
    let commands = snapshot(&provider, &owner);
    assert_eq!(commands.status, NativeCommandStatus::Stale);
    assert!(commands.declarations.is_empty());
}

#[test]
fn newer_catalog_request_and_explicit_invalidation_fence_stale_publication() {
    let (scratch, pipeline, mut provider, owner) = setup("gated");
    provider
        .request_catalog_build(&owner, 1, Generation::from_raw(1))
        .unwrap();
    provider
        .request_catalog_build(&owner, 2, Generation::from_raw(2))
        .unwrap();
    fs::write(scratch.0.join("commands/release"), "go").unwrap();
    await_catalog_retirement(&pipeline, &owner);
    let results = provider.take_catalog_results();
    assert_eq!(results.len(), 2);
    assert!(results
        .iter()
        .any(|result| matches!(result, CatalogBuildResult::Obsolete(_))));
    assert_eq!(snapshot(&provider, &owner).instance, 2);
    let revision = snapshot(&provider, &owner).revision;
    provider.invalidate_commands(&owner);
    let revoked = snapshot(&provider, &owner);
    assert!(revoked.revision > revision);
    assert_eq!(revoked.status, NativeCommandStatus::Unavailable);
    assert!(revoked.declarations.is_empty());
    provider
        .request_catalog_build(&owner, 3, Generation::from_raw(3))
        .unwrap();
    provider.invalidate_commands(&owner);
    await_catalog_retirement(&pipeline, &owner);
    assert!(matches!(complete(&mut provider), CatalogBuildResult::Obsolete(_)));
    assert_eq!(
        snapshot(&provider, &owner).status,
        NativeCommandStatus::Unavailable
    );
}

#[test]
fn repeated_target_discovery_and_invocation_reject_any_duplicate_action_id() {
    for mode in ["repeated", "repeated-unrelated-duplicate", "repeated-duplicate"] {
        let (scratch, pipeline, mut provider, owner) = setup(mode);
        provider
            .request_catalog_build(&owner, 1, Generation::from_raw(1))
            .unwrap();
        assert_catalog(complete(&mut provider));
        let commands = snapshot(&provider, &owner);
        if mode == "repeated" {
            assert_eq!(commands.status, NativeCommandStatus::Ready);
            assert_eq!(commands.declarations.len(), 128);
            for (index, declaration) in commands.declarations.iter().enumerate() {
                assert_eq!(declaration.reference.provider_id, owner);
                assert_eq!(declaration.reference.command_id, format!("repeat-{index}"));
                let ExecutableTarget::Item { item_id, action_id } = &declaration.executable_target else {
                    panic!("repeated item target acquired host authority");
                };
                assert_eq!(item_id.0, "installed");
                assert_eq!(action_id.as_ref().unwrap().0, "alternate");
            }
        } else {
            assert!(
                matches!(commands.status, NativeCommandStatus::Invalid(_)),
                "{mode}"
            );
            assert!(commands.declarations.is_empty());
        }
        let declaration = commands.declarations.first().cloned().unwrap_or_else(|| {
            crikey_core::command::CommandDeclaration {
                reference: crikey_core::command::CommandRef {
                    provider_id: owner.clone(),
                    command_id: "repeat-0".into(),
                },
                label: "Repeated 0".into(),
                executable_target: ExecutableTarget::Item {
                    item_id: crikey_core::ItemId("installed".into()),
                    action_id: Some(crikey_core::ActionId("alternate".into())),
                },
                availability: CommandAvailability::Available,
                argument_policy: crikey_core::ArgumentPolicy::Forbidden,
            }
        });
        let source = crikey_app::DeclaredCommandSource {
            instance: commands.instance,
            generation: commands.generation,
            producer: commands.producer,
            publication: commands.revision,
        };
        let driver = crikey_app::NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
        let endpoint = driver.action_executor();
        let submission = endpoint.submit_declared_command_with_receipt(&declaration, &source, None);
        if mode == "repeated" {
            let receipt = submission.expect("a discovered valid target must pass canonical admission");
            let completion = wait_for(|| endpoint.poll_plugin_actions().into_iter().next());
            assert_eq!(completion.request_id, receipt.request_id);
            assert_eq!(completion.outcome.unwrap(), crikey_app::ActionEffect::Completed);
            assert_eq!(
                fs::read_to_string(scratch.0.join("commands/executed")).unwrap(),
                "installed:alternate"
            );
        } else {
            assert!(
                submission.is_err(),
                "invalid discovery cannot be revived by a forged declaration: {mode}"
            );
            assert!(endpoint.poll_plugin_actions().is_empty());
            assert!(!scratch.0.join("commands/executed").exists());
        }
    }
}

#[test]
fn invalidation_before_first_catalog_completion_publishes_explicit_revocation() {
    let (scratch, pipeline, mut provider, owner) = setup("gated");
    provider
        .request_catalog_build(&owner, 1, Generation::from_raw(1))
        .unwrap();
    provider.invalidate_commands(&owner);
    let revoked = snapshot(&provider, &owner);
    assert_eq!(revoked.status, NativeCommandStatus::Unavailable);
    assert!(revoked.declarations.is_empty());
    fs::write(scratch.0.join("commands/release"), "go").unwrap();
    await_catalog_retirement(&pipeline, &owner);
    assert!(matches!(complete(&mut provider), CatalogBuildResult::Obsolete(_)));
    assert!(Arc::ptr_eq(&revoked, &snapshot(&provider, &owner)));
}

#[test]
fn omitted_action_validates_actual_primary_without_rewriting_the_declaration() {
    for mode in [
        "primary-host",
        "primary-category",
        "primary-duplicate",
        "primary-plugin-host-alternate",
        "primary-actionless",
    ] {
        let (_scratch, _pipeline, mut provider, owner) = setup(mode);
        provider
            .request_catalog_build(&owner, 1, Generation::from_raw(1))
            .unwrap();
        assert_catalog(complete(&mut provider));
        let commands = snapshot(&provider, &owner);
        if matches!(mode, "primary-host" | "primary-category" | "primary-duplicate") {
            assert!(
                matches!(commands.status, NativeCommandStatus::Invalid(_)),
                "{mode}"
            );
            assert!(commands.declarations.is_empty());
        } else {
            assert_eq!(commands.status, NativeCommandStatus::Ready, "{mode}");
            assert_eq!(commands.declarations.len(), 1);
            let ExecutableTarget::Item { item_id, action_id } = &commands.declarations[0].executable_target
            else {
                panic!("primary dispatch acquired host target authority");
            };
            assert_eq!(item_id.0, "installed");
            assert!(action_id.is_none(), "omitted action must remain omitted");
        }
    }
}

#[test]
fn canonical_source_revoked_during_real_lazy_startup_never_executes() {
    let (scratch, pipeline, mut provider, owner) = setup("action");
    provider
        .request_catalog_build(&owner, 7, Generation::from_raw(1))
        .unwrap();
    assert_catalog(complete(&mut provider));
    let commands = snapshot(&provider, &owner);
    let source = crikey_app::DeclaredCommandSource {
        instance: commands.instance,
        generation: commands.generation,
        producer: commands.producer,
        publication: commands.revision,
    };
    let package = scratch.0.join("commands");
    fs::write(package.join("startup-block"), "").unwrap();
    let driver = crikey_app::NativeDriver::spawn(provider, pipeline, Box::new(|_| {}));
    let actions = driver.action_executor();
    let receipt = actions
        .submit_declared_command_with_receipt(&commands.declarations[0], &source, None)
        .unwrap();
    wait_for(|| package.join("startup-entered").exists().then_some(()));
    driver.invalidate_commands(&owner);
    fs::write(package.join("startup-release"), "").unwrap();
    let completion = wait_for(|| actions.poll_plugin_actions().into_iter().next());
    assert_eq!(completion.request_id, receipt.request_id);
    let refusal = completion
        .outcome
        .expect_err("revocation during startup must fence execute");
    assert!(
        refusal.to_string().contains("source is no longer current"),
        "{refusal}"
    );
    assert!(!package.join("executed").exists());
    assert!(actions.poll_plugin_actions().is_empty());
}
