mod download_tools;
mod dsl_tools;
mod file_tools;
mod host_tools;
mod media_tools;
mod process_tools;
mod schema;
mod skill_tools;
mod web_tools;

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use thiserror::Error;

use crate::model_config::{ModelCapability, ModelConfig};

pub use super::ToolRemoteMode;
use super::{SessionInitial, SessionType};

pub use download_tools::download_tool_definitions;
pub(crate) use download_tools::execute_download_tool;
pub(crate) use file_tools::execute_file_tool;
pub use file_tools::file_tool_definitions;
pub use host_tools::{host_tool_definitions, HostToolScope};
pub use media_tools::media_tool_definitions;
pub(crate) use media_tools::{execute_media_tool, execute_provider_backed_media_tool};
pub(crate) use process_tools::execute_process_tool;
pub use process_tools::process_tool_definitions;
pub(crate) use skill_tools::execute_skill_load_tool;
pub use skill_tools::skill_tool_definitions;
pub(crate) use web_tools::execute_web_tool;
pub use web_tools::{web_tool_definitions, WebSearchOptions};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolExecutionMode {
    Immediate,
    Interruptible,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBackedToolKind {
    ImageAnalysis,
    PdfAnalysis,
    AudioAnalysis,
    ImageGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolBackend {
    Local,
    ProviderBacked { kind: ProviderBackedToolKind },
    ConversationBridge { action: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub execution_mode: ToolExecutionMode,
    pub backend: ToolBackend,
}

impl ToolDefinition {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        execution_mode: ToolExecutionMode,
        backend: ToolBackend,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            execution_mode,
            backend,
        }
    }

    pub fn openai_tool_schema(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description_with_execution_mode(),
                "parameters": self.parameters,
            }
        })
    }

    pub fn responses_tool_schema(&self) -> Value {
        json!({
            "type": "function",
            "name": self.name,
            "description": self.description_with_execution_mode(),
            "parameters": self.parameters,
        })
    }

    pub fn claude_tool_schema(&self) -> Value {
        json!({
            "name": self.name,
            "description": self.description_with_execution_mode(),
            "input_schema": self.parameters,
        })
    }

    fn description_with_execution_mode(&self) -> String {
        let guidance = match self.execution_mode {
            ToolExecutionMode::Immediate => {
                "Execution mode: immediate. This tool returns promptly and still runs through the ToolBatchExecutor thread."
            }
            ToolExecutionMode::Interruptible => {
                "Execution mode: interruptible. This tool may wait, but the ToolBatchExecutor can interrupt the current batch and return a stable result."
            }
        };

        format!("{guidance} {}", self.description)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolCatalog {
    tools: BTreeMap<String, ToolDefinition>,
}

impl ToolCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_model_config(model_config: &ModelConfig) -> Result<Self, ToolCatalogError> {
        let options = BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: WebSearchOptions {
                enabled: model_config.supports(ModelCapability::WebSearch),
                ..WebSearchOptions::default()
            },
            enable_native_image_load: model_config.supports(ModelCapability::ImageIn),
            enable_native_pdf_load: model_config.supports(ModelCapability::PdfIn),
            enable_native_audio_load: model_config.supports(ModelCapability::AudioIn),
            enable_provider_image_generation: model_config.supports(ModelCapability::ImageOut),
            ..BuiltinToolCatalogOptions::default()
        };

        builtin_tool_catalog(options)
    }

    pub fn from_model_config_and_session_type(
        model_config: &ModelConfig,
        session_type: SessionType,
    ) -> Result<Self, ToolCatalogError> {
        let options = BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: WebSearchOptions {
                enabled: model_config.supports(ModelCapability::WebSearch),
                ..WebSearchOptions::default()
            },
            enable_native_image_load: model_config.supports(ModelCapability::ImageIn),
            enable_native_pdf_load: model_config.supports(ModelCapability::PdfIn),
            enable_native_audio_load: model_config.supports(ModelCapability::AudioIn),
            enable_provider_image_generation: model_config.supports(ModelCapability::ImageOut),
            host_tool_scope: Some(HostToolScope::from(session_type)),
            ..BuiltinToolCatalogOptions::default()
        };

        builtin_tool_catalog(options)
    }

    pub fn from_model_config_and_initial(
        model_config: &ModelConfig,
        initial: &SessionInitial,
    ) -> Result<Self, ToolCatalogError> {
        let image_tool = selected_media_tool(model_config, initial.image_tool_model.as_ref());
        let pdf_tool = selected_media_tool(model_config, initial.pdf_tool_model.as_ref());
        let audio_tool = selected_media_tool(model_config, initial.audio_tool_model.as_ref());
        let generation_tool =
            selected_media_tool(model_config, initial.image_generation_tool_model.as_ref());
        let search_tool = selected_media_tool(model_config, initial.search_tool_model.as_ref());
        let options = BuiltinToolCatalogOptions {
            remote_mode: initial.tool_remote_mode.clone(),
            web_search: WebSearchOptions {
                enabled: search_tool.model_supports(ModelCapability::WebSearch)
                    || initial.search_image_tool_model.is_some()
                    || initial.search_video_tool_model.is_some()
                    || initial.search_news_tool_model.is_some(),
                image: initial
                    .search_image_tool_model
                    .as_ref()
                    .is_some_and(|model| model.supports(ModelCapability::WebSearch)),
                video: initial
                    .search_video_tool_model
                    .as_ref()
                    .is_some_and(|model| model.supports(ModelCapability::WebSearch)),
                news: initial
                    .search_news_tool_model
                    .as_ref()
                    .is_some_and(|model| model.supports(ModelCapability::WebSearch)),
            },
            enable_native_image_load: image_tool.self_model
                && image_tool.model_supports(ModelCapability::ImageIn),
            enable_native_pdf_load: pdf_tool.self_model
                && pdf_tool.model_supports(ModelCapability::PdfIn),
            enable_native_audio_load: audio_tool.self_model
                && audio_tool.model_supports(ModelCapability::AudioIn),
            enable_provider_image_analysis: !image_tool.self_model
                && image_tool.model_supports(ModelCapability::ImageIn),
            enable_provider_pdf_analysis: !pdf_tool.self_model
                && pdf_tool.model_supports(ModelCapability::PdfIn),
            enable_provider_audio_analysis: !audio_tool.self_model
                && audio_tool.model_supports(ModelCapability::AudioIn),
            enable_provider_image_generation: generation_tool
                .model_supports(ModelCapability::ImageOut),
            enable_skill_persistence_tools: true,
            host_tool_scope: Some(HostToolScope::from(initial.session_type)),
            ..BuiltinToolCatalogOptions::default()
        };

        builtin_tool_catalog(options)
    }

    pub fn add(&mut self, tool: ToolDefinition) -> Result<(), ToolCatalogError> {
        if self.tools.contains_key(&tool.name) {
            return Err(ToolCatalogError::DuplicateTool(tool.name));
        }

        self.tools.insert(tool.name.clone(), tool);
        Ok(())
    }

    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.get(name)
    }

    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &ToolDefinition)> {
        self.tools.iter()
    }

    pub fn openai_tool_schemas(&self) -> Vec<Value> {
        self.tools
            .values()
            .map(ToolDefinition::openai_tool_schema)
            .collect()
    }
}

