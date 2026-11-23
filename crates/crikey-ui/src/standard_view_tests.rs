use super::*;
use crikey_core::result_presentation::ItemPresentation;
use crikey_core::structured_arguments::{ArgumentField, ArgumentSchema, FieldId, FieldKind};
use crikey_core::{ArgumentPolicy, Category, ExecutionPolicy, HitPolicy, Item};

fn owner() -> PluginId {
    PluginId("route.owner".into())
}
fn target(index: usize) -> OwnedItem {
    OwnedItem {
        owner: owner(),
        item: ItemId(format!("item-{index}")),
    }
}
pub(crate) fn descriptor(count: usize) -> Arc<ViewDescriptor> {
    Arc::new(ViewDescriptor {
        title: "Grid".into(),
        definitions: (0..count)
            .map(|index| RouteItemDefinition {
                item: Item {
                    stable_id: target(index).item,
                    plugin_id: owner(),
                    category: Category::Application,
                    label: format!("Item {index}"),
                    description: String::new(),
                    target: "route-local".into(),
                    search_terms: vec![],
                    icon_reference: None,
                    argument_policy: ArgumentPolicy::Forbidden,
                    hit_policy: HitPolicy::Ignored,
                    score_hint: 0,
                    metadata: Default::default(),
                    actions: vec![],
                },
                actionless_policy: Some(ExecutionPolicy::Plugin),
                primary_action: None,
                action_sections: vec![crikey_core::structured_view::ActionSection {
                    id: "actions".into(),
                    label: "Actions".into(),
                    entries: vec![ActionMenuEntry::Action(ExecutablePresentation {
                        action: None,
                        schema: None,
                        shortcut: None,
                        destructive: false,
                    })],
                }],
            })
            .collect::<Vec<_>>()
            .into(),
        view: StandardView::Grid(GridView {
            columns: 3,
            collection: CollectionView {
                items: (0..count)
                    .map(|index| ItemPresentation {
                        target: target(index),
                        section: None,
                        scopes: vec![],
                        image: None,
                    })
                    .collect::<Vec<_>>()
                    .into(),
                sections: vec![],
                scopes: vec![],
                initial_selection: (count > 0).then(|| target(0)),
                initial_scope: None,
            },
        }),
    })
}
fn open(model: &mut LauncherViewModel, route: &str, descriptor: Arc<ViewDescriptor>) -> ViewExecutionContext {
    let request = model
        .open_standard_route(owner(), 7, StructuredRouteOutcome::Push(RouteId(route.into())))
        .unwrap();
    let context = ViewExecutionContext {
        route: request.route,
        session: request.session,
        revision: NonZeroU64::new(11).unwrap(),
    };
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context: context.clone(),
            descriptor
        }
    ));
    context
}
#[test]
fn grid_spatial_selection_and_actionless_execution_keep_exact_context() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "grid", descriptor(8));
    model.apply(UiCommand::SelectNext);
    assert_eq!(model.selected, 3);
    model.apply(UiCommand::StandardHorizontal(true));
    assert_eq!(model.selected, 4);
    context.revision = NonZeroU64::new(12).unwrap();
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context: context.clone(),
            descriptor: descriptor(8)
        }
    ));
    let Some(UiEffect::ExecuteStandard {
        context: actual,
        executable,
        ..
    }) = model.apply(UiCommand::ExecuteDefault)
    else {
        panic!("route did not execute");
    };
    assert_eq!(actual, context);
    assert_eq!(executable.target, target(4));
    assert!(executable.action.is_none());
}
#[test]
fn refresh_retires_old_queued_actions_without_root_collision_lookup() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let context = open(&mut model, "grid", descriptor(2));
    let executable = ExecutableKey {
        target: target(0),
        action: None,
    };
    let mut replacement = context.clone();
    replacement.revision = NonZeroU64::new(12).unwrap();
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context: replacement,
            descriptor: descriptor(2)
        }
    ));
    assert!(model
        .apply(UiCommand::StandardActivate { context, executable })
        .is_none());
}
#[test]
fn back_reopens_same_logical_entry_on_fresh_surface_without_action_replay() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let original = open(&mut model, "first", descriptor(2));
    open(&mut model, "second", descriptor(2));
    model.apply(UiCommand::Back);
    let Some(NavigationRoute::Standard {
        owner: plugin,
        producer,
        route,
        ..
    }) = model.take_page_reopen()
    else {
        panic!("missing restored route");
    };
    let request = model
        .open_standard_route(plugin, producer, StructuredRouteOutcome::Replace(route))
        .unwrap();
    assert_eq!(request.mode, ViewRequestMode::Open);
    assert_eq!(request.session.logical_view, original.session.logical_view);
    assert_ne!(request.session.surface, original.session.surface);
    assert!(request.previous_revision.is_none());
}
#[test]
fn unavailable_route_retains_back_and_settings() {
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "grid", descriptor(2));
    model.standard_unavailable();
    assert!(model.frame().unwrap().navigation.can_go_back);
    model.apply(UiCommand::OpenSettings);
    assert!(model.is_settings_open());
    model.apply(UiCommand::CloseSettings);
    model.apply(UiCommand::Back);
    assert_eq!(model.view().unwrap().1, ViewKind::RootList);
}
#[test]
fn route_depth_uses_existing_sixteen_entry_stack() {
    let mut model = LauncherViewModel::new();
    model.activate();
    for index in 0..15 {
        open(&mut model, &format!("route-{index}"), descriptor(0));
    }
    assert!(model
        .open_standard_route(
            owner(),
            7,
            StructuredRouteOutcome::Push(RouteId("overflow".into()))
        )
        .is_none());
    assert_eq!(model.navigation_stack.len(), 15);
}
#[test]
fn form_reuses_editor_and_restores_only_matching_nonsecret_drafts() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let key = ExecutableKey {
        target: target(0),
        action: None,
    };
    let schema = Arc::new(ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("password".into()),
            label: "Password".into(),
            required: true,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Password {
                min_bytes: 1,
                max_bytes: 128,
            },
        }],
    });
    let mut form = (*descriptor(1)).clone();
    form.view = StandardView::Form(FormView { submit: key.clone() });
    let mut definitions = form.definitions.to_vec();
    let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
        unreachable!()
    };
    binding.schema = Some(schema);
    form.definitions = definitions.into();
    let context = open(&mut model, "form", Arc::new(form));
    let input = model.inline_arguments().unwrap();
    assert_eq!(input.executable, key);
    assert!(model.accepts_typed_draft(input.revision, input.draft_revision, input.view, input.generation));
    assert!(input.draft.values.is_empty());
    let retired = (input.revision, input.draft_revision, input.view, input.generation);
    model.apply(UiCommand::StandardActivate {
        context: context.clone(),
        executable: key,
    });
    assert_ne!(model.inline_arguments().unwrap().revision, retired.0);
    assert!(!model.accepts_typed_draft(retired.0, retired.1, retired.2, retired.3));
    model.apply(UiCommand::OpenSettings);
    assert!(model.inline_arguments().is_none());
    assert!(model
        .navigation_stack
        .iter()
        .filter_map(|entry| entry.state.inline_arguments.as_ref())
        .all(|input| input.draft.values.is_empty()));
    assert_eq!(context.revision.get(), 11);
}
#[test]
fn shared_descriptors_count_once_and_actual_capacity_refuses() {
    let shared = descriptor(4);
    let entry = navigation::Entry {
        route: NavigationRoute::Root,
        rows: Arc::default(),
        state: NavigationRestoration::default(),
        descriptor: Some(shared.clone()),
        admitted_descriptor: None,
        transient_bytes: 0,
        transient_pixels: 0,
        panel: None,
        render_standard: true,
    };
    let first = navigation::retained_bytes(std::iter::once(&entry));
    let duplicate = entry.clone();
    assert_eq!(
        navigation::retained_bytes([&entry, &duplicate].into_iter()),
        first
    );
    let mut oversized = (*shared).clone();
    oversized.title = String::with_capacity(navigation::MAX_NAVIGATION_BYTES);
    let huge = navigation::Entry {
        descriptor: Some(Arc::new(oversized)),
        ..entry
    };
    assert!(navigation::retained_bytes(std::iter::once(&huge)) > navigation::MAX_NAVIGATION_BYTES);
}

