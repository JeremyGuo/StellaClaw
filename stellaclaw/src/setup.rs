use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Context, Result};
use stellaclaw_core::model_config::{
    default_max_request_size, MediaInputConfig, MediaInputTransport, ModelCapability, ModelConfig,
    MultimodalInputConfig, ProviderType, RetryMode, TokenEstimatorType,
};

use crate::config::{
    AgentServerConfig, ChannelConfig, MemoryConfig, ModelSelection, SandboxConfig, SessionDefaults,
    SessionProfile, StellaclawConfig, TelegramChannelConfig, ToolModelTarget, WebChannelConfig,
    LATEST_CONFIG_VERSION,
};

pub struct SetupArgs {
    pub config: PathBuf,
    pub workdir: PathBuf,
    pub install_systemd: bool,
    pub systemd_user: bool,
    pub dry_run: bool,
}

pub fn run(args: SetupArgs) -> Result<()> {
    let config_path = absolute_path(&args.config)?;
    let workdir = absolute_path(&args.workdir)?;

    if !args.dry_run {
        ensure_config_target_writable(&config_path)?;
        ensure_directory_writable(&workdir, "workdir")?;
        if args.install_systemd {
            ensure_systemd_target_writable(args.systemd_user)?;
        }
    }

    let mut config = empty_config();

    println!("{}", bold("Stellaclaw setup"));
    println!("{} {}", dim("Config:"), config_path.display());
    println!("{} {}", dim("Workdir:"), workdir.display());
    if args.dry_run {
        println!(
            "{}",
            yellow("Dry run: no files or systemd units will be created.")
        );
    }
    println!();

    configure_models(&mut config)?;
    configure_tooling(&mut config)?;
    configure_memory(&mut config)?;
    configure_channels(&mut config)?;
    confirm_and_create(&args, &config_path, &workdir, &mut config)?;
    Ok(())
}

fn empty_config() -> StellaclawConfig {
    StellaclawConfig {
        version: LATEST_CONFIG_VERSION.to_string(),
        agent_server: AgentServerConfig::default(),
        default_profile: None,
        models: BTreeMap::new(),
        available_agent_models: Vec::new(),
        session_defaults: SessionDefaults::default(),
        memory: MemoryConfig::default(),
        skill_sync: Vec::new(),
        sandbox: SandboxConfig::default(),
        channels: Vec::new(),
    }
}

fn stage_title(text: &str) -> String {
    bold(&cyan(text))
}

fn bold(text: &str) -> String {
    paint("1", text)
}

fn dim(text: &str) -> String {
    paint("2", text)
}

fn cyan(text: &str) -> String {
    paint("36", text)
}

fn green(text: &str) -> String {
    paint("32", text)
}

fn yellow(text: &str) -> String {
    paint("33", text)
}

