//! Real SDK standard-view traversal; passwords only enter typed callbacks.
use std::sync::Arc;

use crikey_core::result_presentation::{ExecutableSchemaBinding, ItemPresentation, ResultPresentation};
use crikey_core::structured_arguments::*;
use crikey_core::structured_view::*;
use crikey_core::{ActionId, CoreError, ExecutionPolicy, ItemId, Result};
use crikey_plugin_sdk::*;

pub(crate) struct Worker;
const ITEM: &str = "shared-item";
const ACTION: &str = "open";

fn invalid() -> CoreError {
    CoreError::Invalid("unsupported fixture executable".into())
}
fn target(context: &dyn PluginContext) -> OwnedItem {
    OwnedItem {
        owner: context.plugin_id().clone(),
        item: ItemId(ITEM.into()),
    }
}
fn schema(route: bool) -> ArgumentSchema {
    let text = ArgumentField {
        id: FieldId(if route { "username" } else { "root-only" }.into()),
        label: if route { "Username" } else { "Root value" }.into(),
        required: true,
        placeholder: None,
        validation_message: None,
        kind: FieldKind::Text {
            min_bytes: 1,
            max_bytes: 128,
            default: None,
            multiline: false,
        },
    };
    let mut fields = vec![text];
    if route {
        fields.push(ArgumentField {
            id: FieldId("password".into()),
            label: "Password".into(),
            required: true,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Password {
                min_bytes: 1,
                max_bytes: 128,
            },
        });
        fields.push(ArgumentField {
            id: FieldId("destination".into()),
            label: "After submission".into(),
            required: true,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Dropdown {
                options: ["complete", "navigate"]
                    .into_iter()
                    .map(|id| DropdownOption {
                        id: OptionId(id.into()),
                        label: id.into(),
                    })
                    .collect(),
                default: Some(OptionId("complete".into())),
            },
        });
    }
    ArgumentSchema { fields }
}
fn definition(context: &dyn PluginContext, route: &str, refreshed: bool) -> RouteItemDefinition {
    let explicit = route != "grid";
    let mut builder = ItemBuilder::new(
        ITEM,
        format!("{route} {}", if refreshed { "refreshed" } else { "opened" }),
    )
    .target(format!("fixture://{route}"));
    if explicit {
        builder = builder.action(ActionBuilder::new(ACTION, "Continue").build());
    }
    let mut item = builder.build();
    item.plugin_id = context.plugin_id().clone();
    RouteItemDefinition {
        item,
        actionless_policy: (!explicit).then_some(ExecutionPolicy::Plugin),
        primary_action: explicit.then(|| ActionId(ACTION.into())),
        action_sections: vec![ActionSection {
            id: "main".into(),
            label: "Actions".into(),
            entries: vec![ActionMenuEntry::Action(ExecutablePresentation {
                action: explicit.then(|| ActionId(ACTION.into())),
                schema: (route == "form").then(|| Arc::new(schema(true))),
                shortcut: None,
                destructive: false,
            })],
        }],
    }
}