#[test]
fn replacement_does_not_add_a_second_stack_entry() {
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "grid", descriptor(2));
    let depth = model.navigation_stack.len();
    let request = model
        .open_standard_route(
            owner(),
            7,
            StructuredRouteOutcome::Replace(RouteId("detail".into())),
        )
        .unwrap();
    assert_eq!(model.navigation_stack.len(), depth);
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context: ViewExecutionContext {
                route: request.route,
                session: request.session,
                revision: NonZeroU64::new(12).unwrap()
            },
            descriptor: descriptor(2),
        }
    ));
}

#[test]
fn removed_selection_never_executes_the_replacement_first_item() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "grid", descriptor(2));
    model.apply(UiCommand::StandardHorizontal(true));
    context.revision = NonZeroU64::new(12).unwrap();
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context,
            descriptor: descriptor(1)
        }
    ));
    assert!(model.apply(UiCommand::ExecuteDefault).is_none());
    assert_eq!(model.frame().unwrap().selected, usize::MAX);
}

#[test]
fn grid_sections_start_new_spatial_rows_and_are_not_targets() {
    let headers = vec![SectionHeader {
        row: 2,
        owner: owner(),
        id: crikey_core::result_presentation::SectionId("next".into()),
        label: "Next".into(),
    }];
    assert_eq!(grid_neighbor(1, 7, 3, &headers, 2), 3);
    assert_eq!(grid_neighbor(3, 7, 3, &headers, -2), 1);
    assert_eq!(grid_neighbor(2, 7, 3, &headers, -1), 2);
    assert_eq!(grid_neighbor(4, 7, 3, &headers, 1), 4);
}

#[test]
fn stale_detail_links_keep_their_original_revision() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let old = open(&mut model, "grid", descriptor(1));
    let mut current = old.clone();
    current.revision = NonZeroU64::new(12).unwrap();
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context: current,
            descriptor: descriptor(1)
        }
    ));
    assert!(model
        .apply(UiCommand::StandardLink {
            context: old,
            url: "https://example.test/".into()
        })
        .is_none());
}

#[test]
fn actionless_route_is_a_real_keyboard_action_without_synthetic_id() {
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "grid", descriptor(2));
    model.apply(UiCommand::ShowActions);
    assert_eq!(
        model.action_panel.as_ref().unwrap().selected(),
        Some(&ActionIdentity::Actionless)
    );
    let Some(UiEffect::ExecuteStandard { executable, .. }) = model.apply(UiCommand::ActivateSelectedAction)
    else {
        panic!("actionless sheet did not execute");
    };
    assert!(executable.action.is_none());
}

#[test]
fn rendering_grid_uploads_validated_pixels_through_native_raster_cache() {
    let mut grid = (*descriptor(1)).clone();
    let StandardView::Grid(view) = &mut grid.view else {
        unreachable!()
    };
    Arc::make_mut(&mut view.collection.items)[0].image = Some(Arc::new(crikey_core::preview::PreviewImage {
        width: 1,
        height: 1,
        rgba: vec![20, 40, 60, 255],
    }));
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "grid", Arc::new(grid));
    let context = create_launcher_context();
    let frame = build_launcher_frame(
        &context,
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 520.0),
            )),
            ..Default::default()
        },
        &model.frame().unwrap(),
    );
    assert!(frame
        .output
        .textures_delta
        .set
        .iter()
        .any(|(_, delta)| delta.image.size() == [1, 1]));
}

#[test]
fn event_state_is_projected_from_p3_not_mutable_renderer_selection() {
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "grid", descriptor(2));
    let mut renderer = model.frame().unwrap();
    renderer.selected = 1;
    assert_eq!(model.standard_host_state().selected, Some(target(0)));
    assert_eq!(renderer.selected, 1);
}

#[test]
fn detail_reuses_prepared_safe_blocks_and_emits_qualified_link_intent() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut view = (*descriptor(1)).clone();
    let detail = DetailView::prepare(
        crikey_core::PreviewContent {
            text: Some(crikey_core::PreviewText::Markdown(
                "[Safe](https://example.test/) with **prepared** text".into(),
            )),
            ..Default::default()
        },
        Some(target(0)),
    )
    .unwrap();
    let blocks = detail.blocks().clone();
    view.view = StandardView::Detail(detail);
    let context = open(&mut model, "detail", Arc::new(view));
    let surface = model.standard_surface().unwrap();
    let StandardView::Detail(detail) = &surface.descriptor.view else {
        unreachable!()
    };
    assert!(Arc::ptr_eq(&blocks, detail.blocks()));
    let Some(UiEffect::OpenStandardLink {
        context: qualified,
        url,
        ..
    }) = model.apply(UiCommand::StandardLink {
        context: context.clone(),
        url: "https://example.test/".into(),
    })
    else {
        panic!("link intent unavailable");
    };
    assert_eq!(qualified, context);
    assert_eq!(url, "https://example.test/");
}

fn paint_editor(
    context: &egui::Context,
    editor: &mut crate::typed_input::TypedEditor,
    input: Option<&InlineArguments>,
    events: Vec<egui::Event>,
) -> Option<TypedInputSubmission> {
    let mut submitted = None;
    let mut first = true;
    let _ = context.run(
        egui::RawInput {
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 520.0),
            )),
            ..Default::default()
        },
        |context| {
            egui::CentralPanel::default().show(context, |ui| {
                if let Some(submission) = editor.draw(ui, input, &mut Vec::new(), first) {
                    submitted = Some(submission);
                }
            });
            first = false;
        },
    );
    submitted
}

