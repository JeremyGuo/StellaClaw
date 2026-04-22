use super::*;
use crate::config::ModelType;

fn provider_supports_native_audio_input(model: &ModelConfig) -> bool {
    !matches!(model.upstream_api_kind(), UpstreamApiKind::ClaudeMessages)
}

fn supports_native_audio_input(model: &ModelConfig) -> bool {
    model.has_capability(ModelCapability::AudioIn) && provider_supports_native_audio_input(model)
}

impl AgentRuntimeView {
    fn build_upstream_config(
        &self,
        model_key: &str,
        model: &ModelConfig,
        timeout_seconds: f64,
        prompt_cache_key: Option<String>,
        prompt_cache_retention: Option<String>,
        reasoning: Option<ReasoningConfig>,
        native_web_search: Option<NativeWebSearchConfig>,
        external_web_search: Option<ExternalWebSearchConfig>,
        native_image_input: bool,
        native_pdf_input: bool,
        native_audio_input: bool,
        native_image_generation: bool,
    ) -> Result<UpstreamConfig> {
        let token_estimation = model
            .token_estimation
            .clone()
            .map(|config| self.apply_global_token_estimation_cache(model_key, config));
        Ok(UpstreamConfig {
            base_url: model.api_endpoint.clone(),
            model: model.model.clone(),
            api_kind: model.upstream_api_kind(),
            auth_kind: model.upstream_auth_kind(),
            supports_vision_input: model.supports_image_input(),
            supports_pdf_input: model.has_capability(ModelCapability::Pdf),
            supports_audio_input: supports_native_audio_input(model),
            api_key: model.api_key.clone(),
            api_key_env: model.api_key_env.clone(),
            chat_completions_path: model.chat_completions_path.clone(),
            codex_home: model.resolved_codex_home(),
            codex_auth: self.resolved_codex_auth(model)?,
            auth_credentials_store_mode: model.auth_credentials_store_mode,
            timeout_seconds,
            retry_mode: model.retry_mode.clone(),
            context_window_tokens: model.context_window_tokens,
            cache_control: automatic_anthropic_cache_control(model),
            prompt_cache_retention,
            prompt_cache_key,
            reasoning,
            headers: model.headers.clone(),
            native_web_search,
            external_web_search,
            native_image_input,
            native_pdf_input,
            native_audio_input,
            native_image_generation,
            token_estimation,
        })
    }

    fn synthesize_external_web_search_config(
        &self,
        model_key: &str,
        model: &ModelConfig,
    ) -> Option<ExternalWebSearchConfig> {
        if model.model_type == ModelType::BraveSearch {
            return Some(ExternalWebSearchConfig {
                base_url: model.api_endpoint.clone(),
                model: model.model.clone(),
                supports_vision_input: false,
                api_key: model.api_key.clone(),
                api_key_env: model.api_key_env.clone(),
                chat_completions_path: model.chat_completions_path.clone(),
                timeout_seconds: model.timeout_seconds,
                headers: model.headers.clone(),
            });
        }
        if model.upstream_api_kind() != UpstreamApiKind::ChatCompletions {
            warn!(
                log_stream = "server",
                kind = "tooling_web_search_unsupported_upstream",
                model_key,
                model_type = ?model.model_type,
                chat_completions_path = %model.chat_completions_path,
                "tooling.web_search fallback currently requires a chat-completions-compatible upstream"
            );
            return None;
        }
        Some(ExternalWebSearchConfig {
            base_url: model.api_endpoint.clone(),
            model: model.model.clone(),
            supports_vision_input: model.supports_image_input(),
            api_key: model.api_key.clone(),
            api_key_env: model.api_key_env.clone(),
            chat_completions_path: model.chat_completions_path.clone(),
            timeout_seconds: model.timeout_seconds,
            headers: model.headers.clone(),
        })
    }