pub(crate) fn oversized_shared_images(owner: &crikey_core::PluginId) -> ViewDescriptor {
    let image = Arc::new(crikey_core::preview::PreviewImage {
        width: 512,
        height: 512,
        rgba: vec![0xff; 512 * 512 * 4],
    });
    let mut definitions = Vec::new();
    let mut items = Vec::new();
    for index in 0..9 {
        let mut item = ItemBuilder::new(format!("image-{index}"), format!("Image {index}"))
            .target(format!("fixture://grid/{index}"))
            .build();
        item.plugin_id = owner.clone();
        items.push(ItemPresentation {
            target: OwnedItem {
                owner: owner.clone(),
                item: item.stable_id.clone(),
            },
            section: None,
            scopes: vec![],
            image: Some(Arc::clone(&image)),
        });
        definitions.push(RouteItemDefinition {
            item,
            actionless_policy: Some(ExecutionPolicy::Plugin),
            primary_action: None,
            action_sections: vec![ActionSection {
                id: "main".into(),
                label: "Actions".into(),
                entries: vec![ActionMenuEntry::Action(ExecutablePresentation {
                    action: None,
                    schema: None,
                    shortcut: None,
                    destructive: false,
                })],
            }],
        });
    }
    ViewDescriptor {
        title: "Shared images exceeding the wire budget".into(),
        definitions: definitions.into(),
        view: StandardView::Grid(GridView {
            collection: CollectionView {
                initial_selection: Some(items[0].target.clone()),
                items: items.into(),
                sections: vec![],
                scopes: vec![],
                initial_scope: None,
            },
            columns: 3,
        }),
    }
}
impl Plugin for Worker {
    fn supports_structured_views(&self) -> bool {
        true
    }
    fn supports_typed_arguments(&self) -> bool {
        true
    }
    fn supports_result_presentation(&self) -> bool {
        true
    }
    fn supports_commands(&self) -> bool {
        true
    }
    fn commands(&mut self, _: &dyn PluginContext) -> Result<PluginCommands> {
        Ok(PluginCommands::Declarations(vec![CommandBuilder::new(
            "standard-views",
            "Standard views",
            "launch",
        )
        .action(ACTION)
        .build()?]))
    }
    fn start(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn stop(&mut self, _: &dyn PluginContext) -> Result<()> {
        Ok(())
    }
    fn build_catalog(&mut self, _: &dyn PluginContext, sink: &mut dyn CatalogSink) -> Result<()> {
        sink.emit_batch(vec![
            ItemBuilder::new("launch", "Standard views")
                .action(ActionBuilder::new(ACTION, "Open grid").build())
                .build(),
            ItemBuilder::new(ITEM, "Root schema collision")
                .action(ActionBuilder::new(ACTION, "Root submit").build())
                .build(),
        ])?;
        sink.finish()
    }
    fn catalog_presentation(&mut self, context: &dyn PluginContext) -> Result<ResultPresentation> {
        Ok(ResultPresentation {
            schemas: vec![ExecutableSchemaBinding {
                executable: ExecutableKey {
                    target: target(context),
                    action: Some(ActionId(ACTION.into())),
                },
                schema: Arc::new(schema(false)),
            }],
            ..Default::default()
        })
    }
    fn suggest(&mut self, _: Query, _: &dyn PluginContext, sink: &mut dyn SuggestionSink) -> Result<()> {
        sink.finish()
    }
    fn execute(&mut self, _: ExecuteRequest, _: &dyn PluginContext) -> Result<()> {
        Err(invalid())
    }
    fn execute_outcome(&mut self, request: ExecuteRequest, _: &dyn PluginContext) -> Result<ExecuteOutcome> {
        if request.item.0 != "launch"
            || request.action.as_ref().map(|a| a.0.as_str()) != Some(ACTION)
            || request.argument.is_some()
        {
            return Err(invalid());
        }
        Ok(ExecuteOutcome::Navigate(StructuredRouteOutcome::Push(RouteId(
            "grid".into(),
        ))))
    }
    fn execute_typed(
        &mut self,
        request: TypedExecuteRequest,
        _: &dyn PluginContext,
    ) -> Result<TypedExecutionOutcome> {
        if request.item.0 != ITEM || request.action.as_ref().map(|a| a.0.as_str()) != Some(ACTION) {
            return Err(invalid());
        }
        Ok(SubmissionOutcome::Completed.into())
    }
    fn structured_view(
        &mut self,
        request: &ViewRequest,
        context: &dyn PluginContext,
    ) -> Result<ViewDescriptor> {
        let route = request.route.0.as_str();
        if !matches!(route, "grid" | "detail" | "list" | "form") {
            return Err(invalid());
        }
        if route == "grid" && request.query == "remove-selected" {
            return Ok(ViewDescriptor {
                title: format!("Standard {route} ({})", request.query),
                definitions: Vec::new().into(),
                view: StandardView::Grid(GridView {
                    collection: CollectionView {
                        items: Vec::new().into(),
                        sections: vec![],
                        scopes: vec![],
                        initial_selection: None,
                        initial_scope: None,
                    },
                    columns: 2,
                }),
            });
        }
        if route == "grid" && request.query == "oversized-shared-images" {
            return Ok(oversized_shared_images(context.plugin_id()));
        }
        // Nonsecret query enables a deterministic delayed refresh/cancellation probe.
        if request.query == "source-death" {
            std::process::exit(0);
        }
        if request.query == "delay" {
            std::fs::write("route-running", b"running").map_err(|_| invalid())?;
            for _ in 0..100 {
                if context.cancellation().is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        if request.query == "coalesce" {
            std::fs::write("route-coalesce-running", b"running").map_err(|_| invalid())?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
            loop {
                if context.cancellation().is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(invalid());
                }
                if std::path::Path::new("route-coalesce-release").exists() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let definition = definition(context, route, request.mode == ViewRequestMode::Refresh);
        let collection = || CollectionView {
            items: vec![ItemPresentation {
                target: target(context),
                section: None,
                scopes: vec![],
                image: None,
            }]
            .into(),
            sections: vec![],
            scopes: vec![],
            initial_selection: Some(target(context)),
            initial_scope: None,
        };
        let view = match route {
            "grid" => StandardView::Grid(GridView { collection: collection(), columns: 2 }),
            "list" => StandardView::List(collection()),
            "detail" => StandardView::Detail(DetailView::prepare(PreviewContent {
                text: Some(PreviewText::Markdown("# Standard detail\nContinue replaces this detail with a list. [Safe link](https://example.com/)".into())),
                ..Default::default()
            }, Some(target(context))).map_err(|_| invalid())?),
            "form" => StandardView::Form(FormView { submit: ExecutableKey { target: target(context), action: Some(ActionId(ACTION.into())) } }),
            _ => unreachable!(),
        };
        Ok(ViewDescriptor {
            title: if route == "grid" && !request.query.is_empty() {
                format!("Standard {route} ({})", request.query)
            } else {
                format!("Standard {route}")
            },
            definitions: vec![definition].into(),
            view,
        })
    }
    fn execute_view(&mut self, request: ViewExecuteRequest, _: &dyn PluginContext) -> Result<ExecuteOutcome> {
        if request.item.stable_id.0 != ITEM
            || request.item.target != format!("fixture://{}", request.view.route.0)
            || request.argument.is_some()
        {
            return Err(invalid());
        }
        let route = match (
            request.view.route.0.as_str(),
            request.action.as_ref().map(|a| a.0.as_str()),
        ) {
            ("grid", None) => StructuredRouteOutcome::Push(RouteId("detail".into())),
            ("detail", Some(ACTION)) => StructuredRouteOutcome::Replace(RouteId("list".into())),
            ("list", Some(ACTION)) => StructuredRouteOutcome::Push(RouteId("form".into())),
            _ => return Err(invalid()),
        };
        Ok(ExecuteOutcome::Navigate(route))
    }
    fn execute_view_typed(
        &mut self,
        request: ViewTypedExecuteRequest,
        _: &dyn PluginContext,
    ) -> Result<TypedExecutionOutcome> {
        if request.view.route.0 != "form"
            || request.item.stable_id.0 != ITEM
            || request.item.target != "fixture://form"
            || request.action.as_ref().map(|a| a.0.as_str()) != Some(ACTION)
        {
            return Err(invalid());
        }
        if !matches!(request.values.0.get(&FieldId("password".into())), Some(StructuredValue::Password(password)) if password.expose_plaintext() == "view-secret")
        {
            return Ok(SubmissionOutcome::InvalidFields(vec![SubmissionFieldFailure {
                field: FieldId("password".into()),
                code: SubmissionFieldCode::Rejected,
            }])
            .into());
        }
        if matches!(request.values.0.get(&FieldId("destination".into())), Some(StructuredValue::NonSecret(NonSecretValue::Dropdown(id))) if id.0 == "navigate")
        {
            return Ok(TypedExecutionOutcome::Navigate(StructuredRouteOutcome::Replace(
                RouteId("grid".into()),
            )));
        }
        Ok(SubmissionOutcome::Completed.into())
    }
}
fn main() {
    serve(&mut Worker, ServeConfig::new("conformance", "1.0.0")).unwrap();
}