#[test]
fn form_cover_back_and_undo_cannot_restore_password_buffer() {
    let mut form = (*descriptor(1)).clone();
    let key = ExecutableKey {
        target: target(0),
        action: None,
    };
    form.view = StandardView::Form(FormView { submit: key });
    let mut definitions = form.definitions.to_vec();
    let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
        unreachable!()
    };
    binding.schema = Some(Arc::new(ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("password".into()),
            label: "Password".into(),
            required: true,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Password {
                min_bytes: 1,
                max_bytes: 128,
            },
        }],
    }));
    form.definitions = definitions.into();
    let form = Arc::new(form);
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "form", form.clone());
    let context = create_launcher_context();
    let mut editor = crate::typed_input::TypedEditor::default();
    let input = model.inline_arguments().unwrap().clone();
    assert!(paint_editor(
        &context,
        &mut editor,
        Some(&input),
        vec![egui::Event::Text("private-password".into())]
    )
    .is_none());
    model.apply(UiCommand::OpenSettings);
    paint_editor(&context, &mut editor, model.inline_arguments(), vec![]);
    model.apply(UiCommand::CloseSettings);
    let Some(NavigationRoute::Standard {
        owner: plugin,
        producer,
        route,
        ..
    }) = model.take_page_reopen()
    else {
        panic!("missing form restoration");
    };
    let request = model
        .open_standard_route(plugin, producer, StructuredRouteOutcome::Replace(route))
        .unwrap();
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context: ViewExecutionContext {
                route: request.route,
                session: request.session,
                revision: NonZeroU64::new(12).unwrap(),
            },
            descriptor: form
        }
    ));
    let keys = [
        (egui::Key::Z, egui::Modifiers::COMMAND),
        (egui::Key::Enter, egui::Modifiers::NONE),
    ]
    .into_iter()
    .map(|(key, modifiers)| egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    })
    .collect();
    assert!(paint_editor(&context, &mut editor, model.inline_arguments(), keys).is_none());
}

fn text_schema() -> Arc<ArgumentSchema> {
    Arc::new(ArgumentSchema {
        fields: vec![ArgumentField {
            id: FieldId("name".into()),
            label: "Name".into(),
            required: false,
            placeholder: None,
            validation_message: None,
            kind: FieldKind::Text {
                min_bytes: 0,
                max_bytes: 128,
                multiline: false,
                default: None,
            },
        }],
    })
}

#[test]
fn every_collection_selection_path_retires_the_previous_items_editor() {
    for command in 0..3 {
        let mut view = (*descriptor(5)).clone();
        let mut definitions = view.definitions.to_vec();
        let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
            unreachable!()
        };
        binding.schema = Some(text_schema());
        view.definitions = definitions.into();
        let mut model = LauncherViewModel::new();
        model.activate();
        let context = open(&mut model, "grid", Arc::new(view));
        model.apply(UiCommand::StandardActivate {
            context: context.clone(),
            executable: ExecutableKey {
                target: target(0),
                action: None,
            },
        });
        assert!(model.inline_arguments().is_some());
        let command = match command {
            0 => UiCommand::StandardSelect {
                context,
                target: target(1),
            },
            1 => UiCommand::StandardHorizontal(true),
            _ => UiCommand::SelectNext,
        };
        model.apply(command);
        assert!(model.inline_arguments().is_none());
        assert!(model.standard_host_state().input.is_none());
        assert_ne!(model.standard_host_state().selected, Some(target(0)));
    }
}

#[test]
fn detail_and_form_reopen_without_collection_scroll_anchors() {
    for form in [false, true] {
        let mut view = (*descriptor(1)).clone();
        view.view = if form {
            let mut definitions = view.definitions.to_vec();
            let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
                unreachable!()
            };
            binding.schema = Some(text_schema());
            view.definitions = definitions.into();
            StandardView::Form(FormView {
                submit: ExecutableKey {
                    target: target(0),
                    action: None,
                },
            })
        } else {
            StandardView::Detail(
                DetailView::prepare(
                    crikey_core::PreviewContent {
                        text: Some(crikey_core::PreviewText::Plain("Detail".into())),
                        ..Default::default()
                    },
                    Some(target(0)),
                )
                .unwrap(),
            )
        };
        let mut model = LauncherViewModel::new();
        model.activate();
        open(&mut model, "targeted", Arc::new(view));
        assert!(model
            .standard_refresh_request(ViewRefreshReason::Explicit)
            .unwrap()
            .scroll
            .is_none());
        model.apply(UiCommand::OpenSettings);
        model.apply(UiCommand::CloseSettings);
        let Some(NavigationRoute::Standard {
            owner: plugin,
            producer,
            route,
            ..
        }) = model.take_page_reopen()
        else {
            panic!("missing targeted route");
        };
        let request = model
            .open_standard_route(plugin, producer, StructuredRouteOutcome::Replace(route))
            .unwrap();
        assert!(request.scroll.is_none());
    }
}

#[test]
fn superseded_revision_adoption_preserves_latest_p3_intent_without_publishing_intermediate_rows() {
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "grid", descriptor(5));
    model.apply(UiCommand::SetQuery("latest query".into()));
    model.apply(UiCommand::StandardHorizontal(true));
    let expected = model.standard_host_state();
    let rows = model.rows.clone();
    let mut intermediate = (*descriptor(1)).clone();
    intermediate.title = "Superseded title".into();
    context.revision = NonZeroU64::new(12).unwrap();
    assert!(model.adopt_standard_revision(
        &owner(),
        7,
        ViewReply {
            context: context.clone(),
            descriptor: Arc::new(intermediate)
        }
    ));
    assert!(Arc::ptr_eq(&rows, &model.rows));
    assert_eq!(model.query, "latest query");
    assert_eq!(model.standard_host_state(), expected);
    let frame = model.frame().unwrap();
    assert!(frame.navigation.standard_refresh_pending);
    assert_ne!(frame.navigation.title.as_ref(), "Superseded title");
    assert!(model.apply(UiCommand::ExecuteDefault).is_none());
    let request = model.standard_refresh_request(ViewRefreshReason::Query).unwrap();
    assert_eq!(request.previous_revision, Some(context.revision));
    assert_eq!(request.query, "latest query");
    assert_eq!(request.state, Some(expected));
    model.apply(UiCommand::OpenSettings);
    model.apply(UiCommand::CloseSettings);
    assert_ne!(
        model.frame().unwrap().navigation.title.as_ref(),
        "Superseded title"
    );
}

fn input_events(events: Vec<egui::Event>) -> egui::RawInput {
    egui::RawInput {
        events,
        screen_rect: Some(egui::Rect::from_min_size(
            egui::Pos2::ZERO,
            egui::vec2(720.0, 640.0),
        )),
        ..Default::default()
    }
}

fn press(key: egui::Key) -> egui::Event {
    egui::Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers: egui::Modifiers::NONE,
    }
}

#[test]
fn standard_action_search_and_arrows_precede_same_frame_enter() {
    for case in 0..3 {
        let mut descriptor = (*descriptor(1)).clone();
        let mut definitions = descriptor.definitions.to_vec();
        definitions[0].actionless_policy = None;
        definitions[0].item.actions = [("alpha", "Alpha"), ("beta", "Beta")]
            .into_iter()
            .map(|(id, label)| crikey_core::Action {
                action_id: ActionId(id.into()),
                label: label.into(),
                description: String::new(),
                applicable_categories: vec![],
                icon_reference: None,
                execution_policy: ExecutionPolicy::Plugin,
            })
            .collect();
        definitions[0].primary_action = Some(ActionId("alpha".into()));
        definitions[0].action_sections[0].entries = ["alpha", "beta"]
            .into_iter()
            .map(|id| {
                ActionMenuEntry::Action(ExecutablePresentation {
                    action: Some(ActionId(id.into())),
                    schema: None,
                    shortcut: None,
                    destructive: false,
                })
            })
            .collect();
        descriptor.definitions = definitions.into();
        let mut model = LauncherViewModel::new();
        model.activate();
        let context = open(&mut model, "actions", Arc::new(descriptor));
        model.apply(UiCommand::ShowActions);
        let view = model.frame().unwrap();
        let renderer = create_launcher_context();
        let _ = build_launcher_frame(&renderer, input_events(vec![]), &view);
        let events = match case {
            0 => vec![egui::Event::Text("Beta".into()), press(egui::Key::Enter)],
            1 => vec![press(egui::Key::ArrowDown), press(egui::Key::Enter)],
            _ => vec![press(egui::Key::Enter), egui::Event::Text("Beta".into())],
        };
        let frame = build_launcher_frame(&renderer, input_events(events), &view);
        let effects: Vec<_> = frame
            .commands
            .into_iter()
            .filter_map(|command| model.apply(command))
            .collect();
        assert_eq!(
            effects,
            vec![UiEffect::ExecuteStandard {
                owner: owner(),
                context,
                executable: ExecutableKey {
                    target: target(0),
                    action: Some(ActionId(if case == 2 { "alpha" } else { "beta" }.into())),
                },
            }]
        );
    }
}