struct SelectedMediaTool<'a> {
    model_config: &'a ModelConfig,
    self_model: bool,
}

impl SelectedMediaTool<'_> {
    fn model_supports(&self, capability: ModelCapability) -> bool {
        self.model_config.supports(capability)
    }
}

fn selected_media_tool<'a>(
    primary_model: &'a ModelConfig,
    configured_model: Option<&'a ModelConfig>,
) -> SelectedMediaTool<'a> {
    match configured_model {
        Some(model_config) if model_config != primary_model => SelectedMediaTool {
            model_config,
            self_model: false,
        },
        _ => SelectedMediaTool {
            model_config: primary_model,
            self_model: true,
        },
    }
}

impl From<SessionType> for HostToolScope {
    fn from(value: SessionType) -> Self {
        match value {
            SessionType::Foreground => HostToolScope::MainForeground,
            SessionType::Background => HostToolScope::MainBackground,
            SessionType::Subagent => HostToolScope::SubAgent,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuiltinToolCatalogOptions {
    pub remote_mode: ToolRemoteMode,
    pub web_search: WebSearchOptions,
    pub enable_native_image_load: bool,
    pub enable_native_pdf_load: bool,
    pub enable_native_audio_load: bool,
    pub enable_provider_image_analysis: bool,
    pub enable_provider_pdf_analysis: bool,
    pub enable_provider_audio_analysis: bool,
    pub enable_provider_image_generation: bool,
    pub skill_names: Vec<String>,
    pub enable_skill_persistence_tools: bool,
    pub host_tool_scope: Option<HostToolScope>,
}

impl Default for BuiltinToolCatalogOptions {
    fn default() -> Self {
        Self {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: WebSearchOptions::default(),
            enable_native_image_load: false,
            enable_native_pdf_load: false,
            enable_native_audio_load: false,
            enable_provider_image_analysis: false,
            enable_provider_pdf_analysis: false,
            enable_provider_audio_analysis: false,
            enable_provider_image_generation: false,
            skill_names: Vec::new(),
            enable_skill_persistence_tools: false,
            host_tool_scope: None,
        }
    }
}

pub fn builtin_tool_catalog(
    options: BuiltinToolCatalogOptions,
) -> Result<ToolCatalog, ToolCatalogError> {
    let mut catalog = ToolCatalog::new();

    for tool in file_tool_definitions(&options.remote_mode) {
        catalog.add(tool)?;
    }
    for tool in process_tool_definitions(&options.remote_mode) {
        catalog.add(tool)?;
    }
    for tool in download_tool_definitions(&options.remote_mode) {
        catalog.add(tool)?;
    }
    for tool in web_tool_definitions(options.web_search) {
        catalog.add(tool)?;
    }
    for tool in media_tool_definitions(&options) {
        catalog.add(tool)?;
    }
    for tool in skill_tool_definitions(&options.skill_names, options.enable_skill_persistence_tools)
    {
        catalog.add(tool)?;
    }
    if let Some(scope) = options.host_tool_scope {
        for tool in host_tool_definitions(scope) {
            catalog.add(tool)?;
        }
    }

    Ok(catalog)
}

#[derive(Debug, Error)]
pub enum ToolCatalogError {
    #[error("duplicate tool definition: {0}")]
    DuplicateTool(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model_config::{ProviderType, RetryMode, TokenEstimatorType};

    #[test]
    fn builds_builtin_catalog_with_copied_core_tool_schemas() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            web_search: WebSearchOptions {
                enabled: true,
                image: true,
                video: true,
                news: true,
            },
            enable_native_image_load: true,
            enable_native_pdf_load: true,
            enable_native_audio_load: true,
            enable_provider_image_analysis: true,
            enable_provider_pdf_analysis: true,
            enable_provider_audio_analysis: true,
            enable_provider_image_generation: true,
            skill_names: vec!["imagegen".to_string()],
            enable_skill_persistence_tools: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        assert!(catalog.contains("file_read"));
        assert!(catalog.contains("shell"));
        assert!(catalog.contains("file_download_start"));
        assert!(!catalog.contains("dsl_start"));
        assert!(catalog.contains("web_search"));
        assert!(catalog.contains("image_analysis"));
        assert!(catalog.contains("image_stop"));
        assert!(catalog.contains("image_load"));
        assert!(catalog.contains("pdf_analysis"));
        assert!(catalog.contains("pdf_stop"));
        assert!(catalog.contains("audio_analysis"));
        assert!(catalog.contains("audio_stop"));
        assert!(catalog.contains("image_generation"));
        assert!(catalog.contains("image_generation_stop"));
        assert!(catalog.contains("skill_load"));
        assert!(catalog.contains("skill_create"));
        assert!(catalog.contains("skill_update"));
        assert!(catalog.contains("skill_delete"));

        let file_read = catalog.get("file_read").unwrap();
        assert_eq!(file_read.parameters["required"], json!(["file_path"]));
        assert!(file_read.parameters["properties"].get("remote").is_some());

        let ls = catalog.get("ls").unwrap();
        assert_eq!(ls.parameters["required"], json!([]));

        let shell = catalog.get("shell").unwrap();
        assert_eq!(shell.execution_mode, ToolExecutionMode::Interruptible);
        assert!(shell.parameters["properties"].get("wait_ms").is_some());

        let image_generation = catalog.get("image_generation").unwrap();
        assert_eq!(
            image_generation.backend,
            ToolBackend::ProviderBacked {
                kind: ProviderBackedToolKind::ImageGeneration
            }
        );
    }

    #[test]
    fn model_config_enables_capability_dependent_tools() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![
                ModelCapability::Chat,
                ModelCapability::ImageIn,
                ModelCapability::ImageOut,
                ModelCapability::PdfIn,
                ModelCapability::AudioIn,
                ModelCapability::WebSearch,
            ],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let catalog = ToolCatalog::from_model_config(&config).expect("catalog should build");

        assert!(catalog.contains("web_search"));
        assert!(catalog.contains("image_load"));
        assert!(catalog.contains("pdf_load"));
        assert!(catalog.contains("audio_load"));
        assert!(catalog.contains("image_generation"));
    }

    #[test]
    fn initial_external_media_models_expose_analysis_tools_instead_of_load_tools() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut image_helper = primary.clone();
        image_helper.model_name = "vision-helper".to_string();
        image_helper.capabilities = vec![ModelCapability::Chat, ModelCapability::ImageIn];
        let mut pdf_helper = primary.clone();
        pdf_helper.model_name = "pdf-helper".to_string();
        pdf_helper.capabilities = vec![ModelCapability::Chat, ModelCapability::PdfIn];
        let mut audio_helper = primary.clone();
        audio_helper.model_name = "audio-helper".to_string();
        audio_helper.capabilities = vec![ModelCapability::Chat, ModelCapability::AudioIn];

        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.image_tool_model = Some(image_helper);
        initial.pdf_tool_model = Some(pdf_helper);
        initial.audio_tool_model = Some(audio_helper);
        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        assert!(catalog.contains("image_analysis"));
        assert!(catalog.contains("image_stop"));
        assert!(!catalog.contains("image_load"));
        assert!(catalog.contains("pdf_analysis"));
        assert!(catalog.contains("pdf_stop"));
        assert!(!catalog.contains("pdf_load"));
        assert!(catalog.contains("audio_analysis"));
        assert!(catalog.contains("audio_stop"));
        assert!(!catalog.contains("audio_load"));
    }

    #[test]
    fn initial_search_model_exposes_web_search_for_text_primary_model() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut search_model = primary.clone();
        search_model.provider_type = ProviderType::BraveSearch;
        search_model.model_name = "brave-web-search".to_string();
        search_model.url = "https://api.search.brave.com/res/v1/web/search".to_string();
        search_model.api_key_env = "BRAVE_SEARCH_API_KEY".to_string();
        search_model.capabilities = vec![ModelCapability::WebSearch];
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.search_tool_model = Some(search_model);

        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        assert!(catalog.contains("web_search"));
    }