fn paint(code: &str, text: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

fn color_enabled() -> bool {
    env::var_os("NO_COLOR").is_none()
        && env::var("TERM").map(|term| term != "dumb").unwrap_or(true)
        && io::stdout().is_terminal()
}

fn configure_models(config: &mut StellaclawConfig) -> Result<()> {
    loop {
        println!();
        println!("{}", stage_title("Stage 1/5: Model configuration"));
        print_model_summary(config);
        let choice = prompt_menu(
            "Choose an action",
            &[
                "Add Codex subscription chat model",
                "Add OpenRouter chat model",
                "Add Claude chat model",
                "Remove model",
                "Continue",
            ],
        )?;
        match choice {
            0 => add_codex_model(config)?,
            1 => add_openrouter_model(config)?,
            2 => add_claude_model(config)?,
            3 => remove_model(config)?,
            4 if config.available_agent_models().is_empty() => {
                println!(
                    "{}",
                    yellow("Add at least one chat-capable model before continuing.")
                );
            }
            4 => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn add_codex_model(config: &mut StellaclawConfig) -> Result<()> {
    println!();
    println!(
        "{}",
        dim("Codex subscription uses Codex auth.json first, then CHATGPT_ACCESS_TOKEN.")
    );
    maybe_run_codex_login()?;

    let alias = prompt_alias(config, "Model alias", "main")?;
    let model_name = prompt_text("Codex model name", Some("gpt-5.5"))?;
    let context = prompt_u64("Context window tokens", 400_000)?;
    let priority = prompt_bool("Use priority service tier when available", false)?;

    let mut model = base_model(
        ProviderType::CodexSubscription,
        model_name,
        "https://chatgpt.com/backend-api/codex/responses".to_string(),
        "CHATGPT_ACCESS_TOKEN".to_string(),
        vec![
            ModelCapability::Chat,
            ModelCapability::ImageIn,
            ModelCapability::PdfIn,
        ],
        context,
    );
    model.multimodal_input = Some(multimodal_input(true, true, false));
    if priority {
        model.reasoning = Some(serde_json::json!({"service_tier": "priority"}));
    }
    insert_model(config, alias, model);
    Ok(())
}

fn maybe_run_codex_login() -> Result<()> {
    if codex_auth_json_exists() {
        println!("{}", green("Existing Codex auth.json was found."));
        return Ok(());
    }
    let Some(codex) = find_in_path("codex") else {
        println!("{}", yellow("codex executable was not found in PATH."));
        println!(
            "{}",
            dim("Install Codex CLI, then run `codex login --device-auth`, or provide CHATGPT_ACCESS_TOKEN.")
        );
        return Ok(());
    };
    if !prompt_bool("Run `codex login --device-auth` now", true)? {
        return Ok(());
    }
    let status = Command::new(codex)
        .args(["login", "--device-auth"])
        .status()
        .context("failed to run codex login --device-auth")?;
    if !status.success() {
        println!(
            "{}",
            yellow(&format!(
                "codex login exited with status {status}; setup will continue."
            ))
        );
    }
    Ok(())
}

fn add_openrouter_model(config: &mut StellaclawConfig) -> Result<()> {
    println!();
    let provider_choice = prompt_menu(
        "OpenRouter provider API",
        &["Chat Completions", "Responses"],
    )?;
    let (provider_type, default_url) = if provider_choice == 0 {
        (
            ProviderType::OpenRouterCompletion,
            "https://openrouter.ai/api/v1/chat/completions",
        )
    } else {
        (
            ProviderType::OpenRouterResponses,
            "https://openrouter.ai/api/v1/responses",
        )
    };
    let alias = prompt_alias(config, "Model alias", "main")?;
    let model_name = prompt_text("OpenRouter model name", Some("openai/gpt-4.1-mini"))?;
    let api_key_env = prompt_text("API key environment variable", Some("OPENROUTER_API_KEY"))?;
    let url = prompt_text("Endpoint URL", Some(default_url))?;
    let context = prompt_u64("Context window tokens", 1_048_576)?;
    let image = prompt_bool("Enable image input for this model", true)?;
    let pdf = prompt_bool("Enable PDF input for this model", true)?;
    let audio = prompt_bool("Enable audio input for this model", false)?;
    let mut capabilities = vec![ModelCapability::Chat];
    if image {
        capabilities.push(ModelCapability::ImageIn);
    }
    if pdf {
        capabilities.push(ModelCapability::PdfIn);
    }
    if audio {
        capabilities.push(ModelCapability::AudioIn);
    }
    let mut model = base_model(
        provider_type,
        model_name,
        url,
        api_key_env,
        capabilities,
        context,
    );
    model.multimodal_input = Some(multimodal_input(image, pdf, audio));
    insert_model(config, alias, model);
    Ok(())
}

fn add_claude_model(config: &mut StellaclawConfig) -> Result<()> {
    println!();
    let alias = prompt_alias(config, "Model alias", "main")?;
    let model_name = prompt_text("Claude model name", Some("claude-sonnet-4-5"))?;
    let api_key_env = prompt_text("API key environment variable", Some("ANTHROPIC_API_KEY"))?;
    let url = prompt_text(
        "Endpoint URL",
        Some("https://api.anthropic.com/v1/messages"),
    )?;
    let context = prompt_u64("Context window tokens", 200_000)?;
    let image = prompt_bool("Enable image input for this model", true)?;
    let pdf = prompt_bool("Enable PDF input for this model", true)?;
    let mut capabilities = vec![ModelCapability::Chat];
    if image {
        capabilities.push(ModelCapability::ImageIn);
    }
    if pdf {
        capabilities.push(ModelCapability::PdfIn);
    }
    let mut model = base_model(
        ProviderType::ClaudeCode,
        model_name,
        url,
        api_key_env,
        capabilities,
        context,
    );
    model.multimodal_input = Some(multimodal_input(image, pdf, false));
    insert_model(config, alias, model);
    Ok(())
}

fn configure_tooling(config: &mut StellaclawConfig) -> Result<()> {
    loop {
        println!();
        println!("{}", stage_title("Stage 2/5: Tooling configuration"));
        println!(
            "{}",
            dim("Tool model target `main:self` means the tool reuses the active session model.")
        );
        println!("{}", dim("Use it when the current model already supports the capability; use a plain alias for a separate helper model."));
        print_tooling_summary(config);
        let choice = prompt_menu(
            "Choose an action",
            &[
                "Advanced: override compression budgets",
                "Set media helper tool models",
                "Add Brave Search tool models",
                "Add OpenAI image generation tool model",
                "Remove model",
                "Continue",
            ],
        )?;
        match choice {
            0 => configure_compression(config)?,
            1 => configure_media_targets(config)?,
            2 => add_brave_search_models(config)?,
            3 => add_openai_image_model(config)?,
            4 => remove_model(config)?,
            5 => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn configure_compression(config: &mut StellaclawConfig) -> Result<()> {
    println!(
        "{}",
        dim("By default Stellaclaw compresses at about 90% of the active model context window.")
    );
    println!(
        "{}",
        dim("You usually do not need to set this. Use an override only to compress earlier.")
    );
    println!(
        "{}",
        dim("Retain is the recent high-fidelity token budget kept uncompressed after compaction.")
    );
    let threshold = prompt_optional_u64_with_label(
        "Override compression threshold tokens",
        config.session_defaults.compression_threshold_tokens,
        &default_compression_threshold_label(config),
    )?;
    let retain_default_label = threshold
        .map(|value| format!("default {} (10% of override threshold)", value / 10))
        .unwrap_or_else(|| default_compression_retain_label(config));
    let retain = prompt_optional_u64_with_label(
        "Override recent high-fidelity retain tokens",
        config.session_defaults.compression_retain_recent_tokens,
        &retain_default_label,
    )?;
    config.session_defaults.compression_threshold_tokens = threshold;
    config.session_defaults.compression_retain_recent_tokens = retain;
    Ok(())
}

fn configure_media_targets(config: &mut StellaclawConfig) -> Result<()> {
    config.session_defaults.image_tool_model =
        prompt_tool_target(config, "Image understanding tool model")?;
    config.session_defaults.pdf_tool_model = prompt_tool_target(config, "PDF tool model")?;
    config.session_defaults.audio_tool_model = prompt_tool_target(config, "Audio tool model")?;
    Ok(())
}

fn add_brave_search_models(config: &mut StellaclawConfig) -> Result<()> {
    println!();
    let api_key_env = prompt_text(
        "Brave Search API key environment variable",
        Some("BRAVE_SEARCH_API_KEY"),
    )?;
    let web = prompt_alias(config, "Web search model alias", "search_brave")?;
    let image = prompt_alias(config, "Image search model alias", "search_brave_image")?;
    let video = prompt_alias(config, "Video search model alias", "search_brave_video")?;
    let news = prompt_alias(config, "News search model alias", "search_brave_news")?;

    insert_model(
        config,
        web.clone(),
        search_model(
            ProviderType::BraveSearch,
            "brave-web-search",
            "https://api.search.brave.com/res/v1/web/search",
            &api_key_env,
        ),
    );
    insert_model(
        config,
        image.clone(),
        search_model(
            ProviderType::BraveSearchImage,
            "brave-image-search",
            "https://api.search.brave.com/res/v1/images/search",
            &api_key_env,
        ),
    );
    insert_model(
        config,
        video.clone(),
        search_model(
            ProviderType::BraveSearchVideo,
            "brave-video-search",
            "https://api.search.brave.com/res/v1/videos/search",
            &api_key_env,
        ),
    );
    insert_model(
        config,
        news.clone(),
        search_model(
            ProviderType::BraveSearchNews,
            "brave-news-search",
            "https://api.search.brave.com/res/v1/news/search",
            &api_key_env,
        ),
    );
    config.session_defaults.search_tool_model = Some(ToolModelTarget::Alias(web));
    config.session_defaults.search_image_tool_model = Some(ToolModelTarget::Alias(image));
    config.session_defaults.search_video_tool_model = Some(ToolModelTarget::Alias(video));
    config.session_defaults.search_news_tool_model = Some(ToolModelTarget::Alias(news));
    Ok(())
}

fn add_openai_image_model(config: &mut StellaclawConfig) -> Result<()> {
    println!();
    let alias = prompt_alias(config, "Image generation model alias", "image_generation")?;
    let model_name = prompt_text("OpenAI image model name", Some("gpt-image-1"))?;
    let api_key_env = prompt_text("API key environment variable", Some("OPENAI_API_KEY"))?;
    let url = prompt_text("Endpoint base URL", Some("https://api.openai.com/v1"))?;
    let model = base_model(
        ProviderType::OpenAiImageEdit,
        model_name,
        url,
        api_key_env,
        vec![ModelCapability::ImageIn, ModelCapability::ImageOut],
        32_768,
    );
    insert_model(config, alias.clone(), model);
    config.session_defaults.image_generation_tool_model = Some(ToolModelTarget::Alias(alias));
    Ok(())
}

fn configure_memory(config: &mut StellaclawConfig) -> Result<()> {
    loop {
        println!();
        println!("{}", stage_title("Stage 3/5: Memory System"));
        println!(
            "{}",
            dim("Memory stores durable user, public, and conversation facts separately from chat history.")
        );
        println!(
            "{}",
            dim("For dedupe and user-memory compaction, choose a cheap reliable chat model when possible.")
        );
        println!(
            "{}",
            dim("Leaving model fields unset uses local fallback behavior.")
        );
        print_memory_summary(config);
        let choice = prompt_menu(
            "Choose an action",
            &[
                "Enable Memory System",
                "Disable Memory System",
                "Select cheap dedupe model",
                "Select cheap user compaction model",
                "Continue",
            ],
        )?;
        match choice {
            0 => {
                config.memory.enabled = true;
                if config.memory.dedupe_model_alias.is_none()
                    && prompt_bool("Select a cheap dedupe model now", true)?
                {
                    config.memory.dedupe_model_alias =
                        prompt_optional_memory_model(config, "Cheap dedupe model")?;
                }
                if config.memory.user_compaction_model_alias.is_none()
                    && prompt_bool("Select a cheap user compaction model now", true)?
                {
                    config.memory.user_compaction_model_alias =
                        prompt_optional_memory_model(config, "Cheap user compaction model")?;
                }
            }
            1 => {
                config.memory.enabled = false;
                config.memory.dedupe_model_alias = None;
                config.memory.user_compaction_model_alias = None;
            }
            2 => {
                config.memory.dedupe_model_alias =
                    prompt_optional_memory_model(config, "Cheap dedupe model")?;
            }
            3 => {
                config.memory.user_compaction_model_alias =
                    prompt_optional_memory_model(config, "Cheap user compaction model")?;
            }
            4 => {
                if config.memory.enabled
                    && (config.memory.dedupe_model_alias.is_none()
                        || config.memory.user_compaction_model_alias.is_none())
                {
                    println!(
                        "{}",
                        yellow("Memory is enabled without one or more cheap model aliases; local fallback will be used for those tasks.")
                    );
                }
                return Ok(());
            }
            _ => unreachable!(),
        }
    }
}

fn configure_channels(config: &mut StellaclawConfig) -> Result<()> {
    loop {
        println!();
        println!("{}", stage_title("Stage 4/5: Channel configuration"));
        print_channel_summary(config);
        let choice = prompt_menu(
            "Choose an action",
            &[
                "Add Web channel",
                "Add Telegram channel",
                "Remove channel",
                "Continue",
            ],
        )?;
        match choice {
            0 => add_web_channel(config)?,
            1 => add_telegram_channel(config)?,
            2 => remove_channel(config)?,
            3 if config.channels.is_empty() => {
                println!("{}", yellow("Add at least one channel before continuing."));
            }
            3 => return Ok(()),
            _ => unreachable!(),
        }
    }
}

fn add_web_channel(config: &mut StellaclawConfig) -> Result<()> {
    let id = prompt_channel_id(config, "Web channel id", "web-main")?;
    let bind_addr = prompt_text("Bind address", Some("127.0.0.1:3111"))?;
    let token_env = prompt_text(
        "Authorization token environment variable",
        Some("STELLACLAW_WEB_TOKEN"),
    )?;
    config.channels.push(ChannelConfig::Web(WebChannelConfig {
        id,
        bind_addr,
        token_env,
    }));
    Ok(())
}

fn add_telegram_channel(config: &mut StellaclawConfig) -> Result<()> {
    let id = prompt_channel_id(config, "Telegram channel id", "telegram-main")?;
    let use_env = prompt_bool("Read bot token from environment variable", true)?;
    let (bot_token, bot_token_env) = if use_env {
        (
            None,
            prompt_text("Bot token environment variable", Some("TELEGRAM_BOT_TOKEN"))?,
        )
    } else {
        (
            Some(prompt_text("Bot token", None)?),
            "TELEGRAM_BOT_TOKEN".to_string(),
        )
    };
    let api_base_url = prompt_text("Telegram API base URL", Some("https://api.telegram.org"))?;
    config
        .channels
        .push(ChannelConfig::Telegram(TelegramChannelConfig {
            id,
            bot_token,
            bot_token_env,
            api_base_url,
            poll_timeout_seconds: 30,
            poll_interval_ms: 250,
            admin_user_ids: Vec::new(),
        }));
    Ok(())
}

fn confirm_and_create(
    args: &SetupArgs,
    config_path: &Path,
    workdir: &Path,
    config: &mut StellaclawConfig,
) -> Result<()> {
    refresh_model_defaults(config);
    config.validate().map_err(anyhow::Error::msg)?;

    println!();
    println!("{}", stage_title("Stage 5/5: Confirm and create"));
    print_model_summary(config);
    print_tooling_summary(config);
    print_memory_summary(config);
    print_channel_summary(config);
    println!("{} {}", dim("Config file:"), config_path.display());
    println!("{} {}", dim("Workdir:"), workdir.display());
    if args.install_systemd {
        println!(
            "{} {} service",
            dim("systemd:"),
            cyan(if args.systemd_user { "user" } else { "system" })
        );
    } else {
        println!("{} disabled", dim("systemd:"));
    }
    println!();

    if args.dry_run {
        let serialized =
            serde_json::to_string_pretty(config).context("failed to serialize setup config")?;
        println!("{}", bold("Dry-run config preview:"));
        println!("{serialized}");
        if args.install_systemd {
            let exe = env::current_exe().context("failed to resolve current executable")?;
            let unit = systemd_unit(&exe, config_path, workdir, args.systemd_user)?;
            println!();
            println!("{}", bold("Dry-run systemd unit preview:"));
            println!("{unit}");
        }
        println!("{}", green("Dry run complete. No files were written."));
        return Ok(());
    }

    if !prompt_bool("Create this setup", true)? {
        return Err(anyhow!("setup cancelled"));
    }
    if config_path.exists() && !prompt_bool("Config file exists. Overwrite it", false)? {
        return Err(anyhow!("setup cancelled because config exists"));
    }

    fs::create_dir_all(workdir)
        .with_context(|| format!("failed to create workdir {}", workdir.display()))?;
    if let Some(parent) = config_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }
    let serialized =
        serde_json::to_string_pretty(config).context("failed to serialize setup config")?;
    fs::write(config_path, serialized)
        .with_context(|| format!("failed to write config {}", config_path.display()))?;

    if args.install_systemd {
        install_systemd_unit(args.systemd_user, config_path, workdir)?;
    }

    println!();
    println!(
        "{}",
        green(&format!("Created config at {}", config_path.display()))
    );
    println!(
        "{}",
        green(&format!("Created workdir at {}", workdir.display()))
    );
    if !args.install_systemd {
        println!(
            "{} stellaclaw --config {} --workdir {}",
            dim("Run:"),
            config_path.display(),
            workdir.display()
        );
    }
    Ok(())
}

fn base_model(
    provider_type: ProviderType,
    model_name: String,
    url: String,
    api_key_env: String,
    capabilities: Vec<ModelCapability>,
    token_max_context: u64,
) -> ModelConfig {
    ModelConfig {
        provider_type,
        model_name,
        url,
        api_key_env,
        capabilities,
        token_max_context,
        max_tokens: 0,
        cache_timeout: 300,
        idle_timeout_compact_enabled: true,
        conn_timeout: 2,
        request_timeout: 600,
        max_request_size: default_max_request_size(),
        retry_mode: RetryMode::default(),
        reasoning: None,
        token_estimator_type: TokenEstimatorType::Local,
        multimodal_estimator: None,
        multimodal_input: None,
        token_estimator_url: None,
    }
}

fn search_model(
    provider_type: ProviderType,
    model_name: &str,
    url: &str,
    api_key_env: &str,
) -> ModelConfig {
    base_model(
        provider_type,
        model_name.to_string(),
        url.to_string(),
        api_key_env.to_string(),
        vec![ModelCapability::WebSearch],
        32_768,
    )
}

fn multimodal_input(image: bool, pdf: bool, audio: bool) -> MultimodalInputConfig {
    MultimodalInputConfig {
        image: image.then(|| MediaInputConfig {
            transport: MediaInputTransport::InlineBase64,
            supported_media_types: vec![
                "image/png".to_string(),
                "image/jpeg".to_string(),
                "image/webp".to_string(),
            ],
            max_width: Some(4096),
            max_height: Some(4096),
        }),
        pdf: pdf.then(|| MediaInputConfig {
            transport: MediaInputTransport::InlineBase64,
            supported_media_types: vec!["application/pdf".to_string()],
            max_width: None,
            max_height: None,
        }),
        audio: audio.then(|| MediaInputConfig {
            transport: MediaInputTransport::InlineBase64,
            supported_media_types: vec![
                "audio/mpeg".to_string(),
                "audio/wav".to_string(),
                "audio/webm".to_string(),
                "audio/flac".to_string(),
                "audio/mp4".to_string(),
            ],
            max_width: None,
            max_height: None,
        }),
    }
}

fn insert_model(config: &mut StellaclawConfig, alias: String, model: ModelConfig) {
    let is_chat = model.supports(ModelCapability::Chat);
    config.models.insert(alias.clone(), model);
    if is_chat && !config.available_agent_models.contains(&alias) {
        config.available_agent_models.push(alias);
    }
    refresh_model_defaults(config);
}

fn refresh_model_defaults(config: &mut StellaclawConfig) {
    config.available_agent_models.retain(|alias| {
        config
            .models
            .get(alias)
            .is_some_and(|model| model.supports(ModelCapability::Chat))
    });
    if config.available_agent_models.is_empty() {
        config.available_agent_models = config
            .models
            .iter()
            .filter_map(|(alias, model)| {
                model.supports(ModelCapability::Chat).then(|| alias.clone())
            })
            .collect();
    }
    config.default_profile =
        config
            .available_agent_models
            .first()
            .cloned()
            .map(|alias| SessionProfile {
                main_model: ModelSelection::alias(alias),
            });
}

fn remove_model(config: &mut StellaclawConfig) -> Result<()> {
    if config.models.is_empty() {
        println!("{}", yellow("No models to remove."));
        return Ok(());
    }
    let aliases: Vec<String> = config.models.keys().cloned().collect();
    let mut options = aliases.clone();
    options.push("Cancel".to_string());
    let option_refs: Vec<&str> = options.iter().map(String::as_str).collect();
    let choice = prompt_menu("Remove which model", &option_refs)?;
    if choice >= aliases.len() {
        return Ok(());
    }
    let alias = &aliases[choice];
    config.models.remove(alias);
    config.available_agent_models.retain(|value| value != alias);
    clear_tool_target_for_alias(config, alias);
    refresh_model_defaults(config);
    Ok(())
}

fn clear_tool_target_for_alias(config: &mut StellaclawConfig, alias: &str) {
    for target in [
        &mut config.session_defaults.image_tool_model,
        &mut config.session_defaults.pdf_tool_model,
        &mut config.session_defaults.audio_tool_model,
        &mut config.session_defaults.image_generation_tool_model,
        &mut config.session_defaults.search_tool_model,
        &mut config.session_defaults.search_image_tool_model,
        &mut config.session_defaults.search_video_tool_model,
        &mut config.session_defaults.search_news_tool_model,
    ] {
        if target_matches_alias(target, alias) {
            *target = None;
        }
    }
}

fn target_matches_alias(target: &Option<ToolModelTarget>, alias: &str) -> bool {
    match target {
        Some(ToolModelTarget::Alias(value)) => {
            value == alias || value.split_once(':').is_some_and(|(head, _)| head == alias)
        }
        Some(ToolModelTarget::Inline(_)) | None => false,
    }
}

fn prompt_tool_target(config: &StellaclawConfig, label: &str) -> Result<Option<ToolModelTarget>> {
    let mut values: Vec<Option<String>> = vec![None];
    let mut labels = vec!["Disabled".to_string()];
    if let Some(main) = config.available_agent_models.first() {
        values.push(Some(format!("{main}:self")));
        labels.push(format!("{main}:self (reuse active session model)"));
    }
    for alias in config.models.keys() {
        values.push(Some(alias.clone()));
        labels.push(alias.clone());
    }
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let choice = prompt_menu(label, &refs)?;
    Ok(values[choice].clone().map(ToolModelTarget::Alias))
}

fn prompt_optional_memory_model(config: &StellaclawConfig, label: &str) -> Result<Option<String>> {
    let chat_models = chat_model_aliases(config);
    if chat_models.is_empty() {
        println!(
            "{}",
            yellow("No chat-capable models are available for memory tasks.")
        );
        return Ok(None);
    }

    let mut values: Vec<Option<String>> = Vec::new();
    let mut labels = Vec::new();
    for alias in chat_models {
        values.push(Some(alias.clone()));
        labels.push(format!("{alias} (recommended: cheap chat model)"));
    }
    values.push(None);
    labels.push("Unset (use local fallback)".to_string());
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let choice = prompt_menu(label, &refs)?;
    Ok(values[choice].clone())
}

fn chat_model_aliases(config: &StellaclawConfig) -> Vec<String> {
    config
        .models
        .iter()
        .filter_map(|(alias, model)| {
            model
                .supports(ModelCapability::Chat)
                .then(|| alias.to_string())
        })
        .collect()
}

fn remove_channel(config: &mut StellaclawConfig) -> Result<()> {
    if config.channels.is_empty() {
        println!("{}", yellow("No channels to remove."));
        return Ok(());
    }
    let ids: Vec<String> = config
        .channels
        .iter()
        .map(|channel| match channel {
            ChannelConfig::Telegram(channel) => format!("telegram:{}", channel.id),
            ChannelConfig::Web(channel) => format!("web:{}", channel.id),
        })
        .collect();
    let mut options = ids.clone();
    options.push("Cancel".to_string());
    let refs: Vec<&str> = options.iter().map(String::as_str).collect();
    let choice = prompt_menu("Remove which channel", &refs)?;
    if choice < config.channels.len() {
        config.channels.remove(choice);
    }
    Ok(())
}

fn print_model_summary(config: &StellaclawConfig) {
    if config.models.is_empty() {
        println!("{} {}", bold("Models:"), dim("none"));
        return;
    }
    println!("{}", bold("Models:"));
    for (alias, model) in &config.models {
        let agent = if config.available_agent_models.contains(alias) {
            format!(" {}", green("agent"))
        } else {
            String::new()
        };
        println!(
            "  - {}: {:?} {}{}",
            cyan(alias),
            model.provider_type,
            model.model_name,
            agent
        );
    }
}

fn print_tooling_summary(config: &StellaclawConfig) {
    println!("{}", bold("Tooling:"));
    println!(
        "  {} threshold={}, retain={}",
        cyan("compression:"),
        display_compression_threshold(config),
        display_compression_retain(config)
    );
    println!(
        "  {} image={:?}, pdf={:?}, audio={:?}, image_generation={:?}",
        cyan("media:"),
        display_target(&config.session_defaults.image_tool_model),
        display_target(&config.session_defaults.pdf_tool_model),
        display_target(&config.session_defaults.audio_tool_model),
        display_target(&config.session_defaults.image_generation_tool_model)
    );
    println!(
        "  {} web={:?}, image={:?}, video={:?}, news={:?}",
        cyan("search:"),
        display_target(&config.session_defaults.search_tool_model),
        display_target(&config.session_defaults.search_image_tool_model),
        display_target(&config.session_defaults.search_video_tool_model),
        display_target(&config.session_defaults.search_news_tool_model)
    );
}

fn display_compression_threshold(config: &StellaclawConfig) -> String {
    config
        .session_defaults
        .compression_threshold_tokens
        .map(|value| format!("override {value}"))
        .unwrap_or_else(|| default_compression_threshold_label(config))
}

fn display_compression_retain(config: &StellaclawConfig) -> String {
    config
        .session_defaults
        .compression_retain_recent_tokens
        .map(|value| format!("override {value}"))
        .unwrap_or_else(|| default_compression_retain_label(config))
}

fn default_compression_threshold_label(config: &StellaclawConfig) -> String {
    runtime_compression_defaults(config)
        .map(|(alias, threshold, _retain)| format!("default {threshold} (90% of {alias} context)"))
        .unwrap_or_else(|| "default 90% of active model context".to_string())
}

fn default_compression_retain_label(config: &StellaclawConfig) -> String {
    runtime_compression_defaults(config)
        .map(|(alias, _threshold, retain)| {
            format!("default {retain} (10% of compression threshold for {alias})")
        })
        .unwrap_or_else(|| "default 10% of compression threshold".to_string())
}

fn runtime_compression_defaults(config: &StellaclawConfig) -> Option<(String, u64, u64)> {
    let alias = config.initial_main_model_name()?;
    let model = config.models.get(&alias)?;
    let threshold = model.token_max_context.saturating_mul(9) / 10;
    let retain = threshold / 10;
    Some((alias, threshold, retain))
}

fn print_memory_summary(config: &StellaclawConfig) {
    println!("{}", bold("Memory System:"));
    println!(
        "  {} {}",
        cyan("enabled:"),
        if config.memory.enabled {
            green("true")
        } else {
            dim("false")
        }
    );
    println!(
        "  {} {}",
        cyan("dedupe model:"),
        display_optional_alias(config.memory.dedupe_model_alias.as_deref())
    );
    println!(
        "  {} {}",
        cyan("user compaction model:"),
        display_optional_alias(config.memory.user_compaction_model_alias.as_deref())
    );
}

fn display_optional_alias(value: Option<&str>) -> String {
    value
        .map(cyan)
        .unwrap_or_else(|| dim("local fallback / unset"))
}

fn print_channel_summary(config: &StellaclawConfig) {
    if config.channels.is_empty() {
        println!("{} {}", bold("Channels:"), dim("none"));
        return;
    }
    println!("{}", bold("Channels:"));
    for channel in &config.channels {
        match channel {
            ChannelConfig::Telegram(channel) => {
                println!(
                    "  - {}:{} token_env={}",
                    cyan("telegram"),
                    channel.id,
                    channel.bot_token_env
                );
            }
            ChannelConfig::Web(channel) => {
                println!(
                    "  - {}:{} bind={}",
                    cyan("web"),
                    channel.id,
                    channel.bind_addr
                );
            }
        }
    }
}

fn display_target(target: &Option<ToolModelTarget>) -> Option<String> {
    match target {
        Some(ToolModelTarget::Alias(alias)) => Some(alias.clone()),
        Some(ToolModelTarget::Inline(model)) => Some(model.model_name.clone()),
        None => None,
    }
}

fn prompt_alias(config: &StellaclawConfig, label: &str, default: &str) -> Result<String> {
    loop {
        let value = prompt_text(label, Some(&next_available_alias(config, default)))?;
        if is_valid_id(&value) && !config.models.contains_key(&value) {
            return Ok(value);
        }
        println!(
            "{}",
            yellow("Alias must be unique and contain only ASCII letters, digits, '_' or '-'.")
        );
    }
}

fn prompt_channel_id(config: &StellaclawConfig, label: &str, default: &str) -> Result<String> {
    loop {
        let value = prompt_text(label, Some(&next_available_channel_id(config, default)))?;
        if is_valid_id(&value) && !channel_id_exists(config, &value) {
            return Ok(value);
        }
        println!(
            "{}",
            yellow("Channel id must be unique and contain only ASCII letters, digits, '_' or '-'.")
        );
    }
}

fn next_available_alias(config: &StellaclawConfig, base: &str) -> String {
    next_available(base, |candidate| config.models.contains_key(candidate))
}

fn next_available_channel_id(config: &StellaclawConfig, base: &str) -> String {
    next_available(base, |candidate| channel_id_exists(config, candidate))
}

fn next_available(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    for index in 2..1000 {
        let candidate = format!("{base}-{index}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    format!("{base}-{}", std::process::id())
}

fn channel_id_exists(config: &StellaclawConfig, id: &str) -> bool {
    config.channels.iter().any(|channel| match channel {
        ChannelConfig::Telegram(channel) => channel.id == id,
        ChannelConfig::Web(channel) => channel.id == id,
    })
}

fn is_valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
}

fn prompt_menu(prompt: &str, options: &[&str]) -> Result<usize> {
    loop {
        println!("{}:", bold(prompt));
        for (index, option) in options.iter().enumerate() {
            println!("  {} {option}", cyan(&format!("{}.", index + 1)));
        }
        print!("{} ", cyan(">"));
        io::stdout().flush()?;
        let input = read_line()?;
        if let Ok(value) = input.parse::<usize>() {
            if (1..=options.len()).contains(&value) {
                return Ok(value - 1);
            }
        }
        println!(
            "{}",
            yellow(&format!("Enter a number from 1 to {}.", options.len()))
        );
    }
}

fn prompt_text(prompt: &str, default: Option<&str>) -> Result<String> {
    loop {
        match default {
            Some(default) => print!("{} {}: ", cyan(prompt), dim(&format!("[{default}]"))),
            None => print!("{}: ", cyan(prompt)),
        }
        io::stdout().flush()?;
        let input = read_line()?;
        let value = if input.is_empty() {
            default.unwrap_or("").to_string()
        } else {
            input
        };
        if !value.trim().is_empty() {
            return Ok(value.trim().to_string());
        }
        println!("{}", yellow("Value must not be empty."));
    }
}

fn prompt_bool(prompt: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    loop {
        print!("{} {}: ", cyan(prompt), dim(&format!("[{suffix}]")));
        io::stdout().flush()?;
        let input = read_line()?.to_ascii_lowercase();
        if input.is_empty() {
            return Ok(default);
        }
        match input.as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => println!("{}", yellow("Enter y or n.")),
        }
    }
}

fn prompt_u64(prompt: &str, default: u64) -> Result<u64> {
    loop {
        let value = prompt_text(prompt, Some(&default.to_string()))?;
        match value.parse::<u64>() {
            Ok(value) if value > 0 => return Ok(value),
            _ => println!("{}", yellow("Enter a positive integer.")),
        }
    }
}

fn prompt_optional_u64_with_label(
    prompt: &str,
    current: Option<u64>,
    default_label: &str,
) -> Result<Option<u64>> {
    loop {
        print!(
            "{} {}: ",
            cyan(prompt),
            dim(&format!(
                "[{default_label}, blank keeps default, 0 clears override]"
            ))
        );
        io::stdout().flush()?;
        let input = read_line()?;
        if input.is_empty() {
            return Ok(current);
        }
        match input.parse::<u64>() {
            Ok(0) => return Ok(None),
            Ok(value) => return Ok(Some(value)),
            Err(_) => println!("{}", yellow("Enter a positive integer or 0.")),
        }
    }
}

fn read_line() -> Result<String> {
    let mut input = String::new();
    if io::stdin().read_line(&mut input)? == 0 {
        return Err(anyhow!("setup input closed"));
    }
    Ok(input.trim().to_string())
}

fn ensure_config_target_writable(path: &Path) -> Result<()> {
    if path.exists() {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .with_context(|| format!("config file is not writable: {}", path.display()))?;
        return Ok(());
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    ensure_directory_writable(parent, "config directory")
}

fn ensure_directory_writable(path: &Path, label: &str) -> Result<()> {
    fs::create_dir_all(path)
        .with_context(|| format!("failed to create {label} {}", path.display()))?;
    let probe = path.join(format!(
        ".stellaclaw-setup-write-test-{}",
        std::process::id()
    ));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .with_context(|| format!("{label} is not writable: {}", path.display()))?;
    fs::remove_file(&probe)
        .with_context(|| format!("failed to remove write probe {}", probe.display()))?;
    Ok(())
}

fn ensure_systemd_target_writable(user: bool) -> Result<()> {
    if !cfg!(target_os = "linux") {
        return Err(anyhow!("--systemd is only supported on Linux"));
    }
    let unit_dir = systemd_unit_dir(user)?;
    ensure_directory_writable(&unit_dir, "systemd unit directory")
}

fn install_systemd_unit(user: bool, config_path: &Path, workdir: &Path) -> Result<()> {
    let unit_dir = systemd_unit_dir(user)?;
    fs::create_dir_all(&unit_dir)
        .with_context(|| format!("failed to create {}", unit_dir.display()))?;
    let unit_path = unit_dir.join("stellaclaw.service");
    let exe = env::current_exe().context("failed to resolve current executable")?;
    let unit = systemd_unit(&exe, config_path, workdir, user)?;
    fs::write(&unit_path, unit)
        .with_context(|| format!("failed to write systemd unit {}", unit_path.display()))?;

    let mut daemon_reload = Command::new("systemctl");
    if user {
        daemon_reload.arg("--user");
    }
    run_command(daemon_reload.arg("daemon-reload"))?;

    let mut enable = Command::new("systemctl");
    if user {
        enable.arg("--user");
    }
    run_command(enable.args(["enable", "--now", "stellaclaw.service"]))?;
    println!(
        "{}",
        green(&format!(
            "Installed systemd unit at {}",
            unit_path.display()
        ))
    );
    Ok(())
}

fn run_command(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to run {:?}", command))?;
    if !status.success() {
        return Err(anyhow!("{:?} exited with status {status}", command));
    }
    Ok(())
}

fn systemd_unit_dir(user: bool) -> Result<PathBuf> {
    if user {
        if let Ok(value) = env::var("XDG_CONFIG_HOME") {
            return Ok(PathBuf::from(value).join("systemd").join("user"));
        }
        let home = env::var("HOME").context("HOME is required for --systemd --user")?;
        return Ok(PathBuf::from(home)
            .join(".config")
            .join("systemd")
            .join("user"));
    }
    Ok(PathBuf::from("/etc/systemd/system"))
}

fn systemd_unit(exe: &Path, config_path: &Path, workdir: &Path, user: bool) -> Result<String> {
    let wanted_by = if user {
        "default.target"
    } else {
        "multi-user.target"
    };
    let cwd = env::current_dir().context("failed to resolve current directory")?;
    Ok(format!(
        "[Unit]\n\
Description=Stellaclaw Agent Server\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={}\n\
ExecStart={} --config {} --workdir {}\n\
Restart=on-failure\n\
RestartSec=5\n\
\n\
[Install]\n\
WantedBy={wanted_by}\n",
        systemd_quote(cwd.as_path()),
        systemd_quote(exe),
        systemd_quote(config_path),
        systemd_quote(workdir)
    ))
}

fn systemd_quote(path: &Path) -> String {
    let raw = path.display().to_string();
    if raw
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '/' | '.' | '_' | '-' | ':'))
    {
        return raw;
    }
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn absolute_path(path: &Path) -> Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    Ok(env::current_dir()
        .context("failed to resolve current directory")?
        .join(path))
}

fn codex_auth_json_exists() -> bool {
    codex_auth_json_paths().iter().any(|path| path.is_file())
}

fn codex_auth_json_paths() -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for env_name in ["CODEX_AUTH_JSON", "CHATGPT_AUTH_JSON"] {
        if let Ok(path) = env::var(env_name) {
            paths.push(PathBuf::from(path));
        }
    }
    if let Ok(home) = env::var("CODEX_HOME") {
        paths.push(PathBuf::from(home).join("auth.json"));
    }
    if let Ok(home) = env::var("HOME") {
        paths.push(PathBuf::from(home).join(".codex").join("auth.json"));
    }
    paths
}

fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    for dir in env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_systemd_paths_with_spaces() {
        let quoted = systemd_quote(Path::new("/tmp/Stella Claw/stellaclaw"));
        assert_eq!(quoted, "\"/tmp/Stella Claw/stellaclaw\"");
    }

    #[test]
    fn default_config_valid_after_minimum_setup() {
        let mut config = empty_config();
        insert_model(
            &mut config,
            "main".to_string(),
            base_model(
                ProviderType::CodexSubscription,
                "gpt-5.5".to_string(),
                "https://chatgpt.com/backend-api/codex/responses".to_string(),
                "CHATGPT_ACCESS_TOKEN".to_string(),
                vec![ModelCapability::Chat],
                400_000,
            ),
        );
        config.channels.push(ChannelConfig::Web(WebChannelConfig {
            id: "web-main".to_string(),
            bind_addr: "127.0.0.1:3111".to_string(),
            token_env: "STELLACLAW_WEB_TOKEN".to_string(),
        }));

        config.validate().expect("setup config should validate");
        assert_eq!(config.available_agent_models, vec!["main"]);
        assert!(config.default_profile.is_some());
    }
}