#[test]
fn visible_detail_texture_survives_repaint_and_retires_on_nonimage_or_grid_transition() {
    for grid in [false, true] {
        let mut image_detail = (*descriptor(1)).clone();
        image_detail.view = StandardView::Detail(
            DetailView::prepare(
                crikey_core::PreviewContent {
                    image: Some(crikey_core::preview::PreviewImage {
                        width: 1,
                        height: 1,
                        rgba: vec![9, 27, 81, 255],
                    }),
                    ..Default::default()
                },
                None,
            )
            .unwrap(),
        );
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut context = open(&mut model, "image", Arc::new(image_detail));
        let renderer = create_launcher_context();
        let visible = model.frame().unwrap();
        let first = build_launcher_frame(&renderer, input_events(vec![]), &visible);
        let image = first
            .output
            .textures_delta
            .set
            .iter()
            .find(|(_, delta)| delta.image.size() == [1, 1])
            .map(|(id, _)| *id)
            .expect("visible detail raster");
        let repaint = build_launcher_frame(&renderer, input_events(vec![]), &visible);
        assert!(
            !repaint.output.textures_delta.free.contains(&image),
            "a visible image must keep its texture lease"
        );
        let mut replacement = (*descriptor(1)).clone();
        if !grid {
            replacement.view = StandardView::Detail(
                DetailView::prepare(
                    crikey_core::PreviewContent {
                        text: Some(crikey_core::PreviewText::Plain("No image".into())),
                        ..Default::default()
                    },
                    Some(target(0)),
                )
                .unwrap(),
            );
        }
        context.revision = NonZeroU64::new(12).unwrap();
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context,
                descriptor: Arc::new(replacement)
            }
        ));
        let replacement = model.frame().unwrap();
        assert!(!replacement.rows.is_empty());
        let retired = build_launcher_frame(&renderer, input_events(vec![]), &replacement);
        assert!(
            retired.output.textures_delta.free.contains(&image),
            "non-image content must release the old texture lease"
        );
    }
}

#[test]
fn intermediate_revision_adoption_preserves_nonsecret_drafts_until_exact_schema_reconciliation() {
    use crikey_core::structured_arguments::NonSecretValue;
    for schema_changed in [false, true] {
        let mut view = (*descriptor(2)).clone();
        let mut definitions = view.definitions.to_vec();
        let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
            unreachable!()
        };
        binding.schema = Some(text_schema());
        view.definitions = definitions.into();
        let view = Arc::new(view);
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut context = open(&mut model, "draft", view.clone());
        let executable = ExecutableKey {
            target: target(0),
            action: None,
        };
        model.apply(UiCommand::StandardActivate {
            context: context.clone(),
            executable: executable.clone(),
        });
        let revision = model.inline_arguments().unwrap().revision;
        model.apply(UiCommand::SetTypedDraft {
            revision,
            draft_revision: 1,
            draft: [(
                FieldId("name".into()),
                NonSecretValue::Text("latest draft".into()),
            )]
            .into_iter()
            .collect(),
        });
        let draft = model.inline_arguments().unwrap().draft.clone();
        model.set_standard_refresh_pending(true);
        context.revision = NonZeroU64::new(12).unwrap();
        assert!(model.adopt_standard_revision(
            &owner(),
            7,
            ViewReply {
                context: context.clone(),
                descriptor: view.clone()
            }
        ));
        assert!(Arc::ptr_eq(&draft, &model.inline_arguments().unwrap().draft));
        assert_eq!(
            model
                .standard_refresh_request(ViewRefreshReason::Explicit)
                .unwrap()
                .state
                .unwrap()
                .input,
            Some(executable)
        );
        let mut final_view = (*view).clone();
        if schema_changed {
            let mut definitions = final_view.definitions.to_vec();
            let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
                unreachable!()
            };
            let mut schema = (*text_schema()).clone();
            let FieldKind::Text { max_bytes, .. } = &mut schema.fields[0].kind else {
                unreachable!()
            };
            *max_bytes = 64;
            binding.schema = Some(Arc::new(schema));
            final_view.definitions = definitions.into();
        }
        context.revision = NonZeroU64::new(13).unwrap();
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context,
                descriptor: Arc::new(final_view)
            }
        ));
        let restored = &model.inline_arguments().unwrap().draft;
        assert_eq!(
            restored.values.contains_key(&FieldId("name".into())),
            !schema_changed
        );
        assert!(!model.frame().unwrap().navigation.standard_refresh_pending);
    }
}

#[test]
fn third_refresh_intent_keeps_b_when_intermediate_a_reply_omits_b() {
    let presented = descriptor(3);
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "three-intents", presented.clone());
    model.apply(UiCommand::SetQuery("A refresh".into()));
    let request_a = model.standard_refresh_request(ViewRefreshReason::Query).unwrap();
    assert_eq!(request_a.state.unwrap().selected, Some(target(0)));

    model.apply(UiCommand::StandardHorizontal(true));
    assert_eq!(model.standard_host_state().selected, Some(target(1)));
    context.revision = NonZeroU64::new(12).unwrap();
    assert!(model.adopt_standard_revision(
        &owner(),
        7,
        ViewReply {
            context: context.clone(),
            descriptor: descriptor(1),
        }
    ));
    let surface = model.standard_surface().unwrap();
    assert!(Arc::ptr_eq(&presented, &surface.descriptor));
    assert!(surface.admitted_descriptor.definition(&target(1)).is_none());
    let request_b = model
        .standard_refresh_request(ViewRefreshReason::Selection)
        .unwrap();
    assert_eq!(request_b.previous_revision, Some(context.revision));
    assert_eq!(request_b.state.unwrap().selected, Some(target(1)));

    // B's refresh is still in flight when the third intent arrives.
    model.apply(UiCommand::SetQuery("third query".into()));
    assert_eq!(model.standard_host_state().selected, Some(target(1)));
    assert!(model.apply(UiCommand::ExecuteDefault).is_none());
    context.revision = NonZeroU64::new(13).unwrap();
    assert!(model.adopt_standard_revision(
        &owner(),
        7,
        ViewReply {
            context: context.clone(),
            descriptor: descriptor(3),
        }
    ));
    let request_c = model.standard_refresh_request(ViewRefreshReason::Query).unwrap();
    assert_eq!(request_c.previous_revision, Some(context.revision));
    assert_eq!(request_c.query, "third query");
    assert_eq!(request_c.state.unwrap().selected, Some(target(1)));
    assert!(model.frame().unwrap().navigation.standard_refresh_pending);

    context.revision = NonZeroU64::new(14).unwrap();
    assert!(model.publish_standard_view(
        owner(),
        7,
        ViewReply {
            context,
            descriptor: descriptor(3)
        }
    ));
    let surface = model.standard_surface().unwrap();
    assert!(Arc::ptr_eq(&surface.descriptor, &surface.admitted_descriptor));
    assert_eq!(model.query, "third query");
    assert_eq!(model.standard_host_state().selected, Some(target(1)));
    assert!(!model.frame().unwrap().navigation.standard_refresh_pending);
}