    fn resolve_image_tool_upstream(
        &self,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> Result<(bool, Option<UpstreamConfig>)> {
        let configured_target = self.tooling_target(ToolingFamily::Image);
        let image_model_key = match configured_target {
            Some(target) if target.prefer_self && model.supports_image_input() => {
                return Ok((true, None));
            }
            Some(target) => Some(target.alias.as_str()),
            None => match model.image_tool_model.as_deref() {
                None => return Ok((false, None)),
                Some("self") if model.supports_image_input() => return Ok((true, None)),
                Some("self") => return Ok((false, None)),
                Some(other_model_key) => Some(other_model_key),
            },
        };
        let Some(image_model_key) = image_model_key else {
            return Ok((false, None));
        };
        let Some(image_model) = self.models.get(image_model_key) else {
            warn!(
                log_stream = "server",
                kind = "tooling_image_model_missing",
                active_model_key,
                image_model_key,
                "configured image tooling model is missing; falling back to current upstream"
            );
            return Ok((false, None));
        };
        if !image_model.supports_image_input() {
            warn!(
                log_stream = "server",
                kind = "tooling_image_model_without_capability",
                active_model_key,
                image_model_key,
                "configured image tooling model does not advertise image input support"
            );
        }
        self.build_upstream_config(
            image_model_key,
            image_model,
            image_model.timeout_seconds,
            None,
            default_prompt_cache_retention(image_model.cache_ttl.as_deref(), image_model),
            image_model.reasoning.clone(),
            image_model.native_web_search.clone(),
            None,
            false,
            false,
            false,
            false,
        )
        .map(|upstream| (false, Some(upstream)))
    }

    fn resolve_named_tool_upstream(
        &self,
        family: ToolingFamily,
        active_model_key: &str,
    ) -> Result<Option<UpstreamConfig>> {
        let Some(target) = self.tooling_target(family) else {
            return Ok(None);
        };
        let Some(tool_model) = self.models.get(&target.alias) else {
            warn!(
                log_stream = "server",
                kind = "tooling_model_missing",
                family = family.field_name(),
                active_model_key,
                target = %target.alias,
                "configured tooling model is missing"
            );
            return Ok(None);
        };
        let required = family.required_capability();
        let supports_required = match family {
            ToolingFamily::Image => tool_model.supports_image_input(),
            capability => tool_model.has_capability(capability.required_capability()),
        };
        if !supports_required {
            warn!(
                log_stream = "server",
                kind = "tooling_model_missing_capability",
                family = family.field_name(),
                active_model_key,
                target = %target.alias,
                required_capability = ?required,
                "configured tooling model does not advertise the required capability"
            );
        }
        self.build_upstream_config(
            &target.alias,
            tool_model,
            tool_model.timeout_seconds,
            None,
            default_prompt_cache_retention(tool_model.cache_ttl.as_deref(), tool_model),
            tool_model.reasoning.clone(),
            tool_model.native_web_search.clone(),
            None,
            false,
            false,
            false,
            false,
        )
        .map(Some)
    }

    fn resolve_native_or_tool_upstream(
        &self,
        family: ToolingFamily,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> (bool, Option<UpstreamConfig>) {
        let Some(target) = self.tooling_target(family) else {
            return (false, None);
        };
        let self_supported = match family {
            ToolingFamily::Image => model.supports_image_input(),
            ToolingFamily::Pdf => model.has_capability(ModelCapability::Pdf),
            ToolingFamily::AudioInput => supports_native_audio_input(model),
            _ => false,
        };
        if target.prefer_self && self_supported {
            return (true, None);
        }
        match self.resolve_named_tool_upstream(family, active_model_key) {
            Ok(upstream) => (false, upstream),
            Err(error) => {
                warn!(
                    log_stream = "server",
                    kind = "tooling_model_resolve_failed",
                    family = family.field_name(),
                    active_model_key,
                    target = %target.alias,
                    error = %error,
                    "failed to resolve external tooling model"
                );
                (false, None)
            }
        }
    }

    fn resolve_native_image_generation(
        &self,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> (bool, Option<UpstreamConfig>) {
        let target = self.tooling_target(ToolingFamily::ImageGen);
        if matches!(
            target,
            Some(target) if target.prefer_self && model.has_capability(ModelCapability::ImageOut)
        ) && model.upstream_api_kind() != UpstreamApiKind::Responses
        {
            warn!(
                log_stream = "server",
                kind = "tooling_image_generation_self_requires_responses",
                active_model_key,
                model_type = ?model.model_type,
                chat_completions_path = %model.chat_completions_path,
                "native provider image generation is only enabled for responses-based upstreams; falling back to the configured alias"
            );
        }
        match select_image_generation_routing(target, model) {
            ImageGenerationRouting::Native => (true, None),
            ImageGenerationRouting::Disabled => (false, None),
            ImageGenerationRouting::Tool => {
                match self.resolve_named_tool_upstream(ToolingFamily::ImageGen, active_model_key) {
                    Ok(upstream) => (false, upstream),
                    Err(error) => {
                        warn!(
                            log_stream = "server",
                            kind = "tooling_image_generation_resolve_failed",
                            active_model_key,
                            target = %target
                                .expect("tool routing requires a configured target")
                                .alias,
                            error = %error,
                            "failed to resolve external image generation tooling model"
                        );
                        (false, None)
                    }
                }
            }
        }
    }

    fn resolve_web_search_configs(
        &self,
        active_model_key: &str,
        model: &ModelConfig,
    ) -> (
        Option<NativeWebSearchConfig>,
        Option<ExternalWebSearchConfig>,
    ) {
        if let Some(target) = self.tooling_target(ToolingFamily::WebSearch) {
            if target.prefer_self && model.has_capability(ModelCapability::WebSearch) {
                if model.upstream_api_kind() == UpstreamApiKind::Responses {
                    if let Some(native) = model
                        .native_web_search
                        .clone()
                        .filter(|settings| settings.enabled)
                    {
                        return (Some(native), None);
                    }
                    warn!(
                        log_stream = "server",
                        kind = "tooling_web_search_self_without_native_payload",
                        active_model_key,
                        "tooling.web_search requested :self but the active model has no native_web_search payload; falling back to the configured alias"
                    );
                } else {
                    warn!(
                        log_stream = "server",
                        kind = "tooling_web_search_self_requires_responses",
                        active_model_key,
                        model_type = ?model.model_type,
                        chat_completions_path = %model.chat_completions_path,
                        "tooling.web_search requested :self but native provider web search is only enabled for responses-based upstreams; falling back to the configured alias"
                    );
                }
            }
            let Some(search_model) = self.models.get(&target.alias) else {
                warn!(
                    log_stream = "server",
                    kind = "tooling_web_search_model_missing",
                    active_model_key,
                    target = %target.alias,
                    "configured web search tooling model is missing"
                );
                return (None, None);
            };
            let external = self.synthesize_external_web_search_config(&target.alias, search_model);
            if external.is_none() {
                warn!(
                    log_stream = "server",
                    kind = "tooling_web_search_model_unavailable",
                    active_model_key,
                    target = %target.alias,
                    "configured web search tooling model could not be translated into an external web search upstream"
                );
            }
            return (None, external);
        }

        let native = if model.upstream_api_kind() == UpstreamApiKind::Responses {
            model
                .native_web_search
                .clone()
                .filter(|settings| settings.enabled)
        } else {
            None
        };
        let external = model.web_search_model.as_ref().and_then(|alias| {
            self.context
                .model_catalog
                .web_search
                .get(alias)
                .cloned()
                .or_else(|| {
                    self.models.get(alias).and_then(|search_model| {
                        self.synthesize_external_web_search_config(alias, search_model)
                    })
                })
        });
        (native, external)
    }

    pub(super) fn build_agent_frame_config(
        &self,
        session: &SessionSnapshot,
        workspace_root: &Path,
        kind: AgentPromptKind,
        model_key: &str,
        upstream_timeout_seconds: Option<f64>,
    ) -> Result<FrameAgentConfig> {
        let model = self.model_config(model_key)?;
        let prompt_cache_key = self.selected_chat_version_id.as_ref().map(Uuid::to_string);
        let prompt_cache_retention =
            default_prompt_cache_retention(model.cache_ttl.as_deref(), model);
        let (native_image_input, image_tool_upstream) =
            self.resolve_image_tool_upstream(model_key, model)?;
        let (native_pdf_input, pdf_tool_upstream) =
            self.resolve_native_or_tool_upstream(ToolingFamily::Pdf, model_key, model);
        let (native_audio_input, audio_tool_upstream) =
            self.resolve_native_or_tool_upstream(ToolingFamily::AudioInput, model_key, model);
        let workspace_summary = self
            .workspace_manager
            .ensure_workspace_exists(&session.workspace_id)
            .map(|workspace| workspace.summary)
            .unwrap_or_default();
        let reasoning =
            effective_reasoning_config(model, self.selected_reasoning_effort.as_deref());
        let (native_web_search, external_web_search) =
            self.resolve_web_search_configs(model_key, model);
        let (native_image_generation, image_generation_tool_upstream) =
            self.resolve_native_image_generation(model_key, model);
        let prompt_available_models = self.available_agent_models(AgentBackendKind::AgentFrame);
        let mut available_upstreams = BTreeMap::new();
        for available_model_key in &prompt_available_models {
            let Some(available_model) = self.models.get(available_model_key) else {
                continue;
            };
            let (available_native_image_input, _) =
                self.resolve_image_tool_upstream(available_model_key, available_model)?;
            let (available_native_pdf_input, _) = self.resolve_native_or_tool_upstream(
                ToolingFamily::Pdf,
                available_model_key,
                available_model,
            );
            let (available_native_audio_input, _) = self.resolve_native_or_tool_upstream(
                ToolingFamily::AudioInput,
                available_model_key,
                available_model,
            );
            let (available_native_web_search, available_external_web_search) =
                self.resolve_web_search_configs(available_model_key, available_model);
            let (available_native_image_generation, _) =
                self.resolve_native_image_generation(available_model_key, available_model);
            let available_reasoning = effective_reasoning_config(
                available_model,
                self.selected_reasoning_effort.as_deref(),
            );
            available_upstreams.insert(
                available_model_key.clone(),
                self.build_upstream_config(
                    available_model_key,
                    available_model,
                    available_model.timeout_seconds,
                    None,
                    default_prompt_cache_retention(
                        available_model.cache_ttl.as_deref(),
                        available_model,
                    ),
                    available_reasoning,
                    available_native_web_search,
                    available_external_web_search,
                    available_native_image_input,
                    available_native_pdf_input,
                    available_native_audio_input,
                    available_native_image_generation,
                )?,
            );
        }
        let remote_workpaths = self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(&session.address)
                .map(|snapshot| snapshot.settings.remote_workpaths)
                .unwrap_or_default())
        })?;
        let local_mounts = self.with_conversations(|conversations| {
            Ok(conversations
                .get_snapshot(&session.address)
                .map(|snapshot| snapshot.settings.local_mounts)
                .unwrap_or_default())
        })?;

        Ok(FrameAgentConfig {
            upstream: self.build_upstream_config(
                model_key,
                model,
                upstream_timeout_seconds
                    .unwrap_or(model.timeout_seconds)
                    .min(model.timeout_seconds),
                prompt_cache_key,
                prompt_cache_retention,
                reasoning,
                native_web_search,
                external_web_search,
                native_image_input,
                native_pdf_input,
                native_audio_input,
                native_image_generation,
            )?,
            available_upstreams,
            image_tool_upstream,
            pdf_tool_upstream,
            audio_tool_upstream,
            image_generation_tool_upstream,
            skills_dirs: if matches!(self.sandbox.mode, crate::config::SandboxMode::Bubblewrap) {
                vec![workspace_root.join(".skills")]
            } else {
                vec![self.agent_workspace.skills_dir.clone()]
            },
            skills_metadata_prompt: session
                .prompt_component_system_value(crate::session::SKILLS_METADATA_PROMPT_COMPONENT)
                .map(ToOwned::to_owned),
            system_prompt: build_agent_system_prompt_state(
                &self.agent_workspace,
                session,
                &workspace_summary,
                &remote_workpaths,
                &local_mounts,
                kind,
                model_key,
                model,
                &self.models,
                &prompt_available_models,
                &self.main_agent,
            )
            .system_prompt,
            remote_workpaths: remote_workpaths
                .iter()
                .map(|workpath| agent_frame::config::RemoteWorkpathConfig {
                    host: workpath.host.clone(),
                    path: workpath.path.clone(),
                    description: workpath.description.clone(),
                })
                .collect(),
            max_tool_roundtrips: self.main_agent.max_tool_roundtrips,
            workspace_root: workspace_root.to_path_buf(),
            runtime_state_root: self
                .agent_workspace
                .root_dir
                .join("runtime")
                .join(&session.workspace_id),
            enable_context_compression: self
                .selected_context_compaction_enabled
                .unwrap_or(self.main_agent.enable_context_compression),
            context_compaction: agent_frame::config::ContextCompactionConfig {
                trigger_ratio: self.main_agent.context_compaction.trigger_ratio,
                token_limit_override: self.main_agent.context_compaction.token_limit_override,
                recent_fidelity_target_ratio: self
                    .main_agent
                    .context_compaction
                    .recent_fidelity_target_ratio,
            },
            timeout_observation_compaction:
                agent_frame::config::TimeoutObservationCompactionConfig {
                    enabled: self.main_agent.timeout_observation_compaction.enabled,
                },
            memory_system: self.main_agent.memory_system,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{provider_supports_native_audio_input, supports_native_audio_input};
    use crate::config::{ModelCapability, ModelConfig, ModelType};
    use agent_frame::config::{AuthCredentialsStoreMode, RetryModeConfig};
    use serde_json::Map;

    fn test_model(model_type: ModelType, capabilities: Vec<ModelCapability>) -> ModelConfig {
        ModelConfig {
            model_type,
            api_endpoint: "https://example.com".to_string(),
            model: "demo".to_string(),
            backend: Default::default(),
            supports_vision_input: false,
            image_tool_model: None,
            web_search_model: None,
            api_key: None,
            api_key_env: "API_KEY".to_string(),
            chat_completions_path: "/messages".to_string(),
            codex_home: None,
            auth_credentials_store_mode: AuthCredentialsStoreMode::Auto,
            timeout_seconds: 30.0,
            retry_mode: RetryModeConfig::No,
            context_window_tokens: 200_000,
            cache_ttl: None,
            reasoning: None,
            headers: Map::new(),
            description: String::new(),
            agent_model_enabled: true,
            capabilities,
            native_web_search: None,
            token_estimation: None,
        }
    }

    #[test]
    fn claude_messages_models_do_not_claim_native_audio_input() {
        let model = test_model(
            ModelType::ClaudeCode,
            vec![ModelCapability::Chat, ModelCapability::AudioIn],
        );

        assert!(!provider_supports_native_audio_input(&model));
        assert!(!supports_native_audio_input(&model));
    }

    #[test]
    fn non_claude_models_can_keep_native_audio_input_when_capability_exists() {
        let model = test_model(
            ModelType::OpenrouterResp,
            vec![ModelCapability::Chat, ModelCapability::AudioIn],
        );

        assert!(provider_supports_native_audio_input(&model));
        assert!(supports_native_audio_input(&model));
    }
}