    #[test]
    fn initial_image_search_model_enables_image_mode_on_web_search() {
        let primary = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "text-model".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut image_search_model = primary.clone();
        image_search_model.provider_type = ProviderType::BraveSearchImage;
        image_search_model.model_name = "brave-image-search".to_string();
        image_search_model.url = "https://api.search.brave.com/res/v1/images/search".to_string();
        image_search_model.api_key_env = "BRAVE_SEARCH_API_KEY".to_string();
        image_search_model.capabilities = vec![ModelCapability::WebSearch];
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.search_image_tool_model = Some(image_search_model);

        let catalog =
            ToolCatalog::from_model_config_and_initial(&primary, &initial).expect("catalog");

        let tool = catalog
            .get("web_search")
            .expect("web_search should be exposed");
        assert!(tool.description.contains("image"));
        assert!(tool.parameters["properties"].get("image").is_some());
    }

    #[test]
    fn session_type_controls_host_tool_scope() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };

        let foreground =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Foreground)
                .expect("foreground catalog should build");
        let background =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Background)
                .expect("background catalog should build");
        let subagent =
            ToolCatalog::from_model_config_and_session_type(&config, SessionType::Subagent)
                .expect("subagent catalog should build");

        assert!(foreground.contains("start_background_agent"));
        assert!(!foreground.contains("terminate"));
        assert!(background.contains("terminate"));
        assert!(!background.contains("start_background_agent"));
        assert!(subagent.contains("user_tell"));
        let subagent_start = subagent.get("subagent_start").unwrap();
        assert!(subagent_start.parameters["properties"]
            .get("model")
            .is_none());
        assert!(!subagent.contains("list_cron_tasks"));
    }

    #[test]
    fn exports_openai_tool_schema() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions::default())
            .expect("catalog should build");
        let schemas = catalog.openai_tool_schemas();

        let file_read = schemas
            .iter()
            .find(|tool| tool["function"]["name"] == "file_read")
            .expect("file_read schema should be exported");

        assert_eq!(file_read["type"], "function");
        assert_eq!(
            file_read["function"]["parameters"]["additionalProperties"],
            false
        );
        assert!(file_read["function"]["description"]
            .as_str()
            .unwrap()
            .contains("Execution mode: immediate"));
    }

    #[test]
    fn host_tools_use_conversation_bridge_without_legacy_workspace_tools() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            host_tool_scope: Some(HostToolScope::MainForeground),
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        assert!(catalog.contains("user_tell"));
        assert!(catalog.contains("subagent_start"));
        assert!(catalog.contains("start_background_agent"));
        assert!(catalog.contains("list_cron_tasks"));
        assert!(!catalog.contains("workpath_add"));
        assert!(!catalog.contains("workspaces_list"));
        assert!(!catalog.contains("workspace_content_list"));
        assert!(!catalog.contains("workspace_mount"));
        assert!(!catalog.contains("workspace_content_move"));

        let user_tell = catalog.get("user_tell").unwrap();
        assert_eq!(
            user_tell.backend,
            ToolBackend::ConversationBridge {
                action: "user_tell".to_string()
            }
        );

        let background = catalog.get("start_background_agent").unwrap();
        assert_eq!(background.parameters["required"], json!(["task"]));
        assert!(background.parameters["properties"].get("model").is_none());
    }

    #[test]
    fn selectable_remote_mode_exposes_remote_schema_from_ssh_config() {
        let config = ModelConfig {
            provider_type: ProviderType::OpenRouterCompletion,
            model_name: "openai/gpt-4o-mini".to_string(),
            url: "https://openrouter.ai/api/v1/chat/completions".to_string(),
            api_key_env: "OPENROUTER_API_KEY".to_string(),
            capabilities: vec![ModelCapability::Chat],
            token_max_context: 128_000,
            max_tokens: 0,
            cache_timeout: 300,
            conn_timeout: 10,
            request_timeout: 600,
            max_request_size: 30 * 1024 * 1024,
            retry_mode: RetryMode::Once,
            reasoning: None,
            token_estimator_type: TokenEstimatorType::Local,
            multimodal_estimator: None,
            multimodal_input: None,
            token_estimator_url: None,
        };
        let mut initial = SessionInitial::new("session_1", SessionType::Foreground);
        initial.tool_remote_mode = ToolRemoteMode::Selectable;

        let catalog =
            ToolCatalog::from_model_config_and_initial(&config, &initial).expect("catalog");
        let file_read = catalog.get("file_read").unwrap();
        let remote = &file_read.parameters["properties"]["remote"];

        assert_eq!(remote["type"], "string");
        assert!(remote["description"]
            .as_str()
            .unwrap()
            .contains("~/.ssh/config"));
        assert!(
            catalog.get("file_download_start").unwrap().parameters["properties"]
                .get("remote")
                .is_some()
        );
        assert!(!catalog.contains("workpath_add"));
        assert!(catalog.contains("skill_load"));
        assert!(catalog.contains("skill_create"));
        assert!(catalog.contains("skill_update"));
        assert!(catalog.contains("skill_delete"));
    }

    #[test]
    fn fixed_remote_mode_hides_remote_schema() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::FixedSsh {
                host: "gpu-box".to_string(),
                cwd: None,
            },
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        let file_read = catalog.get("file_read").unwrap();
        assert!(file_read.parameters["properties"].get("remote").is_none());
        let shell = catalog.get("shell").unwrap();
        assert!(shell.parameters["properties"].get("remote").is_none());
        let download = catalog.get("file_download_start").unwrap();
        assert!(download.parameters["properties"].get("remote").is_none());
    }

    #[test]
    fn media_path_tools_expose_remote_schema_in_selectable_mode() {
        let catalog = builtin_tool_catalog(BuiltinToolCatalogOptions {
            remote_mode: ToolRemoteMode::Selectable,
            enable_native_image_load: true,
            enable_native_pdf_load: true,
            enable_native_audio_load: true,
            enable_provider_image_analysis: true,
            enable_provider_pdf_analysis: true,
            enable_provider_audio_analysis: true,
            ..BuiltinToolCatalogOptions::default()
        })
        .expect("catalog should build");

        for name in [
            "image_load",
            "pdf_load",
            "audio_load",
            "image_analysis",
            "pdf_analysis",
            "audio_analysis",
        ] {
            let tool = catalog
                .get(name)
                .unwrap_or_else(|| panic!("missing {name}"));
            assert!(tool.parameters["properties"].get("remote").is_some());
        }
    }
}