#[test]
fn intermediate_descriptor_and_presentation_share_one_atomic_retention_budget() {
    let large = || {
        let mut view = (*descriptor(1)).clone();
        view.title = String::with_capacity(navigation::MAX_NAVIGATION_BYTES / 2 + 1024);
        view.title.push_str("Large");
        Arc::new(view)
    };
    let presentation = large();
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "budget", presentation.clone());
    let mut entry = model.capture_entry();
    entry.admitted_descriptor = None;
    let mut duplicate = entry.clone();
    duplicate.admitted_descriptor = Some(presentation);
    let baseline = navigation::retained_bytes(std::iter::once(&entry));
    assert!(baseline > navigation::MAX_NAVIGATION_BYTES / 2);
    assert!(baseline < navigation::MAX_NAVIGATION_BYTES);
    assert!(navigation::retained_bytes(std::iter::once(&duplicate)) < navigation::MAX_NAVIGATION_BYTES);
    model.set_standard_refresh_pending(true);
    context.revision = NonZeroU64::new(12).unwrap();
    assert!(!model.adopt_standard_revision(
        &owner(),
        7,
        ViewReply {
            context,
            descriptor: large()
        }
    ));
    assert!(model.standard_surface().is_none());
    assert!(model.frame().unwrap().navigation.unavailable.is_some());
}

#[test]
fn pending_refresh_releases_unpainted_grid_rasters() {
    let mut grid = (*descriptor(1)).clone();
    let StandardView::Grid(view) = &mut grid.view else {
        unreachable!()
    };
    Arc::make_mut(&mut view.collection.items)[0].image = Some(Arc::new(crikey_core::preview::PreviewImage {
        width: 1,
        height: 1,
        rgba: vec![12, 34, 56, 255],
    }));
    let mut model = LauncherViewModel::new();
    model.activate();
    open(&mut model, "pending-image", Arc::new(grid));
    let renderer = create_launcher_context();
    let first = build_launcher_frame(&renderer, input_events(vec![]), &model.frame().unwrap());
    let image = first
        .output
        .textures_delta
        .set
        .iter()
        .find(|(_, delta)| delta.image.size() == [1, 1])
        .map(|(id, _)| *id)
        .expect("visible grid raster");
    model.apply(UiCommand::SetQuery("refresh".into()));
    let pending = build_launcher_frame(&renderer, input_events(vec![]), &model.frame().unwrap());
    assert!(pending.output.textures_delta.free.contains(&image));
}

#[test]
fn empty_scope_refresh_has_no_out_of_scope_scroll_or_selection() {
    let mut view = (*descriptor(1)).clone();
    let StandardView::Grid(grid) = &mut view.view else {
        unreachable!()
    };
    grid.collection.scopes = ["visible", "empty"]
        .map(|id| crikey_core::result_presentation::Scope {
            id: crikey_core::result_presentation::ScopeId(id.into()),
            label: id.into(),
        })
        .into();
    Arc::make_mut(&mut grid.collection.items)[0].scopes =
        vec![crikey_core::result_presentation::ScopeId("visible".into())];
    let view = Arc::new(view);
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "scopes", view.clone());
    let empty = model
        .scope_choices
        .iter()
        .find(|scope| scope.label == "empty")
        .unwrap()
        .identity
        .clone();
    model.apply(UiCommand::SetScope(Some(empty.clone())));
    let request = model.standard_refresh_request(ViewRefreshReason::Scope).unwrap();
    assert!(request.scroll.is_none());
    let state = request.state.as_ref().unwrap();
    assert_eq!(state.scope.as_ref(), Some(&empty.id));
    assert!(state.selected.is_none());
    assert!(state.input.is_none());
    context.revision = NonZeroU64::new(12).unwrap();
    let reply = ViewReply {
        context: context.clone(),
        descriptor: view,
    };
    let admission = RouteAdmission::worker(
        RouteIdentity {
            owner: owner(),
            route: context.route.clone(),
            incarnation: NonZeroU64::new(7).unwrap(),
        },
        context.surface(),
    )
    .unwrap();
    reply.validate(&request, context.revision, &admission).unwrap();
    assert!(model.publish_standard_view(owner(), 7, reply));
    let frame = model.frame().unwrap();
    assert!(frame.rows.is_empty());
    assert!(frame.navigation.unavailable.is_none());
    assert_eq!(
        frame
            .navigation
            .selected_scope
            .as_ref()
            .map(|scope| (&scope.owner, &scope.id)),
        Some((&empty.owner, &empty.id))
    );
    assert!(model.apply(UiCommand::ExecuteDefault).is_none());
}

fn paint_restored_editor(
    context: &egui::Context,
    editor: &mut crate::typed_input::TypedEditor,
    model: &mut LauncherViewModel,
    frame: &mut ViewModel,
    events: Vec<egui::Event>,
) -> (Option<TypedInputSubmission>, egui::FullOutput) {
    if let Some(current) = model.frame() {
        *frame = current;
    }
    let mut submitted = None;
    let mut commands = Vec::new();
    let mut first = true;
    let output = context.run(input_events(events), |context| {
        egui::CentralPanel::default().show(context, |ui| {
            if let Some(value) = editor.draw(
                ui,
                frame.navigation.inline_arguments.as_ref(),
                &mut commands,
                first,
            ) {
                submitted = Some(value);
            }
        });
        first = false;
    });
    for command in commands {
        model.apply(command);
    }
    (submitted, output)
}

