// Included in worker.rs: standard routes use this worker's existing queues,
// cancellation, deadline and supervisor lifecycle, never a second runtime.
impl NativeWorker {
    fn send_admitted<F>(&mut self, envelope: Envelope, admission: F) -> Result<bool, HostError>
    where
        F: FnOnce(&mut dyn FnMut() -> bool) -> bool,
    {
        let mut envelope = Some(envelope);
        let mut sent = false;
        let mut error = None;
        let allowed = admission(&mut || {
            if self.link.cancel_latched.load(Ordering::Acquire) || !self.poll_alive() {
                return false;
            }
            let Some(envelope) = envelope.take() else {
                return false;
            };
            match self.send_control(envelope) {
                Ok(()) => {
                    sent = true;
                    true
                }
                Err(e) => {
                    error = Some(e);
                    false
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        Ok(allowed && sent)
    }
    fn view_error(&self, code: &'static str) -> HostError {
        HostError::PluginFailed {
            plugin: self.spec.plugin.clone(),
            detail: code.into(),
        }
    }
    fn view_admission(
        &self,
        context: &ViewExecutionContext,
    ) -> Result<crikey_core::structured_view::RouteAdmission, HostError> {
        use crikey_core::structured_view::{RouteAdmission, RouteIdentity};
        RouteAdmission::worker(
            RouteIdentity {
                owner: self.spec.plugin.clone(),
                route: context.route.clone(),
                incarnation: std::num::NonZeroU64::new(self.incarnation).ok_or(HostError::Closed)?,
            },
            context.surface(),
        )
        .map_err(|_| self.view_error("structured_view_invalid"))
    }
    pub fn structured_view(
        &mut self,
        request: &crikey_core::structured_view::ViewRequest,
    ) -> Result<ViewReply, HostError> {
        self.structured_view_with_admission(request, |send| send())
    }
    pub fn structured_view_with_admission<F>(
        &mut self,
        request: &crikey_core::structured_view::ViewRequest,
        admission: F,
    ) -> Result<ViewReply, HostError>
    where
        F: FnOnce(&mut dyn FnMut() -> bool) -> bool,
    {
        use crikey_core::structured_view::{ViewExecutionContext, ViewRequestMode};
        self.ensure_alive()?;
        if !self.handshake.structured_view {
            return Err(self.view_error("structured_view_unsupported"));
        }
        request
            .validate()
            .map_err(|_| self.view_error("structured_view_invalid"))?;
        let owner = &self.spec.plugin;
        if request.state.as_ref().is_some_and(|s| {
            s.selected.as_ref().is_some_and(|t| &t.owner != owner)
                || s.input.as_ref().is_some_and(|e| &e.target.owner != owner)
        }) || request.scroll.as_ref().is_some_and(|s| &s.item.owner != owner)
        {
            return Err(self.view_error("structured_view_wrong_owner"));
        }
        if self.link.cancel_latched.load(Ordering::Acquire) {
            return Err(self.view_error("structured_view_cancelled"));
        }
        if request.mode == ViewRequestMode::Refresh {
            let old = self
                .view
                .as_ref()
                .ok_or_else(|| self.view_error("structured_view_unavailable"))?;
            if old.context.route != request.route
                || old.context.session != request.session
                || Some(old.context.revision) != request.previous_revision
            {
                return Err(self.view_error("structured_view_stale"));
            }
        }
        // Refresh publishes only a complete replacement. A fresh Open retires
        // the previously covered route; the provider owns admission lifetime.
        if request.mode == ViewRequestMode::Open {
            self.view = None;
        }
        let payload = crikey_native_protocol::structured_view::to_request(request)
            .map_err(|_| self.view_error("structured_view_invalid"))?;
        let request_id = self.begin_request(0);
        let revision = std::num::NonZeroU64::new(request_id).ok_or(HostError::Closed)?;
        self.link.view_request.store(request_id, Ordering::Release);
        let result = (|| {
            let admitted = self.send_admitted(
                Envelope {
                    connection_id: self.link.connection_id,
                    request_id,
                    generation: 0,
                    deadline_ms: self.options.call_timeout_ms,
                    payload: Some(Payload::StructuredViewRequest(payload)),
                    unknown: Default::default(),
                },
                admission,
            )?;
            if !admitted {
                return Err(self.view_error("structured_view_cancelled"));
            }
            let end = deadline(self.options.call_timeout_ms);
            loop {
                let envelope = match self.next_event(end, request_id, 0)? {
                    ReaderEvent::Envelope(v) => v,
                    ReaderEvent::Failure(f) => return Err(self.transport_error(f)),
                };
                match envelope.payload {
                    Some(Payload::StructuredViewResponse(response)) => {
                        if response.status == 1 {
                            self.replenish_credit()?;
                        }
                        if response.route_id != request.route.0
                            || response.logical_view != request.session.logical_view.get()
                            || response.surface != request.session.surface.get()
                        {
                            return Err(self.protocol_failure("structured view response identity mismatch"));
                        }
                        if self.link.cancel_latched.load(Ordering::Acquire) {
                            return Err(self.view_error("structured_view_cancelled"));
                        }
                        if response.status != 1 {
                            if response.descriptor.is_some() || !matches!(response.status, 2..=5) {
                                return Err(self.protocol_failure("invalid structured view refusal"));
                            }
                            return Err(self.view_error(match response.status {
                                2 => "structured_view_unsupported",
                                3 => "structured_view_cancelled",
                                5 => "structured_view_timeout",
                                _ => "structured_view_failed",
                            }));
                        }
                        let context = ViewExecutionContext {
                            route: request.route.clone(),
                            session: request.session,
                            revision,
                        };
                        let admission = self.view_admission(&context)?;
                        let descriptor = crikey_native_protocol::structured_view::from_descriptor(
                            *response
                                .descriptor
                                .ok_or_else(|| self.protocol_failure("missing structured view descriptor"))?,
                            &admission,
                        )
                        .map_err(|_| self.protocol_failure("invalid structured view descriptor"))?;
                        let reply = ViewReply {
                            context,
                            descriptor: Arc::new(descriptor),
                        };
                        reply
                            .validate(request, revision, &admission)
                            .map_err(|_| self.view_error("structured_view_invalid_state"))?;
                        self.view = Some(reply.clone());
                        self.mark_call_succeeded();
                        return Ok(reply);
                    }
                    Some(Payload::Log(_)) => {}
                    Some(Payload::Error(error)) => return Err(self.error_payload(error, request_id)),
                    _ => return Err(self.protocol_failure("unexpected structured view response")),
                }
            }
        })();
        self.link.view_request.store(0, Ordering::Release);
        result
    }
    fn view_executable(
        &self,
        context: &ViewExecutionContext,
        key: &ExecutableKey,
    ) -> Option<(
        &crikey_core::structured_view::RouteItemDefinition,
        &crikey_core::structured_view::ExecutablePresentation,
    )> {
        if !self.handshake.structured_view || self.link.cancel_latched.load(Ordering::Acquire) {
            return None;
        }
        let reply = self.view.as_ref()?;
        if &reply.context != context {
            return None;
        }
        let definition = reply.descriptor.definition(&key.target)?;
        let (policy, presentation) = reply.descriptor.executable(key)?;
        if policy != crikey_core::ExecutionPolicy::Plugin {
            return None;
        }
        if let crikey_core::structured_view::StandardView::Form(form) = &reply.descriptor.view {
            if &form.submit != key {
                return None;
            }
        }
        Some((definition, presentation))
    }
    fn view_schema(
        &self,
        context: &ViewExecutionContext,
        key: &ExecutableKey,
    ) -> Option<Arc<crikey_core::structured_arguments::ArgumentSchema>> {
        let (_, presentation) = self.view_executable(context, key)?;
        presentation.schema.as_ref().map(Arc::clone)
    }
    pub fn execute_view(
        &mut self,
        context: &ViewExecutionContext,
        item: &ItemId,
        action: Option<&ActionId>,
        argument: Option<&str>,
    ) -> Result<ExecuteOutcome, HostError> {
        self.execute_view_with_admission(context, item, action, argument, |send| send())
    }
    pub fn execute_view_with_admission<F>(
        &mut self,
        context: &ViewExecutionContext,
        item: &ItemId,
        action: Option<&ActionId>,
        argument: Option<&str>,
        admission: F,
    ) -> Result<ExecuteOutcome, HostError>
    where
        F: FnOnce(&mut dyn FnMut() -> bool) -> bool,
    {
        self.ensure_alive()?;
        let key = ExecutableKey {
            target: OwnedItem {
                owner: self.spec.plugin.clone(),
                item: item.clone(),
            },
            action: action.cloned(),
        };
        let Some((definition, presentation)) = self.view_executable(context, &key) else {
            return Ok(ExecuteOutcome::Unsupported);
        };
        if presentation.schema.is_some()
            || (argument.is_some_and(|s| !s.is_empty())
                && definition.item.argument_policy == crikey_core::ArgumentPolicy::Forbidden)
            || (argument.is_none_or(str::is_empty)
                && definition.item.argument_policy == crikey_core::ArgumentPolicy::Required)
        {
            return Ok(ExecuteOutcome::Unsupported);
        }
        if argument.is_some_and(|s| s.len() > 8192) {
            return Ok(ExecuteOutcome::Unsupported);
        }
        self.execute_inner(Some(context), item, action, argument, admission)
    }
    pub fn prepare_view_typed_execution(
        &mut self,
        context: &ViewExecutionContext,
        item: &ItemId,
        action: Option<&ActionId>,
        expected_fingerprint: [u8; 32],
    ) -> Result<TypedPreparation, HostError> {
        self.ensure_alive()?;
        if !self.handshake.structured_view || !self.handshake.typed_arguments {
            return Ok(TypedPreparation::Refused(SubmissionFailureCode::Unsupported));
        }
        let executable = ExecutableKey {
            target: OwnedItem {
                owner: self.spec.plugin.clone(),
                item: item.clone(),
            },
            action: action.cloned(),
        };
        let Some(schema) = self.view_schema(context, &executable) else {
            return Ok(TypedPreparation::Refused(SubmissionFailureCode::Unavailable));
        };
        if schema.fingerprint().ok() != Some(expected_fingerprint) {
            return Ok(TypedPreparation::Refused(SubmissionFailureCode::SchemaMismatch));
        }
        Ok(TypedPreparation::Ready(ConfirmedTypedExecution {
            incarnation: self.incarnation,
            executable,
            fingerprint: expected_fingerprint,
            view: Some(context.clone()),
        }))
    }
    /// Revoke presentation/execution immediately; this does not release the
    /// foreground callback's physical slot or replace its producer.
    pub fn retire_view(&mut self) {
        self.view = None;
    }
}
