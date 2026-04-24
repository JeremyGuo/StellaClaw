use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use stellaclaw_core::model_config::{
    MediaInputConfig, MediaInputTransport, ModelCapability, ModelConfig, MultimodalInputConfig,
    ProviderType, RetryMode, TokenEstimatorType,
};

use crate::config::{
    AgentServerConfig, ChannelConfig, SandboxConfig, SandboxMode, SessionDefaults,
    StellaclawConfig, TelegramChannelConfig, ToolModelTarget,
};

pub fn load_and_upgrade(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    let legacy: LegacyServerConfig =
        serde_json::from_str(raw).context("failed to parse partyclaw v0.28 config")?;
    convert_legacy_config(legacy, path)
}

pub(super) fn load_compatible(raw: &str, path: &Path) -> Result<StellaclawConfig> {
    let legacy: LegacyServerConfig =
        serde_json::from_str(raw).context("failed to parse stellaclaw v0.2 compact config")?;
    convert_legacy_config(legacy, path)
}

#[derive(Debug, Deserialize)]
struct LegacyServerConfig {
    #[allow(dead_code)]
    version: String,
    #[serde(default)]
    agent_server: AgentServerConfig,
    #[serde(default)]
    models: BTreeMap<String, LegacyModelConfig>,
    #[serde(default)]
    agent: LegacyAgentConfig,
    #[serde(default)]
    available_models: Vec<String>,
    #[serde(default)]
    model_catalog: LegacyModelCatalogConfig,
    #[serde(default)]
    tooling: LegacyToolingConfig,
    #[serde(default)]
    chat_model_keys: Vec<String>,
    #[serde(default)]
    main_agent: LegacyMainAgentConfig,
    #[serde(default)]
    sandbox: LegacySandboxConfig,
    #[serde(default)]
    channels: Vec<LegacyChannelConfig>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyAgentConfig {
    #[serde(default)]
    agent_frame: LegacyAgentBackendConfig,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyAgentBackendConfig {
    #[serde(default)]
    available_models: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyModelCatalogConfig {
    #[serde(default)]
    web_search: BTreeMap<String, LegacyExternalWebSearchConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyExternalWebSearchConfig {
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    model: String,
    #[serde(default = "default_openai_api_key_env")]
    api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    chat_completions_path: String,
    #[serde(default = "default_external_timeout_seconds")]
    timeout_seconds: f64,
    #[serde(default)]
    supports_vision_input: bool,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyToolingConfig {
    #[serde(default)]
    web_search: Option<String>,
    #[serde(default)]
    image: Option<String>,
    #[serde(default)]
    image_gen: Option<String>,
    #[serde(default)]
    pdf: Option<String>,
    #[serde(default)]
    audio_input: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyMainAgentConfig {
    #[serde(default)]
    enable_context_compression: bool,
    #[serde(default)]
    context_compaction: LegacyContextCompactionConfig,
    #[serde(default)]
    #[allow(dead_code)]
    memory_system: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct LegacyContextCompactionConfig {
    #[serde(default)]
    trigger_ratio: f64,
    #[serde(default)]
    token_limit_override: Option<usize>,
    #[serde(default)]
    recent_fidelity_target_ratio: f64,
}

#[derive(Debug, Default, Deserialize)]
struct LegacySandboxConfig {
    #[serde(default)]
    mode: LegacySandboxMode,
    #[serde(default = "default_bubblewrap_binary")]
    bubblewrap_binary: String,
    #[serde(default)]
    map_docker_socket: bool,
}

#[derive(Debug, Default, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacySandboxMode {
    #[default]
    Subprocess,
    Bubblewrap,
    Disabled,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum LegacyChannelConfig {
    Telegram(LegacyTelegramChannelConfig),
    CommandLine(LegacyUnsupportedChannelConfig),
    Dingtalk(LegacyUnsupportedChannelConfig),
    DingtalkRobot(LegacyUnsupportedChannelConfig),
    Web(LegacyUnsupportedChannelConfig),
}

#[derive(Debug, Deserialize)]
struct LegacyTelegramChannelConfig {
    id: String,
    #[serde(default)]
    bot_token: Option<String>,
    #[serde(default = "default_telegram_bot_token_env")]
    bot_token_env: String,
    #[serde(default = "default_telegram_api_base_url")]
    api_base_url: String,
    #[serde(default = "default_poll_timeout_seconds")]
    poll_timeout_seconds: u64,
    #[serde(default = "default_poll_interval_ms")]
    poll_interval_ms: u64,
    #[serde(default)]
    admin_user_ids: Vec<i64>,
}

#[derive(Debug, Deserialize)]
struct LegacyUnsupportedChannelConfig {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyModelConfig {
    #[serde(rename = "type")]
    model_type: LegacyModelType,
    api_endpoint: String,
    model: String,
    #[serde(default)]
    supports_vision_input: bool,
    #[serde(default)]
    image_tool_model: Option<String>,
    #[serde(default, rename = "web_search")]
    web_search_model: Option<String>,
    #[serde(default = "default_openai_api_key_env")]
    api_key_env: String,
    #[serde(default = "default_chat_completions_path")]
    chat_completions_path: String,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: f64,
    #[serde(default)]
    retry_mode: LegacyRetryMode,
    #[serde(default = "default_context_window_tokens")]
    context_window_tokens: usize,
    #[serde(default)]
    cache_ttl: Option<String>,
    #[serde(default)]
    headers: serde_json::Map<String, serde_json::Value>,
    #[serde(default)]
    reasoning: Option<serde_json::Value>,
    #[serde(default)]
    capabilities: Vec<LegacyModelCapability>,
    #[serde(default)]
    native_web_search: Option<serde_json::Value>,
    #[serde(default)]
    token_estimation: Option<LegacyTokenEstimationConfig>,
    #[serde(default)]
    multimodal_input: Option<LegacyMultimodalInputConfig>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct LegacyMultimodalInputConfig {
    #[serde(default)]
    image: Option<LegacyMediaInputConfig>,
    #[serde(default)]
    pdf: Option<LegacyMediaInputConfig>,
    #[serde(default)]
    audio: Option<LegacyMediaInputConfig>,
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyMediaInputConfig {
    transport: MediaInputTransport,
    #[serde(default)]
    supported_media_types: Vec<String>,
    #[serde(default)]
    max_width: Option<u32>,
    #[serde(default)]
    max_height: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum LegacyModelType {
    Openrouter,
    OpenrouterResp,
    CodexSubscription,
    ClaudeCode,
    BraveSearch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyModelCapability {
    Chat,
    WebSearch,
    ImageIn,
    ImageOut,
    Pdf,
    AudioIn,
    FileIn,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum LegacyRetryMode {
    No,
    Random {
        max_retries: u32,
        retry_random_mean: f64,
    },
}

impl Default for LegacyRetryMode {
    fn default() -> Self {
        Self::No
    }
}

#[derive(Debug, Clone, Deserialize)]
struct LegacyTokenEstimationConfig {
    #[serde(default)]
    source: Option<LegacyTokenEstimationSource>,
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    revision: Option<String>,
    #[serde(default)]
    template: Option<LegacyTokenEstimationTemplateConfig>,
    #[serde(default)]
    tokenizer: Option<LegacyTokenEstimationTokenizerConfig>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum LegacyTokenEstimationSource {
    Huggingface,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum LegacyTokenEstimationTemplateConfig {
    Builtin,
    Local {
        path: PathBuf,
        #[serde(default)]
        field: Option<String>,
    },
    Huggingface {
        repo: String,
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        field: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum LegacyTokenEstimationTokenizerConfig {
    Tiktoken {
        #[serde(default)]
        encoding: Option<String>,
    },
    Local {
        path: PathBuf,
    },
    Huggingface {
        repo: String,
        #[serde(default)]
        revision: Option<String>,
        #[serde(default)]
        file: Option<String>,
    },
}

fn convert_legacy_config(legacy: LegacyServerConfig, path: &Path) -> Result<StellaclawConfig> {
    if legacy.models.is_empty() {
        return Err(anyhow!("partyclaw config must define at least one model"));
    }
    if legacy.channels.is_empty() {
        return Err(anyhow!("partyclaw config must define at least one channel"));
    }
    if legacy.sandbox.map_docker_socket {
        return Err(anyhow!(
            "partyclaw sandbox.map_docker_socket is not supported by stellaclaw"
        ));
    }
    let base_dir = path.parent().unwrap_or_else(|| Path::new("."));
    let named_models =
        convert_named_models(&legacy.models, &legacy.model_catalog.web_search, base_dir)?;
    let main_model_name = select_main_model_name(&legacy)?;
    if !named_models.contains_key(&main_model_name) {
        return Err(anyhow!(
            "main model '{}' is missing after conversion",
            main_model_name
        ));
    }
    let session_defaults = convert_session_defaults(&legacy, &named_models, &main_model_name)?;

    let channels = legacy
        .channels
        .into_iter()
        .filter_map(convert_channel)
        .collect::<Vec<_>>();

    Ok(StellaclawConfig {
        version: crate::config::LATEST_CONFIG_VERSION.to_string(),
        agent_server: legacy.agent_server,
        default_profile: None,
        models: named_models,
        session_defaults,
        sandbox: SandboxConfig {
            mode: match legacy.sandbox.mode {
                LegacySandboxMode::Subprocess | LegacySandboxMode::Disabled => {
                    SandboxMode::Subprocess
                }
                LegacySandboxMode::Bubblewrap => SandboxMode::Bubblewrap,
            },
            bubblewrap_binary: legacy.sandbox.bubblewrap_binary,
        },
        channels,
    })
}

fn convert_named_models(
    legacy_models: &BTreeMap<String, LegacyModelConfig>,
    legacy_web_search_catalog: &BTreeMap<String, LegacyExternalWebSearchConfig>,
    base_dir: &Path,
) -> Result<BTreeMap<String, ModelConfig>> {
    let mut models = BTreeMap::new();
    for (name, model) in legacy_models {
        models.insert(name.clone(), convert_model(name, model, base_dir)?);
    }
    for (name, model) in legacy_web_search_catalog {
        if models.contains_key(name) {
            continue;
        }
        models.insert(name.clone(), convert_external_web_search_model(model)?);
    }
    Ok(models)
}

fn convert_model(name: &str, legacy: &LegacyModelConfig, base_dir: &Path) -> Result<ModelConfig> {
    if !legacy.headers.is_empty() {
        return Err(anyhow!(
            "partyclaw model '{}' uses custom headers, which stellaclaw does not support yet",
            name
        ));
    }
    if legacy
        .native_web_search
        .as_ref()
        .and_then(|value| value.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
    {
        return Err(anyhow!(
            "partyclaw model '{}' enables native_web_search, which stellaclaw does not support yet",
            name
        ));
    }

    let capabilities = convert_capabilities(legacy);
    let token_estimator = convert_token_estimator(
        name,
        legacy.token_estimation.as_ref(),
        &legacy.model,
        base_dir,
    )?;

    Ok(ModelConfig {
        provider_type: match legacy.model_type {
            LegacyModelType::Openrouter => ProviderType::OpenRouterCompletion,
            LegacyModelType::OpenrouterResp => ProviderType::OpenRouterResponses,
            LegacyModelType::CodexSubscription => ProviderType::CodexSubscription,
            LegacyModelType::ClaudeCode => ProviderType::ClaudeCode,
            LegacyModelType::BraveSearch => ProviderType::BraveSearch,
        },
        model_name: legacy.model.clone(),
        url: join_endpoint(&legacy.api_endpoint, &legacy.chat_completions_path),
        api_key_env: legacy.api_key_env.clone(),
        capabilities: capabilities.clone(),
        token_max_context: legacy.context_window_tokens as u64,
        cache_timeout: parse_cache_timeout_secs(legacy.cache_ttl.as_deref()).unwrap_or(300),
        conn_timeout: legacy.timeout_seconds.ceil().max(1.0) as u64,
        retry_mode: convert_retry_mode(&legacy.retry_mode),
        reasoning: legacy.reasoning.clone(),
        token_estimator_type: token_estimator.0,
        multimodal_estimator: None,
        multimodal_input: legacy
            .multimodal_input
            .as_ref()
            .map(convert_multimodal_input)
            .or_else(|| build_multimodal_input(&capabilities)),
        token_estimator_url: token_estimator.1,
    })
}

fn convert_external_web_search_model(
    legacy: &LegacyExternalWebSearchConfig,
) -> Result<ModelConfig> {
    Ok(ModelConfig {
        provider_type: ProviderType::OpenRouterResponses,
        model_name: legacy.model.clone(),
        url: join_endpoint(&legacy.base_url, &legacy.chat_completions_path),
        api_key_env: legacy.api_key_env.clone(),
        capabilities: vec![ModelCapability::WebSearch],
        token_max_context: 128_000,
        cache_timeout: 300,
        conn_timeout: legacy.timeout_seconds.ceil().max(1.0) as u64,
        retry_mode: RetryMode::Once,
        reasoning: None,
        token_estimator_type: TokenEstimatorType::Local,
        multimodal_estimator: None,
        multimodal_input: build_multimodal_input(&if legacy.supports_vision_input {
            vec![ModelCapability::WebSearch, ModelCapability::ImageIn]
        } else {
            vec![ModelCapability::WebSearch]
        }),
        token_estimator_url: None,
    })
}

fn convert_capabilities(legacy: &LegacyModelConfig) -> Vec<ModelCapability> {
    let mut capabilities = legacy
        .capabilities
        .iter()
        .map(|capability| match capability {
            LegacyModelCapability::Chat => ModelCapability::Chat,
            LegacyModelCapability::WebSearch => ModelCapability::WebSearch,
            LegacyModelCapability::ImageIn => ModelCapability::ImageIn,
            LegacyModelCapability::ImageOut => ModelCapability::ImageOut,
            LegacyModelCapability::Pdf => ModelCapability::PdfIn,
            LegacyModelCapability::AudioIn => ModelCapability::AudioIn,
            LegacyModelCapability::FileIn => ModelCapability::FileIn,
        })
        .collect::<Vec<_>>();
    if legacy.supports_vision_input && !capabilities.contains(&ModelCapability::ImageIn) {
        capabilities.push(ModelCapability::ImageIn);
    }
    capabilities.sort_by_key(|capability| match capability {
        ModelCapability::Chat => 0,
        ModelCapability::ImageIn => 1,
        ModelCapability::ImageOut => 2,
        ModelCapability::PdfIn => 3,
        ModelCapability::AudioIn => 4,
        ModelCapability::FileIn => 5,
        ModelCapability::WebSearch => 6,
    });
    capabilities.dedup();
    capabilities
}

fn convert_token_estimator(
    name: &str,
    token_estimation: Option<&LegacyTokenEstimationConfig>,
    model_name: &str,
    base_dir: &Path,
) -> Result<(TokenEstimatorType, Option<String>)> {
    let Some(config) = token_estimation else {
        return Ok((TokenEstimatorType::Local, None));
    };

    if matches!(
        config.source,
        Some(LegacyTokenEstimationSource::Huggingface)
    ) {
        let repo = config.repo.as_deref().ok_or_else(|| {
            anyhow!(
                "model '{}' token_estimation.source=huggingface requires repo",
                name
            )
        })?;
        let revision = config.revision.as_deref().unwrap_or("main");
        return Ok((
            TokenEstimatorType::HuggingFace,
            Some(format!(
                "https://huggingface.co/{repo}/raw/{revision}/tokenizer_config.json"
            )),
        ));
    }

    if let Some(LegacyTokenEstimationTemplateConfig::Huggingface {
        repo,
        revision,
        file,
        field,
    }) = config.template.as_ref()
    {
        let template_file = file.as_deref().unwrap_or("tokenizer_config.json");
        if field.as_deref().unwrap_or("chat_template") != "chat_template" {
            return Err(anyhow!(
                "model '{}' uses unsupported huggingface token template field '{}'",
                name,
                field.as_deref().unwrap_or("chat_template")
            ));
        }
        match config.tokenizer.as_ref() {
            Some(LegacyTokenEstimationTokenizerConfig::Huggingface {
                repo: tokenizer_repo,
                revision: tokenizer_revision,
                file,
            }) => {
                if tokenizer_repo != repo {
                    return Err(anyhow!(
                        "model '{}' uses different huggingface repos for template/tokenizer",
                        name
                    ));
                }
                if file.as_deref().unwrap_or("tokenizer.json") != "tokenizer.json" {
                    return Err(anyhow!(
                        "model '{}' uses unsupported huggingface tokenizer file '{}'",
                        name,
                        file.as_deref().unwrap_or("tokenizer.json")
                    ));
                }
                if tokenizer_revision.as_deref().unwrap_or("main")
                    != revision.as_deref().unwrap_or("main")
                {
                    return Err(anyhow!(
                        "model '{}' uses different huggingface revisions for template/tokenizer",
                        name
                    ));
                }
                let _ = tokenizer_repo;
            }
            Some(LegacyTokenEstimationTokenizerConfig::Tiktoken { .. })
            | Some(LegacyTokenEstimationTokenizerConfig::Local { .. })
            | None => {
                return Err(anyhow!(
                    "model '{}' uses unsupported mixed token_estimation config",
                    name
                ))
            }
        };
        let revision = revision.as_deref().unwrap_or("main");
        if template_file == "chat_template.jinja" {
            if let Some((template_path, tokenizer_path)) =
                find_legacy_huggingface_token_estimator_assets(
                    name,
                    repo,
                    revision,
                    template_file,
                    base_dir,
                )?
            {
                return migrate_token_estimator_assets_from_paths(
                    name,
                    &template_path,
                    &tokenizer_path,
                    base_dir,
                );
            }
        } else if template_file != "tokenizer_config.json" {
            return Err(anyhow!(
                "model '{}' uses unsupported huggingface token template file '{}'",
                name,
                template_file
            ));
        }
        return Ok((
            TokenEstimatorType::HuggingFace,
            Some(format!(
                "https://huggingface.co/{repo}/raw/{revision}/tokenizer_config.json"
            )),
        ));
    }

    if let Some(LegacyTokenEstimationTokenizerConfig::Tiktoken { encoding }) =
        config.tokenizer.as_ref()
    {
        if encoding.as_deref().unwrap_or("auto") != "auto" {
            return Err(anyhow!(
                "model '{}' uses explicit tiktoken encoding '{}', which stellaclaw does not support yet",
                name,
                encoding.as_deref().unwrap_or("auto")
            ));
        }
        let _ = model_name;
        return Ok((TokenEstimatorType::Local, None));
    }

    if matches!(
        config.template,
        Some(LegacyTokenEstimationTemplateConfig::Local { .. })
    ) && matches!(
        config.tokenizer,
        Some(LegacyTokenEstimationTokenizerConfig::Local { .. })
    ) {
        return migrate_local_token_estimator_assets(name, config, base_dir);
    }

    Err(anyhow!(
        "model '{}' uses an unsupported token_estimation configuration",
        name
    ))
}

fn migrate_local_token_estimator_assets(
    name: &str,
    config: &LegacyTokenEstimationConfig,
    base_dir: &Path,
) -> Result<(TokenEstimatorType, Option<String>)> {
    let template = match config.template.as_ref() {
        Some(LegacyTokenEstimationTemplateConfig::Local { path, field }) => {
            let field = field.as_deref().unwrap_or("chat_template");
            if field != "chat_template" {
                return Err(anyhow!(
                    "model '{}' uses unsupported local token template field '{}'",
                    name,
                    field
                ));
            }
            (base_dir.join(path), field)
        }
        _ => {
            return Err(anyhow!(
                "model '{}' local token estimation requires a local template",
                name
            ));
        }
    };
    let tokenizer_path = match config.tokenizer.as_ref() {
        Some(LegacyTokenEstimationTokenizerConfig::Local { path }) => base_dir.join(path),
        _ => {
            return Err(anyhow!(
                "model '{}' local token estimation requires a local tokenizer",
                name
            ));
        }
    };
    let template_path = template.0;
    migrate_token_estimator_assets_from_paths(name, &template_path, &tokenizer_path, base_dir)
}

fn migrate_token_estimator_assets_from_paths(
    name: &str,
    template_path: &Path,
    tokenizer_path: &Path,
    base_dir: &Path,
) -> Result<(TokenEstimatorType, Option<String>)> {
    let template_path = template_path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", template_path.display()))?;
    let tokenizer_path = tokenizer_path
        .canonicalize()
        .with_context(|| format!("failed to resolve {}", tokenizer_path.display()))?;

    if template_path.file_name().and_then(|value| value.to_str()) == Some("tokenizer_config.json") {
        if template_path.parent() == tokenizer_path.parent()
            && tokenizer_path.file_name().and_then(|value| value.to_str()) == Some("tokenizer.json")
        {
            return Ok((
                TokenEstimatorType::HuggingFace,
                Some(template_path.display().to_string()),
            ));
        }
    }

    let chat_template = read_local_chat_template(name, &template_path)?;
    let migrated_base = base_dir
        .canonicalize()
        .unwrap_or_else(|_| base_dir.to_path_buf());
    let migrated_root = migrated_base
        .join(".stellaclaw_migrated")
        .join("token_estimators")
        .join(sanitize_name(name));
    fs::create_dir_all(&migrated_root)
        .with_context(|| format!("failed to create {}", migrated_root.display()))?;
    fs::copy(&tokenizer_path, migrated_root.join("tokenizer.json")).with_context(|| {
        format!(
            "failed to copy {} to {}",
            tokenizer_path.display(),
            migrated_root.join("tokenizer.json").display()
        )
    })?;
    fs::write(
        migrated_root.join("tokenizer_config.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "chat_template": chat_template,
        }))
        .context("failed to serialize migrated tokenizer_config.json")?,
    )
    .with_context(|| {
        format!(
            "failed to write {}",
            migrated_root.join("tokenizer_config.json").display()
        )
    })?;
    Ok((
        TokenEstimatorType::HuggingFace,
        Some(
            migrated_root
                .join("tokenizer_config.json")
                .display()
                .to_string(),
        ),
    ))
}

fn find_legacy_huggingface_token_estimator_assets(
    name: &str,
    repo: &str,
    revision: &str,
    template_file: &str,
    base_dir: &Path,
) -> Result<Option<(PathBuf, PathBuf)>> {
    let template_path = find_legacy_huggingface_cache_file(
        base_dir,
        "template-cache",
        name,
        repo,
        revision,
        template_file,
    )?;
    let tokenizer_path = find_legacy_huggingface_cache_file(
        base_dir,
        "tokenizer-cache",
        name,
        repo,
        revision,
        "tokenizer.json",
    )?;
    Ok(template_path.zip(tokenizer_path))
}

fn find_legacy_huggingface_cache_file(
    base_dir: &Path,
    cache_dir: &str,
    name: &str,
    repo: &str,
    revision: &str,
    file: &str,
) -> Result<Option<PathBuf>> {
    let repo_dir = base_dir
        .join(cache_dir)
        .join("hf")
        .join(name)
        .join(format!("models--{}", repo.replace('/', "--")));
    let snapshots_dir = repo_dir.join("snapshots");
    if !snapshots_dir.is_dir() {
        return Ok(None);
    }

    let ref_path = repo_dir.join("refs").join(revision);
    if ref_path.is_file() {
        let snapshot = fs::read_to_string(&ref_path)
            .with_context(|| format!("failed to read {}", ref_path.display()))?;
        let candidate = snapshots_dir.join(snapshot.trim()).join(file);
        if candidate.is_file() {
            return Ok(Some(candidate));
        }
    }

    let candidate = snapshots_dir.join(revision).join(file);
    if candidate.is_file() {
        return Ok(Some(candidate));
    }

    let mut matches = Vec::new();
    for entry in fs::read_dir(&snapshots_dir)
        .with_context(|| format!("failed to read {}", snapshots_dir.display()))?
    {
        let entry =
            entry.with_context(|| format!("failed to enumerate {}", snapshots_dir.display()))?;
        let candidate = entry.path().join(file);
        if candidate.is_file() {
            matches.push(candidate);
        }
    }
    if matches.len() == 1 {
        return Ok(matches.pop());
    }

    Ok(None)
}

fn read_local_chat_template(name: &str, path: &Path) -> Result<String> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read local token template {}", path.display()))?;
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
    {
        let value: serde_json::Value = serde_json::from_str(&raw).with_context(|| {
            format!(
                "model '{}' local token template {} is not valid JSON",
                name,
                path.display()
            )
        })?;
        return value
            .get("chat_template")
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                anyhow!(
                    "model '{}' local token template {} is missing chat_template",
                    name,
                    path.display()
                )
            });
    }
    Ok(raw)
}

fn sanitize_name(name: &str) -> String {
    let mut sanitized = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            sanitized.push(ch);
        } else {
            sanitized.push('_');
        }
    }
    if sanitized.is_empty() {
        "_".to_string()
    } else {
        sanitized
    }
}

fn build_multimodal_input(capabilities: &[ModelCapability]) -> Option<MultimodalInputConfig> {
    let image = capabilities
        .contains(&ModelCapability::ImageIn)
        .then_some(MediaInputConfig {
            transport: MediaInputTransport::InlineBase64,
            supported_media_types: vec![
                "image/png".to_string(),
                "image/jpeg".to_string(),
                "image/webp".to_string(),
                "image/gif".to_string(),
            ],
            max_width: Some(4096),
            max_height: Some(4096),
        });
    let pdf = capabilities
        .contains(&ModelCapability::PdfIn)
        .then_some(MediaInputConfig {
            transport: MediaInputTransport::InlineBase64,
            supported_media_types: vec!["application/pdf".to_string()],
            max_width: None,
            max_height: None,
        });
    let audio = capabilities
        .contains(&ModelCapability::AudioIn)
        .then_some(MediaInputConfig {
            transport: MediaInputTransport::InlineBase64,
            supported_media_types: vec![
                "audio/mpeg".to_string(),
                "audio/mp3".to_string(),
                "audio/wav".to_string(),
                "audio/ogg".to_string(),
                "audio/webm".to_string(),
                "audio/flac".to_string(),
                "audio/mp4".to_string(),
            ],
            max_width: None,
            max_height: None,
        });
    if image.is_none() && pdf.is_none() && audio.is_none() {
        None
    } else {
        Some(MultimodalInputConfig { image, pdf, audio })
    }
}

fn convert_multimodal_input(input: &LegacyMultimodalInputConfig) -> MultimodalInputConfig {
    MultimodalInputConfig {
        image: input.image.as_ref().map(convert_media_input),
        pdf: input.pdf.as_ref().map(convert_media_input),
        audio: input.audio.as_ref().map(convert_media_input),
    }
}

fn convert_media_input(input: &LegacyMediaInputConfig) -> MediaInputConfig {
    MediaInputConfig {
        transport: input.transport,
        supported_media_types: input.supported_media_types.clone(),
        max_width: input.max_width,
        max_height: input.max_height,
    }
}

fn convert_retry_mode(mode: &LegacyRetryMode) -> RetryMode {
    match mode {
        LegacyRetryMode::No => RetryMode::Once,
        LegacyRetryMode::Random {
            max_retries,
            retry_random_mean,
        } => RetryMode::RandomInterval {
            max_interval_secs: retry_random_mean.ceil().max(1.0) as u64,
            max_retries: *max_retries as u64,
        },
    }
}

fn convert_session_defaults(
    legacy: &LegacyServerConfig,
    models: &BTreeMap<String, ModelConfig>,
    main_model_name: &str,
) -> Result<SessionDefaults> {
    let main_model = legacy
        .models
        .get(main_model_name)
        .ok_or_else(|| anyhow!("missing legacy main model '{}'", main_model_name))?;
    let compression_threshold_tokens = if legacy.main_agent.enable_context_compression {
        legacy
            .main_agent
            .context_compaction
            .token_limit_override
            .map(|value| value as u64)
            .or_else(|| {
                let ratio = if legacy.main_agent.context_compaction.trigger_ratio > 0.0 {
                    legacy.main_agent.context_compaction.trigger_ratio
                } else {
                    0.8
                };
                models
                    .get(main_model_name)
                    .map(|model| (model.token_max_context as f64 * ratio) as u64)
            })
    } else {
        None
    };
    let compression_retain_recent_tokens = compression_threshold_tokens.map(|threshold| {
        let ratio = if legacy
            .main_agent
            .context_compaction
            .recent_fidelity_target_ratio
            > 0.0
        {
            legacy
                .main_agent
                .context_compaction
                .recent_fidelity_target_ratio
        } else {
            0.2
        };
        ((threshold as f64) * ratio).round().max(256.0) as u64
    });

    Ok(SessionDefaults {
        compression_threshold_tokens,
        compression_retain_recent_tokens,
        image_tool_model: resolve_helper_model(
            models,
            main_model_name,
            legacy
                .tooling
                .image
                .as_deref()
                .or(main_model.image_tool_model.as_deref()),
        )?,
        pdf_tool_model: resolve_helper_model(
            models,
            main_model_name,
            legacy.tooling.pdf.as_deref(),
        )?,
        audio_tool_model: resolve_helper_model(
            models,
            main_model_name,
            legacy.tooling.audio_input.as_deref(),
        )?,
        image_generation_tool_model: resolve_helper_model(
            models,
            main_model_name,
            legacy.tooling.image_gen.as_deref(),
        )?,
        search_tool_model: resolve_search_model(
            models,
            main_model_name,
            legacy
                .tooling
                .web_search
                .as_deref()
                .or(main_model.web_search_model.as_deref()),
        )?,
    })
}

fn resolve_helper_model(
    models: &BTreeMap<String, ModelConfig>,
    main_model_name: &str,
    raw: Option<&str>,
) -> Result<Option<ToolModelTarget>> {
    let Some(target) = raw else {
        return Ok(None);
    };
    let (alias, prefer_self) = parse_tooling_target(target)?;
    if alias == main_model_name || prefer_self {
        return Ok(Some(ToolModelTarget::inline(
            models
                .get(main_model_name)
                .cloned()
                .ok_or_else(|| anyhow!("missing main model '{}'", main_model_name))?,
        )));
    }
    models
        .get(alias)
        .cloned()
        .map(ToolModelTarget::inline)
        .map(Some)
        .ok_or_else(|| anyhow!("unknown tooling model alias '{}'", alias))
}

fn resolve_search_model(
    models: &BTreeMap<String, ModelConfig>,
    main_model_name: &str,
    raw: Option<&str>,
) -> Result<Option<ToolModelTarget>> {
    let Some(target) = raw else {
        return Ok(None);
    };
    let (alias, prefer_self) = parse_tooling_target(target)?;
    let resolved_alias = if alias == main_model_name || prefer_self {
        main_model_name
    } else {
        alias
    };
    models
        .get(resolved_alias)
        .cloned()
        .map(ToolModelTarget::inline)
        .map(Some)
        .ok_or_else(|| {
            anyhow!(
                "unknown web_search tooling model alias '{}'",
                resolved_alias
            )
        })
}

fn parse_tooling_target(raw: &str) -> Result<(&str, bool)> {
    if raw.trim().is_empty() {
        return Err(anyhow!("tooling target must not be empty"));
    }
    if let Some((alias, suffix)) = raw.split_once(':') {
        if suffix.trim() != "self" {
            return Err(anyhow!(
                "unsupported tooling target suffix '{}'; expected ':self'",
                suffix.trim()
            ));
        }
        return Ok((alias.trim(), true));
    }
    Ok((raw.trim(), false))
}

fn select_main_model_name(legacy: &LegacyServerConfig) -> Result<String> {
    if let Some(first) = legacy.chat_model_keys.first() {
        return Ok(first.clone());
    }
    if let Some(first) = legacy.available_models.first() {
        return Ok(first.clone());
    }
    if let Some(first) = legacy.agent.agent_frame.available_models.first() {
        return Ok(first.clone());
    }
    legacy
        .models
        .iter()
        .find(|(_, model)| {
            model.capabilities.contains(&LegacyModelCapability::Chat)
                || matches!(
                    model.model_type,
                    LegacyModelType::Openrouter
                        | LegacyModelType::OpenrouterResp
                        | LegacyModelType::ClaudeCode
                        | LegacyModelType::CodexSubscription
                )
        })
        .map(|(name, _)| name.clone())
        .ok_or_else(|| anyhow!("unable to determine main chat model from partyclaw config"))
}

fn convert_channel(channel: LegacyChannelConfig) -> Option<ChannelConfig> {
    match channel {
        LegacyChannelConfig::Telegram(telegram) => {
            Some(ChannelConfig::Telegram(TelegramChannelConfig {
                id: telegram.id,
                bot_token: telegram.bot_token,
                bot_token_env: telegram.bot_token_env,
                api_base_url: telegram.api_base_url,
                poll_timeout_seconds: telegram.poll_timeout_seconds,
                poll_interval_ms: telegram.poll_interval_ms,
                admin_user_ids: telegram.admin_user_ids,
            }))
        }
        LegacyChannelConfig::CommandLine(channel)
        | LegacyChannelConfig::Dingtalk(channel)
        | LegacyChannelConfig::DingtalkRobot(channel)
        | LegacyChannelConfig::Web(channel) => {
            let _ = channel.id;
            None
        }
    }
}

fn join_endpoint(base: &str, path: &str) -> String {
    let base = base.trim_end_matches('/');
    let path = if path.is_empty() {
        "/chat/completions"
    } else {
        path
    };
    if path.starts_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn parse_cache_timeout_secs(raw: Option<&str>) -> Option<u64> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let (value, unit) = raw.split_at(raw.len().saturating_sub(1));
    let amount = value.parse::<u64>().ok()?;
    match unit {
        "s" => Some(amount),
        "m" => Some(amount.saturating_mul(60)),
        "h" => Some(amount.saturating_mul(3600)),
        "d" => Some(amount.saturating_mul(86400)),
        _ => raw.parse::<u64>().ok(),
    }
}

fn default_context_window_tokens() -> usize {
    128_000
}

fn default_timeout_seconds() -> f64 {
    30.0
}

fn default_external_timeout_seconds() -> f64 {
    30.0
}

fn default_chat_completions_path() -> String {
    "/chat/completions".to_string()
}

fn default_openai_api_key_env() -> String {
    "OPENAI_API_KEY".to_string()
}

fn default_telegram_bot_token_env() -> String {
    "TELEGRAM_BOT_TOKEN".to_string()
}

fn default_telegram_api_base_url() -> String {
    "https://api.telegram.org".to_string()
}

fn default_poll_timeout_seconds() -> u64 {
    30
}

fn default_poll_interval_ms() -> u64 {
    250
}

fn default_bubblewrap_binary() -> String {
    "bwrap".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn converts_simple_legacy_telegram_config() {
        let raw = r#"
        {
          "version": "0.28",
          "models": {
            "main": {
              "type": "openrouter",
              "api_endpoint": "https://openrouter.ai/api/v1",
              "chat_completions_path": "/chat/completions",
              "model": "openai/gpt-4.1-mini",
              "api_key_env": "OPENROUTER_API_KEY",
              "context_window_tokens": 128000,
              "capabilities": ["chat", "image_in"]
            },
            "brave": {
              "type": "brave-search",
              "api_endpoint": "https://api.search.brave.com",
              "chat_completions_path": "/res/v1/web/search",
              "model": "brave-web-search",
              "api_key_env": "BRAVE_SEARCH_API_KEY",
              "capabilities": ["web_search"]
            }
          },
          "agent": {
            "agent_frame": { "available_models": ["main"] }
          },
          "tooling": {
            "web_search": "brave"
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TG_TOKEN"
            }
          ]
        }
        "#;
        let path = Path::new("/tmp/config.json");
        let config = load_and_upgrade(raw, path).expect("legacy config should convert");

        assert_eq!(
            config
                .initial_main_model()
                .expect("main model should exist")
                .model_name,
            "openai/gpt-4.1-mini"
        );
        assert_eq!(config.channels.len(), 1);
        assert!(config.session_defaults.search_tool_model.is_some());
        assert_eq!(
            config.models["brave"].provider_type,
            ProviderType::BraveSearch
        );
    }

    #[test]
    fn skips_legacy_channels_that_are_not_supported_yet() {
        let raw = r#"
        {
          "version": "0.28",
          "models": {
            "main": {
              "type": "openrouter",
              "api_endpoint": "https://openrouter.ai/api/v1",
              "chat_completions_path": "/chat/completions",
              "model": "openai/gpt-4.1-mini",
              "api_key_env": "OPENROUTER_API_KEY",
              "context_window_tokens": 128000,
              "capabilities": ["chat"]
            }
          },
          "agent": {
            "agent_frame": { "available_models": ["main"] }
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TG_TOKEN"
            },
            {
              "kind": "web",
              "id": "web-main"
            }
          ]
        }
        "#;
        let path = Path::new("/tmp/config.json");
        let config = load_and_upgrade(raw, path).expect("legacy config should convert");

        assert_eq!(config.channels.len(), 1);
        match &config.channels[0] {
            ChannelConfig::Telegram(channel) => assert_eq!(channel.id, "telegram-main"),
        }
    }

    #[test]
    fn preserves_reasoning_and_imports_local_token_estimation_assets() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw_legacy_loader_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        fs::create_dir_all(&root).expect("temp root should be created");
        fs::write(
            root.join("chat_template.jinja"),
            "{{ bos_token }}{{ messages }}",
        )
        .expect("template should write");
        fs::write(root.join("tokenizer.json"), "{\"version\":\"1.0\"}")
            .expect("tokenizer should write");

        let raw = r#"
        {
          "version": "0.28",
          "models": {
            "main": {
              "type": "openrouter",
              "api_endpoint": "https://openrouter.ai/api/v1",
              "chat_completions_path": "/chat/completions",
              "model": "openai/gpt-4.1-mini",
              "api_key_env": "OPENROUTER_API_KEY",
              "context_window_tokens": 128000,
              "reasoning": { "effort": "high", "max_tokens": 2048 },
              "token_estimation": {
                "template": {
                  "source": "local",
                  "path": "chat_template.jinja"
                },
                "tokenizer": {
                  "source": "local",
                  "path": "tokenizer.json"
                }
              },
              "capabilities": ["chat"]
            }
          },
          "agent": {
            "agent_frame": { "available_models": ["main"] }
          },
          "main_agent": {
            "memory_system": "claude_code"
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TG_TOKEN"
            }
          ]
        }
        "#;
        let path = root.join("config.json");
        let config = load_and_upgrade(raw, &path).expect("legacy config should convert");
        let main = config
            .initial_main_model()
            .expect("main model should exist");

        assert_eq!(
            main.reasoning
                .as_ref()
                .and_then(|value| value.get("effort"))
                .and_then(serde_json::Value::as_str),
            Some("high")
        );
        assert_eq!(main.token_estimator_type, TokenEstimatorType::HuggingFace);
        let estimator_url = main
            .token_estimator_url
            .as_deref()
            .expect("token estimator url should exist");
        assert!(estimator_url.ends_with("tokenizer_config.json"));
        assert!(Path::new(estimator_url).is_file());

        let migrated = fs::read_to_string(estimator_url).expect("migrated config should exist");
        assert!(migrated.contains("\"chat_template\""));

        fs::remove_dir_all(&root).expect("temp root should be cleaned");
    }

    #[test]
    fn imports_legacy_huggingface_split_template_cache() {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw_legacy_hf_loader_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("time should move forward")
                .as_nanos()
        ));
        let repo_root = root.join("template-cache/hf/glm-5.1/models--zai-org--GLM-5.1");
        let tokenizer_repo_root = root.join("tokenizer-cache/hf/glm-5.1/models--zai-org--GLM-5.1");
        fs::create_dir_all(repo_root.join("snapshots/snapshot-a")).expect("template cache exists");
        fs::create_dir_all(repo_root.join("refs")).expect("template refs exists");
        fs::create_dir_all(tokenizer_repo_root.join("snapshots/snapshot-a"))
            .expect("tokenizer cache exists");
        fs::create_dir_all(tokenizer_repo_root.join("refs")).expect("tokenizer refs exists");
        fs::write(repo_root.join("refs/main"), "snapshot-a").expect("template ref should write");
        fs::write(tokenizer_repo_root.join("refs/main"), "snapshot-a")
            .expect("tokenizer ref should write");
        fs::write(
            repo_root.join("snapshots/snapshot-a/chat_template.jinja"),
            "{{ messages }}",
        )
        .expect("template should write");
        fs::write(
            tokenizer_repo_root.join("snapshots/snapshot-a/tokenizer.json"),
            "{\"version\":\"1.0\"}",
        )
        .expect("tokenizer should write");

        let raw = r#"
        {
          "version": "0.28",
          "models": {
            "glm-5.1": {
              "type": "openrouter",
              "api_endpoint": "https://openrouter.ai/api/v1",
              "chat_completions_path": "/chat/completions",
              "model": "zai-org/glm-5.1",
              "api_key_env": "OPENROUTER_API_KEY",
              "context_window_tokens": 128000,
              "token_estimation": {
                "template": {
                  "source": "huggingface",
                  "repo": "zai-org/GLM-5.1",
                  "revision": "main",
                  "file": "chat_template.jinja",
                  "field": "chat_template"
                },
                "tokenizer": {
                  "source": "huggingface",
                  "repo": "zai-org/GLM-5.1",
                  "revision": "main",
                  "file": "tokenizer.json"
                }
              },
              "capabilities": ["chat"]
            }
          },
          "agent": {
            "agent_frame": { "available_models": ["glm-5.1"] }
          },
          "channels": [
            {
              "kind": "telegram",
              "id": "telegram-main",
              "bot_token_env": "TG_TOKEN"
            }
          ]
        }
        "#;
        let path = root.join("config.json");
        let config = load_and_upgrade(raw, &path).expect("legacy config should convert");
        let main_model = config
            .initial_main_model()
            .expect("main model should exist");
        let estimator_url = main_model
            .token_estimator_url
            .as_deref()
            .expect("token estimator url should exist");

        assert!(estimator_url.contains(".stellaclaw_migrated"));
        assert!(Path::new(estimator_url).is_file());
        assert!(Path::new(estimator_url)
            .with_file_name("tokenizer.json")
            .is_file());

        fs::remove_dir_all(&root).expect("temp root should be cleaned");
    }
}