fn click_password(output: &egui::FullOutput) -> Vec<egui::Event> {
    let position = output
        .shapes
        .iter()
        .find_map(|shape| {
            let egui::epaint::Shape::Text(text) = &shape.shape else {
                return None;
            };
            (text.galley.text() == "Enter password")
                .then(|| text.galley.rect.translate(text.pos.to_vec2()).center())
        })
        .expect("empty password control");
    vec![
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

#[test]
fn form_settings_back_then_hide_resume_password_undo_preserves_nonsecret_draft() {
    use crikey_core::structured_arguments::{NonSecretValue, StructuredValue};
    let mut form = (*descriptor(1)).clone();
    form.view = StandardView::Form(FormView {
        submit: ExecutableKey {
            target: target(0),
            action: None,
        },
    });
    let mut fields = text_schema().fields.clone();
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
    let definitions = Arc::make_mut(&mut form.definitions);
    let ActionMenuEntry::Action(binding) = &mut definitions[0].action_sections[0].entries[0] else {
        unreachable!()
    };
    binding.schema = Some(Arc::new(ArgumentSchema { fields }));
    let form = Arc::new(form);
    let mut model = LauncherViewModel::new();
    model.set_resume_delay(std::time::Duration::from_secs(60));
    model.activate();
    open(&mut model, "form", form.clone());
    let context = create_launcher_context();
    let mut editor = crate::typed_input::TypedEditor::default();
    let mut frame = model.frame().unwrap();
    let (_, output) = paint_restored_editor(
        &context,
        &mut editor,
        &mut model,
        &mut frame,
        vec![egui::Event::Text("native-route-user".into())],
    );
    paint_restored_editor(
        &context,
        &mut editor,
        &mut model,
        &mut frame,
        click_password(&output),
    );
    paint_restored_editor(
        &context,
        &mut editor,
        &mut model,
        &mut frame,
        vec![egui::Event::Text("before-cover".into())],
    );
    for (revision, hide) in [(12, false), (13, true)] {
        let retired = model.inline_arguments().unwrap().clone();
        if hide {
            model.dismiss();
            editor.clear(); // Native hide retires the live move-only editor.
            model.activate();
        } else {
            model.apply(UiCommand::OpenSettings);
            paint_restored_editor(&context, &mut editor, &mut model, &mut frame, vec![]);
            model.apply(UiCommand::CloseSettings);
        }
        let Some(NavigationRoute::Standard {
            owner: plugin,
            producer,
            route,
            ..
        }) = model.take_page_reopen()
        else {
            panic!("missing restored form")
        };
        let request = model
            .open_standard_route(plugin, producer, StructuredRouteOutcome::Replace(route))
            .unwrap();
        let pending = model.frame().unwrap();
        assert!(pending.navigation.inline_arguments.is_none());
        assert!(!model.accepts_typed_input(retired.revision, retired.view, retired.generation));
        frame = pending;
        let undo = egui::Event::Key {
            key: egui::Key::Z,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers::COMMAND,
        };
        assert!(
            paint_restored_editor(&context, &mut editor, &mut model, &mut frame, vec![undo.clone()])
                .0
                .is_none()
        );
        // A queued retired editor update must not overwrite restoration intent.
        model.apply(UiCommand::SetTypedDraft {
            revision: retired.revision,
            draft_revision: retired.draft_revision + 1,
            draft: Default::default(),
        });
        assert_eq!(
            model
                .inline_arguments()
                .unwrap()
                .draft
                .values
                .get(&FieldId("name".into())),
            Some(&NonSecretValue::Text("native-route-user".into()))
        );
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context: ViewExecutionContext {
                    route: request.route,
                    session: request.session,
                    revision: NonZeroU64::new(revision).unwrap()
                },
                descriptor: form.clone(),
            }
        ));
        let (_, output) = paint_restored_editor(&context, &mut editor, &mut model, &mut frame, vec![]);
        paint_restored_editor(
            &context,
            &mut editor,
            &mut model,
            &mut frame,
            click_password(&output),
        );
        paint_restored_editor(&context, &mut editor, &mut model, &mut frame, vec![undo]);
        assert!(
            paint_restored_editor(
                &context,
                &mut editor,
                &mut model,
                &mut frame,
                vec![press(egui::Key::Enter)]
            )
            .0
            .is_none(),
            "Undo must not revive the retired password"
        );
        assert_eq!(
            model
                .inline_arguments()
                .unwrap()
                .draft
                .values
                .get(&FieldId("name".into())),
            Some(&NonSecretValue::Text("native-route-user".into()))
        );
        paint_restored_editor(
            &context,
            &mut editor,
            &mut model,
            &mut frame,
            vec![egui::Event::Text("replacement-password".into())],
        );
    }
    let submitted = paint_restored_editor(
        &context,
        &mut editor,
        &mut model,
        &mut frame,
        vec![press(egui::Key::Enter)],
    )
    .0
    .unwrap();
    assert!(matches!(submitted.values.0.get(&FieldId("name".into())),
        Some(StructuredValue::NonSecret(NonSecretValue::Text(name))) if name == "native-route-user"));
    assert!(matches!(submitted.values.0.get(&FieldId("password".into())),
        Some(StructuredValue::Password(password)) if password.expose_plaintext() == "replacement-password"));
}

#[test]
fn successful_filtered_reply_reconciles_removed_selection_editor_and_anchor_without_first_item_fallback() {
    for query in [false, true] {
        let mut initial = (*descriptor(2)).clone();
        for definition in Arc::make_mut(&mut initial.definitions) {
            let ActionMenuEntry::Action(binding) = &mut definition.action_sections[0].entries[0] else {
                unreachable!()
            };
            binding.schema = Some(text_schema());
        }
        let initial = Arc::new(initial);
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut context = open(&mut model, "filter", initial.clone());
        model.apply(UiCommand::StandardHorizontal(true));
        context.revision = NonZeroU64::new(12).unwrap();
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context: context.clone(),
                descriptor: initial
            }
        ));
        model.apply(UiCommand::StandardActivate {
            context: context.clone(),
            executable: ExecutableKey {
                target: target(1),
                action: None,
            },
        });
        assert!(model.inline_arguments().is_some());
        model.apply(UiCommand::SetLogicalScroll(LogicalScroll {
            row: 1,
            offset: 33.0,
        }));
        if query {
            model.apply(UiCommand::SetQuery("filtered".into()));
        } else {
            model.set_standard_refresh_pending(true);
        }
        let request = model.standard_refresh_request(ViewRefreshReason::Query).unwrap();
        assert_eq!(request.state.as_ref().unwrap().selected, Some(target(1)));
        assert_eq!(request.scroll.as_ref().unwrap().item, target(1));
        if !query {
            assert!(request.state.as_ref().unwrap().input.is_some());
        }
        context.revision = NonZeroU64::new(13).unwrap();
        let reply = ViewReply {
            context: context.clone(),
            descriptor: descriptor(1),
        };
        let admission = RouteAdmission::worker(
            RouteIdentity {
                owner: owner(),
                route: context.route.clone(),
                incarnation: NonZeroU64::new(7).unwrap(),
            },
            context.surface(),
        )
        .unwrap();
        reply.validate(&request, context.revision, &admission).unwrap();
        assert!(model.publish_standard_view(owner(), 7, reply));
        assert_eq!(model.standard_host_state().selected, None);
        assert!(model.inline_arguments().is_none());
        assert_eq!(model.navigation_scroll, LogicalScroll::default());
        assert!(model.apply(UiCommand::ExecuteDefault).is_none());
        assert_eq!(model.frame().unwrap().selected, usize::MAX);
        model.apply(UiCommand::OpenSettings);
        model.apply(UiCommand::CloseSettings);
        let Some(NavigationRoute::Standard {
            owner: plugin,
            producer,
            route,
            ..
        }) = model.take_page_reopen()
        else {
            panic!("missing filtered route")
        };
        let reopened = model
            .open_standard_route(plugin, producer, StructuredRouteOutcome::Replace(route))
            .unwrap();
        assert!(reopened.state.as_ref().unwrap().selected.is_none());
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context: ViewExecutionContext {
                    route: reopened.route,
                    session: reopened.session,
                    revision: NonZeroU64::new(14).unwrap()
                },
                descriptor: descriptor(1),
            }
        ));
        assert!(model.standard_host_state().selected.is_none());
        assert!(model.apply(UiCommand::ExecuteDefault).is_none());
        model.apply(UiCommand::StandardHorizontal(true));
        assert_eq!(model.standard_host_state().selected, Some(target(0)));
    }
}

#[test]
fn retired_explicit_scope_stays_unavailable_after_otherwise_valid_reply() {
    let mut initial = (*descriptor(1)).clone();
    let StandardView::Grid(grid) = &mut initial.view else {
        unreachable!()
    };
    grid.collection.scopes = vec![crikey_core::result_presentation::Scope {
        id: crikey_core::result_presentation::ScopeId("chosen".into()),
        label: "Chosen".into(),
    }];
    Arc::make_mut(&mut grid.collection.items)[0].scopes =
        vec![crikey_core::result_presentation::ScopeId("chosen".into())];
    let mut model = LauncherViewModel::new();
    model.activate();
    let mut context = open(&mut model, "scoped", Arc::new(initial));
    let scope = model.scope_choices[0].identity.clone();
    model.apply(UiCommand::SetScope(Some(scope.clone())));
    let request = model.standard_refresh_request(ViewRefreshReason::Scope).unwrap();
    context.revision = NonZeroU64::new(12).unwrap();
    let reply = ViewReply {
        context: context.clone(),
        descriptor: descriptor(1),
    };
    let admission = RouteAdmission::worker(
        RouteIdentity {
            owner: owner(),
            route: context.route.clone(),
            incarnation: NonZeroU64::new(7).unwrap(),
        },
        context.surface(),
    )
    .unwrap();
    reply.validate(&request, context.revision, &admission).unwrap();
    assert!(!model.publish_standard_view(owner(), 7, reply));
    let frame = model.frame().unwrap();
    assert_eq!(frame.navigation.selected_scope, Some(scope));
    assert!(frame.navigation.unavailable.is_some());
    assert!(model.apply(UiCommand::ExecuteDefault).is_none());
    model.apply(UiCommand::OpenSettings);
    assert!(model.is_settings_open());
}

fn paint_route(
    context: &egui::Context,
    frame: &ViewModel,
    time: f64,
    events: Vec<egui::Event>,
) -> NativeUiFrame {
    build_launcher_frame(
        context,
        egui::RawInput {
            time: Some(time),
            events,
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(720.0, 520.0),
            )),
            ..Default::default()
        },
        frame,
    )
}

fn visible_label(output: &egui::FullOutput, label: &str) -> Option<egui::Rect> {
    output.shapes.iter().find_map(|shape| {
        let egui::epaint::Shape::Text(text) = &shape.shape else {
            return None;
        };
        let rect = text.galley.rect.translate(text.pos.to_vec2());
        (text.galley.text() == label && shape.clip_rect.contains_rect(rect)).then_some(rect)
    })
}

fn route_click(position: egui::Pos2) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(position),
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: egui::Modifiers::NONE,
        },
        egui::Event::PointerButton {
            pos: position,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        },
    ]
}

#[test]
fn grid_double_click_activates_exact_selected_or_unselected_target_once() {
    for index in [0, 1] {
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut context = open(&mut model, "double-click", descriptor(6));
        let renderer = create_launcher_context();
        let frame = model.frame().unwrap();
        let first = paint_route(&renderer, &frame, 0.0, vec![]);
        let position = visible_label(&first.output, &format!("Item {index}"))
            .unwrap()
            .center();
        // Native window_event reports the physical press before egui recognizes release.
        assert!(model.apply(UiCommand::PointerActivity).is_none());
        let click = paint_route(&renderer, &frame, 0.1, route_click(position));
        for command in click.commands {
            let effect = model.apply(command);
            assert!(
                effect.is_none()
                    || matches!(
                        effect,
                        Some(UiEffect::RefreshStandard(ViewRefreshReason::Selection))
                    )
            );
        }
        assert_eq!(model.standard_refresh_pending, index != 0);
        let pending = model.frame().unwrap_or(frame);
        // Redraw the actual pending surface before the second physical click.
        let waiting = paint_route(&renderer, &pending, 0.15, vec![]);
        assert!(visible_label(&waiting.output, &format!("Item {index}")).is_some());
        assert!(model.apply(UiCommand::PointerActivity).is_none());
        let double = paint_route(&renderer, &pending, 0.2, route_click(position));
        assert_eq!(
            double
                .commands
                .iter()
                .filter(|command| matches!(command, UiCommand::StandardDoubleClick { .. }))
                .count(),
            1
        );
        assert!(!double
            .commands
            .iter()
            .any(|command| matches!(command, UiCommand::StandardSelect { .. })));
        let mut executions = Vec::new();
        for command in double.commands {
            if let Some(UiEffect::ExecuteStandard {
                context: actual,
                executable,
                ..
            }) = model.apply(command)
            {
                executions.push((actual, executable.target));
            }
        }
        if index != 0 {
            assert!(executions.is_empty(), "pending definitions cannot execute");
            let queued_bytes = model.standard_activation_bytes();
            assert!(queued_bytes >= executable_bytes(model.standard_deferred_activation.as_ref().unwrap()));
            assert!(model.take_standard_activation().is_none());
            context.revision = NonZeroU64::new(12).unwrap();
            assert!(model.adopt_standard_revision(
                &owner(),
                7,
                ViewReply {
                    context: context.clone(),
                    descriptor: descriptor(6),
                }
            ));
            assert!(
                model.standard_activation_bytes()
                    >= queued_bytes + standard_surface_bytes(model.standard_pointer_source.as_ref().unwrap())
            );
            assert!(model.take_standard_activation().is_none());
            context.revision = NonZeroU64::new(13).unwrap();
            assert!(model.publish_standard_view(
                owner(),
                7,
                ViewReply {
                    context: context.clone(),
                    descriptor: descriptor(6),
                }
            ));
            let Some(UiEffect::ExecuteStandard {
                context: actual,
                executable,
                ..
            }) = model.take_standard_activation()
            else {
                panic!("validated double-click did not execute");
            };
            executions.push((actual, executable.target));
        }
        assert_eq!(executions, vec![(context.clone(), target(index))]);
        assert!(model.take_standard_activation().is_none());
        assert_eq!(model.standard_host_state().selected, Some(target(index)));
        let mut stale = context;
        stale.revision = NonZeroU64::new(10).unwrap();
        assert!(model
            .apply(UiCommand::StandardDoubleClick {
                context: stale,
                executable: ExecutableKey {
                    target: target(2),
                    action: None
                },
            })
            .is_none());
        assert_eq!(model.standard_host_state().selected, Some(target(index)));
    }
}

#[test]
fn pending_grid_double_click_discards_changed_definitions_and_superseding_intent() {
    for change in 0..11 {
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut context = open(&mut model, "deferred", descriptor(6));
        let renderer = create_launcher_context();
        let frame = model.frame().unwrap();
        let first = paint_route(&renderer, &frame, 0.0, vec![]);
        let position = visible_label(&first.output, "Item 1").unwrap().center();
        model.apply(UiCommand::PointerActivity);
        for command in paint_route(&renderer, &frame, 0.1, route_click(position)).commands {
            model.apply(command);
        }
        let pending = model.frame().unwrap();
        paint_route(&renderer, &pending, 0.15, vec![]);
        model.apply(UiCommand::PointerActivity);
        for command in paint_route(&renderer, &pending, 0.2, route_click(position)).commands {
            assert!(model.apply(command).is_none());
        }
        assert!(model.standard_deferred_activation.is_some());
        let mut replacement = (*descriptor(6)).clone();
        match change {
            0 => Arc::make_mut(&mut replacement.definitions)[1].item.target = "replacement".into(),
            1 => {
                let ActionMenuEntry::Action(binding) =
                    &mut Arc::make_mut(&mut replacement.definitions)[1].action_sections[0].entries[0]
                else {
                    unreachable!()
                };
                binding.schema = Some(text_schema());
            }
            2 => replacement = (*descriptor(1)).clone(),
            3 => {
                model.apply(UiCommand::SetQuery("newer".into()));
            }
            4 => {
                model.apply(UiCommand::SelectNext);
            }
            5 => {
                model.apply(UiCommand::Back);
            }
            6 => model.dismiss(),
            7 => model.standard_unavailable(),
            8 => {
                model.apply(UiCommand::PointerActivity);
            }
            9 => {
                model.apply(UiCommand::UserActivity);
            }
            _ => {
                model.apply(UiCommand::OpenSettings);
            }
        }
        context.revision = NonZeroU64::new(12).unwrap();
        model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context,
                descriptor: Arc::new(replacement),
            },
        );
        assert!(
            model.take_standard_activation().is_none(),
            "changed source/intent must not replay: {change}"
        );
        assert!(model.standard_deferred_activation.is_none());
        assert!(model.standard_pointer_source.is_none());
    }
}

fn assert_selected_surface_visible(output: &egui::FullOutput, label: &str, clip_margin: f32) {
    let label = visible_label(output, label).expect("selected label must be inside its viewport");
    let selected = output
        .shapes
        .iter()
        .find_map(|shape| {
            let egui::epaint::Shape::Rect(rect) = &shape.shape else {
                return None;
            };
            (rect.fill == theme::palette().accent_soft && rect.rect.contains(label.center()))
                .then_some((rect.rect, shape.clip_rect))
        })
        .expect("selected row/tile background");
    let viewport = selected.1.shrink(clip_margin);
    assert!(
        viewport.contains_rect(label),
        "selected label must fit the actual viewport, not its expanded paint clip"
    );
    let visible = viewport.intersect(selected.0);
    assert!(
        visible.width() > 0.0 && visible.height() > 0.0,
        "selected row/tile must have positive area inside its scroll viewport"
    );
}

fn settle_route(
    renderer: &egui::Context,
    model: &mut LauncherViewModel,
    frame: &ViewModel,
    time: &mut f64,
    events: Vec<egui::Event>,
) -> NativeUiFrame {
    let mut output = paint_route(renderer, frame, *time, events);
    for _ in 0..24 {
        for command in output.commands {
            if let UiCommand::SetLogicalScroll(_) = command {
                model.apply(command);
            }
        }
        *time += 1.0 / 60.0;
        output = paint_route(renderer, frame, *time, vec![]);
    }
    for command in &output.commands {
        if let UiCommand::SetLogicalScroll(scroll) = command {
            model.apply(UiCommand::SetLogicalScroll(scroll.clone()));
        }
    }
    output
}

#[test]
fn long_standard_collections_follow_keyboard_after_coalesced_refresh_but_preserve_pointer_and_query_scroll() {
    for (grid, painted) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut view = (*descriptor(96)).clone();
        if !grid {
            let StandardView::Grid(original) = &view.view else {
                unreachable!()
            };
            view.view = StandardView::List(original.collection.clone());
        }
        let view = Arc::new(view);
        let mut model = LauncherViewModel::new();
        model.activate();
        let mut context = open(&mut model, "long-collection", view.clone());
        let renderer = create_launcher_context();
        let mut time = 0.0;
        let frame = model.frame().unwrap();
        if painted {
            settle_route(&renderer, &mut model, &frame, &mut time, vec![]);
        }
        let restoration = model.navigation_restore;
        for _ in 0..3 {
            model.apply(UiCommand::PageDown);
        }
        let selected = model.selected;
        assert!(selected >= 24);
        context.revision = NonZeroU64::new(12).unwrap();
        assert!(model.adopt_standard_revision(
            &owner(),
            7,
            ViewReply {
                context: context.clone(),
                descriptor: view.clone(),
            }
        ));
        context.revision = NonZeroU64::new(13).unwrap();
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context: context.clone(),
                descriptor: view.clone(),
            }
        ));
        let frame = model.frame().unwrap();
        assert!(!frame.navigation.preserve_scroll);
        assert_eq!(model.navigation_restore, restoration);
        assert_eq!(frame.navigation.restoration, 0);
        let output = settle_route(&renderer, &mut model, &frame, &mut time, vec![]);
        assert_selected_surface_visible(
            &output.output,
            &format!("Item {selected}"),
            renderer.style().visuals.clip_rect_margin,
        );
        // A real wheel gesture leaves the selected item behind, without a new selection.
        let output = settle_route(
            &renderer,
            &mut model,
            &frame,
            &mut time,
            vec![
                egui::Event::PointerMoved(egui::pos2(320.0, 200.0)),
                egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, 900.0),
                    modifiers: egui::Modifiers::NONE,
                },
            ],
        );
        assert!(visible_label(&output.output, &format!("Item {selected}")).is_none());
        let manual = model.navigation_scroll.clone();
        let anchor_label = format!("Item {}", manual.row + if grid { 3 } else { 1 });
        let manual_rect = visible_label(&output.output, &anchor_label).unwrap();
        model.apply(UiCommand::SetQuery("keep anchor".into()));
        context.revision = NonZeroU64::new(14).unwrap();
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context: context.clone(),
                descriptor: view.clone(),
            }
        ));
        let frame = model.frame().unwrap();
        assert!(frame.navigation.preserve_scroll);
        assert_eq!(frame.navigation.scroll, manual);
        let output = settle_route(&renderer, &mut model, &frame, &mut time, vec![]);
        assert!(visible_label(&output.output, &format!("Item {selected}")).is_none());
        assert_eq!(model.navigation_scroll.row, manual.row);
        assert!((model.navigation_scroll.offset - manual.offset).abs() < 0.01);
        let restored_rect = visible_label(&output.output, &anchor_label).unwrap();
        assert!(
            (restored_rect.top() - manual_rect.top()).abs() < 0.01,
            "query refresh must preserve actual painted row position"
        );
        // Back's captured scroll remains authoritative even over the selected row.
        let saved = model.navigation_restoration();
        open(&mut model, "cover", descriptor(1));
        model.apply(UiCommand::Back);
        let Some(NavigationRoute::Standard {
            owner: plugin,
            producer,
            route,
            ..
        }) = model.take_page_reopen()
        else {
            panic!("missing long collection");
        };
        let request = model
            .open_standard_route(plugin, producer, StructuredRouteOutcome::Replace(route))
            .unwrap();
        context.session = request.session;
        context.route = request.route;
        context.revision = NonZeroU64::new(15).unwrap();
        assert!(model.publish_standard_view(
            owner(),
            7,
            ViewReply {
                context,
                descriptor: view
            }
        ));
        let frame = model.frame().unwrap();
        assert!(frame.navigation.preserve_scroll);
        assert_eq!(frame.navigation.scroll, saved.scroll);
        let output = settle_route(&renderer, &mut model, &frame, &mut time, vec![]);
        assert!(visible_label(&output.output, &format!("Item {selected}")).is_none());
        assert_eq!(model.navigation_scroll.row, manual.row);
        assert!((model.navigation_scroll.offset - manual.offset).abs() < 0.01);
        let restored_rect = visible_label(&output.output, &anchor_label).unwrap();
        assert!(
            (restored_rect.top() - manual_rect.top()).abs() < 0.01,
            "Back must preserve actual painted row position"
        );
    }
}
