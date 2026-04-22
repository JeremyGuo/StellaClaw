use crate::config::{ExternalWebSearchConfig, RemoteWorkpathConfig, UpstreamConfig};
use crate::llm::{create_chat_completion, create_image_generation};
use crate::message::ChatMessage;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use image::{ImageFormat, ImageReader};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{Cursor, Read, Write};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolWorkerJob {
    WebFetch {
        url: String,
        max_chars: usize,
        headers: Map<String, Value>,
    },
    WebSearch {
        search_config: ExternalWebSearchConfig,
        query: String,
        max_results: usize,
        images: Vec<String>,
    },
    Image {
        image_id: String,
        path: String,
        question: String,
        upstream: UpstreamConfig,
        status_path: String,
    },
    Pdf {
        path: String,
        question: String,
        upstream: UpstreamConfig,
        images: Vec<String>,
    },
    Audio {
        path: String,
        question: String,
        upstream: UpstreamConfig,
        images: Vec<String>,
    },
    ImageGenerate {
        prompt: String,
        upstream: UpstreamConfig,
        output_path: String,
        images: Vec<String>,
    },
    FileDownload {
        download_id: String,
        url: String,
        path: String,
        temp_path: String,
        headers: Map<String, Value>,
        status_path: String,
    },
    ShellSession {
        session_id: String,
        #[serde(default)]
        interactive: bool,
        #[serde(default)]
        remote: Option<String>,
        initial_cwd: String,
        status_path: String,
        requests_dir: String,
        output_root: String,
    },
    Dsl {
        dsl_id: String,
        #[serde(default)]
        label: Option<String>,
        code: String,
        upstream: UpstreamConfig,
        #[serde(default)]
        available_upstreams: std::collections::BTreeMap<String, UpstreamConfig>,
        workspace_root: String,
        runtime_state_root: String,
        remote_workpaths: Vec<RemoteWorkpathConfig>,
        status_path: String,
        result_path: String,
        max_runtime_seconds: u64,
        max_llm_calls: u64,
        max_tool_calls: u64,
        max_emit_calls: u64,
    },
}

pub fn run_job_file(job_file: &Path) -> Result<()> {
    let raw = fs::read_to_string(job_file)
        .with_context(|| format!("failed to read worker job file {}", job_file.display()))?;
    let job: ToolWorkerJob =
        serde_json::from_str(&raw).context("failed to parse worker job file")?;
    run_job(job)
}

fn run_job(job: ToolWorkerJob) -> Result<()> {
    match job {
        ToolWorkerJob::WebFetch {
            url,
            max_chars,
            headers,
        } => {
            let result = run_web_fetch(&url, max_chars, headers)?;
            write_json_stdout(&result)
        }
        ToolWorkerJob::WebSearch {
            search_config,
            query,
            max_results,
            images,
        } => {
            let result = run_web_search(search_config, &query, max_results, &images)?;
            write_json_stdout(&result)
        }
        ToolWorkerJob::Image {
            image_id,
            path,
            question,
            upstream,
            status_path,
        } => run_image_job(
            &image_id,
            &path,
            &question,
            upstream,
            Path::new(&status_path),
        ),
        ToolWorkerJob::Pdf {
            path,
            question,
            upstream,
            images,
        } => {
            let result = run_pdf_job(&path, &question, upstream, &images)?;
            write_json_stdout(&result)
        }
        ToolWorkerJob::Audio {
            path,
            question,
            upstream,
            images,
        } => {
            let result = run_audio_job(&path, &question, upstream, &images)?;
            write_json_stdout(&result)
        }
        ToolWorkerJob::ImageGenerate {
            prompt,
            upstream,
            output_path,
            images,
        } => {
            let result =
                run_image_generate_job(&prompt, upstream, Path::new(&output_path), &images)?;
            write_json_stdout(&result)
        }
        ToolWorkerJob::FileDownload {
            download_id,
            url,
            path,
            temp_path,
            headers,
            status_path,
        } => run_file_download_job(
            &download_id,
            &url,
            Path::new(&path),
            Path::new(&temp_path),
            headers,
            Path::new(&status_path),
        ),
        ToolWorkerJob::ShellSession {
            session_id,
            interactive,
            remote,
            initial_cwd,
            status_path,
            requests_dir,
            output_root,
        } => run_shell_session_job(
            &session_id,
            interactive,
            remote.as_deref(),
            &initial_cwd,
            Path::new(&status_path),
            Path::new(&requests_dir),
            Path::new(&output_root),
        ),
        ToolWorkerJob::Dsl {
            dsl_id,
            label,
            code,
            upstream,
            available_upstreams,
            workspace_root,
            runtime_state_root,
            remote_workpaths,
            status_path,
            result_path,
            max_runtime_seconds,
            max_llm_calls,
            max_tool_calls,
            max_emit_calls,
        } => crate::tooling::dsl::run_dsl_worker_job(
            &dsl_id,
            label,
            &code,
            upstream,
            available_upstreams,
            Path::new(&workspace_root),
            Path::new(&runtime_state_root),
            &remote_workpaths,
            Path::new(&status_path),
            Path::new(&result_path),
            max_runtime_seconds,
            max_llm_calls,
            max_tool_calls,
            max_emit_calls,
        ),
    }
}

fn write_json_stdout(value: &Value) -> Result<()> {
    let stdout = std::io::stdout();
    let mut handle = stdout.lock();
    serde_json::to_writer(&mut handle, value).context("failed to serialize worker output")?;
    handle
        .write_all(b"\n")
        .context("failed to flush worker output newline")?;
    handle.flush().context("failed to flush worker output")?;
    Ok(())
}

fn write_json_file(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(
        path,
        serde_json::to_vec_pretty(value).context("failed to serialize worker status")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

fn should_bypass_proxy(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    match parsed.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false),
        None => false,
    }
}

fn build_http_client(url: &str, timeout_seconds: Option<f64>) -> Result<reqwest::blocking::Client> {
    let mut builder = reqwest::blocking::Client::builder();
    if let Some(timeout_seconds) = timeout_seconds {
        builder = builder.timeout(std::time::Duration::from_secs_f64(timeout_seconds));
    }
    if should_bypass_proxy(url) {
        builder = builder.no_proxy();
    }
    builder.build().context("failed to construct http client")
}

fn run_web_fetch(url: &str, max_chars: usize, headers: Map<String, Value>) -> Result<Value> {
    let client = build_http_client(url, None)?;
    let mut request = client.get(url);
    for (key, value) in headers {
        if let Some(value) = value.as_str() {
            request = request.header(&key, value);
        }
    }
    let response = request.send().context("web fetch failed")?;
    let status = response.status().as_u16();
    let final_url = response.url().to_string();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = response.text().context("failed to read fetched body")?;
    let cleaned = if content_type.contains("html") {
        strip_html_tags(&body)
    } else {
        body
    };
    let truncated = cleaned.chars().count() > max_chars;
    let content = cleaned.chars().take(max_chars).collect::<String>();
    Ok(json!({
        "status": status,
        "url": final_url,
        "content_type": content_type,
        "content": content,
        "truncated": truncated
    }))
}

fn run_web_search(
    search_config: ExternalWebSearchConfig,
    query: &str,
    max_results: usize,
    images: &[String],
) -> Result<Value> {
    let request_spec = build_web_search_request_spec(&search_config, query, max_results, images)?;
    let client = build_http_client(&request_spec.url, Some(search_config.timeout_seconds))?;
    let mut request = client.request(request_spec.method.clone(), &request_spec.url);
    if !request_spec.query.is_empty() {
        request = request.query(&request_spec.query);
    }
    if let Some(body) = request_spec.json_body.as_ref() {
        request = request.json(body);
    }
    if let Some((name, value)) = request_spec.auth_header.as_ref() {
        request = request.header(name, value);
    }
    for (key, value) in &request_spec.headers {
        request = request.header(key, value);
    }
    let response = request.send().context("web search request failed")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read web search response")?;
    if !status.is_success() {
        return Err(anyhow::anyhow!(
            "web search upstream failed with {}: {}",
            status,
            body
        ));
    }
    let value: Value =
        serde_json::from_str(&body).context("failed to parse web search response")?;
    Ok(match request_spec.provider {
        ExternalWebSearchProvider::OpenAiCompatible => {
            parse_openai_compatible_web_search_response(&value, query, max_results)
        }
        ExternalWebSearchProvider::Brave => {
            parse_brave_web_search_response(&value, query, max_results)
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExternalWebSearchProvider {
    OpenAiCompatible,
    Brave,
}

#[derive(Clone, Debug, PartialEq)]
struct WebSearchRequestSpec {
    provider: ExternalWebSearchProvider,
    method: reqwest::Method,
    url: String,
    query: Vec<(String, String)>,
    json_body: Option<Value>,
    auth_header: Option<(String, String)>,
    headers: Vec<(String, String)>,
}

fn build_web_search_request_spec(
    search_config: &ExternalWebSearchConfig,
    query: &str,
    max_results: usize,
    images: &[String],
) -> Result<WebSearchRequestSpec> {
    let request_url = build_web_search_request_url(search_config);
    let provider = detect_external_web_search_provider(search_config, &request_url);
    match provider {
        ExternalWebSearchProvider::OpenAiCompatible => {
            build_openai_compatible_web_search_request_spec(
                search_config,
                request_url,
                query,
                images,
            )
        }
        ExternalWebSearchProvider::Brave => build_brave_web_search_request_spec(
            search_config,
            request_url,
            query,
            max_results,
            images,
        ),
    }
}

fn build_web_search_request_url(search_config: &ExternalWebSearchConfig) -> String {
    let base = search_config.base_url.trim_end_matches('/');
    let path = if search_config.chat_completions_path.starts_with('/') {
        search_config.chat_completions_path.clone()
    } else {
        format!("/{}", search_config.chat_completions_path)
    };
    format!("{}{}", base, path)
}

fn detect_external_web_search_provider(
    search_config: &ExternalWebSearchConfig,
    request_url: &str,
) -> ExternalWebSearchProvider {
    let path = if search_config.chat_completions_path.starts_with('/') {
        search_config.chat_completions_path.as_str()
    } else {
        ""
    };
    let host_is_brave = reqwest::Url::parse(request_url)
        .ok()
        .and_then(|url| {
            url.host_str()
                .map(|host| host.ends_with("search.brave.com"))
        })
        .unwrap_or(false);
    if host_is_brave && (path.ends_with("/web/search") || request_url.ends_with("/web/search")) {
        ExternalWebSearchProvider::Brave
    } else {
        ExternalWebSearchProvider::OpenAiCompatible
    }
}

fn build_openai_compatible_web_search_request_spec(
    search_config: &ExternalWebSearchConfig,
    request_url: String,
    query: &str,
    images: &[String],
) -> Result<WebSearchRequestSpec> {
    let mut payload = Map::new();
    payload.insert(
        "model".to_string(),
        Value::String(search_config.model.clone()),
    );
    let mut user_content = vec![json!({
        "type": "text",
        "text": query
    })];
    append_reference_images(&mut user_content, images)?;
    payload.insert(
        "messages".to_string(),
        json!([
            {
                "role": "system",
                "content": "Search the web and answer the query. Include source URLs in the answer when available."
            },
            {
                "role": "user",
                "content": user_content
            }
        ]),
    );
    Ok(WebSearchRequestSpec {
        provider: ExternalWebSearchProvider::OpenAiCompatible,
        method: reqwest::Method::POST,
        url: request_url,
        query: Vec::new(),
        json_body: Some(Value::Object(payload)),
        auth_header: resolve_web_search_api_key(search_config)
            .map(|value| ("authorization".to_string(), format!("Bearer {value}"))),
        headers: external_web_search_headers(search_config),
    })
}

fn build_brave_web_search_request_spec(
    search_config: &ExternalWebSearchConfig,
    request_url: String,
    query: &str,
    max_results: usize,
    images: &[String],
) -> Result<WebSearchRequestSpec> {
    if !images.is_empty() {
        return Err(anyhow!(
            "the configured Brave web search provider does not support image inputs"
        ));
    }
    Ok(WebSearchRequestSpec {
        provider: ExternalWebSearchProvider::Brave,
        method: reqwest::Method::GET,
        url: request_url,
        query: vec![
            ("q".to_string(), query.to_string()),
            ("count".to_string(), max_results.clamp(1, 20).to_string()),
            ("result_filter".to_string(), "web".to_string()),
            ("text_decorations".to_string(), "false".to_string()),
            ("extra_snippets".to_string(), "true".to_string()),
        ],
        json_body: None,
        auth_header: resolve_web_search_api_key(search_config)
            .map(|value| ("x-subscription-token".to_string(), value)),
        headers: external_web_search_headers(search_config),
    })
}

fn resolve_web_search_api_key(search_config: &ExternalWebSearchConfig) -> Option<String> {
    search_config
        .api_key
        .clone()
        .or_else(|| std::env::var(&search_config.api_key_env).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn external_web_search_headers(search_config: &ExternalWebSearchConfig) -> Vec<(String, String)> {
    search_config
        .headers
        .iter()
        .filter_map(|(key, value)| value.as_str().map(|value| (key.clone(), value.to_string())))
        .collect()
}

fn parse_openai_compatible_web_search_response(
    value: &Value,
    query: &str,
    max_results: usize,
) -> Value {
    let answer = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .map(chat_message_content_to_text)
        .unwrap_or_default();
    let citations = value
        .get("citations")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .take(max_results)
        .collect::<Vec<_>>();
    json!({
        "query": query,
        "answer": answer,
        "citations": citations,
    })
}

fn parse_brave_web_search_response(value: &Value, query: &str, max_results: usize) -> Value {
    let results = value
        .get("web")
        .and_then(|web| web.get("results"))
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(brave_web_result_summary)
                .take(max_results)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let citations = results
        .iter()
        .filter_map(|result| result.get("url").cloned())
        .collect::<Vec<_>>();
    let answer = if results.is_empty() {
        "No web results returned.".to_string()
    } else {
        results
            .iter()
            .enumerate()
            .map(|(index, result)| {
                let title = result
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("Untitled result");
                let url = result.get("url").and_then(Value::as_str).unwrap_or("");
                let description = result
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let extra = result
                    .get("extra_snippets")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(" | ")
                    })
                    .filter(|value| !value.is_empty());

                let mut lines = vec![format!("{}. {}", index + 1, title)];
                if !url.is_empty() {
                    lines.push(format!("URL: {url}"));
                }
                if !description.is_empty() {
                    lines.push(format!("Snippet: {description}"));
                }
                if let Some(extra) = extra {
                    lines.push(format!("Extra snippets: {extra}"));
                }
                lines.join("\n")
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    json!({
        "query": query,
        "answer": answer,
        "citations": citations,
        "results": results,
    })
}

fn brave_web_result_summary(result: &Value) -> Option<Value> {
    let result = result.as_object()?;
    let title = result
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let url = result
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let description = result
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if title.is_empty() && url.is_empty() && description.is_empty() {
        return None;
    }

    let mut summary = Map::new();
    if !title.is_empty() {
        summary.insert("title".to_string(), Value::String(title.to_string()));
    }
    if !url.is_empty() {
        summary.insert("url".to_string(), Value::String(url.to_string()));
    }
    if !description.is_empty() {
        summary.insert(
            "description".to_string(),
            Value::String(description.to_string()),
        );
    }
    if let Some(extra_snippets) = result.get("extra_snippets").and_then(Value::as_array) {
        let extra_snippets = extra_snippets
            .iter()
            .filter_map(Value::as_str)
            .map(|value| value.trim())
            .filter(|value| !value.is_empty())
            .map(|value| Value::String(value.to_string()))
            .collect::<Vec<_>>();
        if !extra_snippets.is_empty() {
            summary.insert("extra_snippets".to_string(), Value::Array(extra_snippets));
        }
    }
    Some(Value::Object(summary))
}

fn run_image_job(
    image_id: &str,
    path: &str,
    question: &str,
    upstream: UpstreamConfig,
    status_path: &Path,
) -> Result<()> {
    if !upstream.supports_vision_input {
        return write_json_file(
            status_path,
            &json!({
                "image_id": image_id,
                "path": path,
                "question": question,
                "running": false,
                "completed": false,
                "cancelled": false,
                "failed": true,
                "error": "the configured upstream model does not support multimodal image input",
            }),
        );
    }
    let data_url = image_to_data_url(Path::new(path))?;
    let outcome = create_chat_completion(
        &upstream,
        &[
            ChatMessage::text(
                "system",
                "You inspect a local image for an agent runtime. Answer the user's question about the image directly and concisely. If relevant visible text appears in the image, quote or transcribe it accurately.",
            ),
            ChatMessage {
                role: "user".to_string(),
                content: Some(Value::Array(vec![
                    json!({
                        "type": "text",
                        "text": question
                    }),
                    json!({
                        "type": "image_url",
                        "image_url": {
                            "url": data_url
                        }
                    }),
                ])),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        &[],
        Some(Map::from_iter([(
            "max_completion_tokens".to_string(),
            Value::from(800_u64),
        )])),
        None,
    )?;
    write_json_file(
        status_path,
        &json!({
            "image_id": image_id,
            "path": path,
            "question": question,
            "running": false,
            "completed": true,
            "cancelled": false,
            "failed": false,
            "answer": chat_message_text(&outcome.message),
        }),
    )
}

fn run_pdf_job(
    path: &str,
    question: &str,
    upstream: UpstreamConfig,
    images: &[String],
) -> Result<Value> {
    let base64_data = file_to_base64(Path::new(path))?;
    let filename = Path::new(path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("document.pdf")
        .to_string();
    let mut user_content = vec![
        json!({
            "type": "text",
            "text": question
        }),
        json!({
            "type": "file",
            "file": {
                "file_data": base64_data,
                "filename": filename,
            }
        }),
    ];
    append_reference_images(&mut user_content, images)?;
    let outcome = create_chat_completion(
        &upstream,
        &[
            ChatMessage::text(
                "system",
                "You inspect a local PDF document for an agent runtime. Answer the user's question directly and concisely. Quote exact text when relevant and note uncertainty if the PDF content is ambiguous.",
            ),
            ChatMessage {
                role: "user".to_string(),
                content: Some(Value::Array(user_content)),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        &[],
        Some(Map::from_iter([(
            "max_completion_tokens".to_string(),
            Value::from(1_200_u64),
        )])),
        None,
    )?;
    Ok(json!({
        "path": path,
        "question": question,
        "answer": chat_message_text(&outcome.message),
    }))
}

fn run_audio_job(
    path: &str,
    question: &str,
    upstream: UpstreamConfig,
    images: &[String],
) -> Result<Value> {
    let format = infer_audio_format(Path::new(path))
        .ok_or_else(|| anyhow!("unsupported audio format for audio tool: {}", path))?;
    let base64_data = file_to_base64(Path::new(path))?;
    let mut user_content = vec![
        json!({
            "type": "text",
            "text": question
        }),
        json!({
            "type": "input_audio",
            "input_audio": {
                "data": base64_data,
                "format": format,
            }
        }),
    ];
    append_reference_images(&mut user_content, images)?;
    let outcome = create_chat_completion(
        &upstream,
        &[
            ChatMessage::text(
                "system",
                "You inspect a local audio clip for an agent runtime. First understand or transcribe the audio, then answer the user's question directly and concisely.",
            ),
            ChatMessage {
                role: "user".to_string(),
                content: Some(Value::Array(user_content)),
                reasoning: None,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ],
        &[],
        Some(Map::from_iter([(
            "max_completion_tokens".to_string(),
            Value::from(1_200_u64),
        )])),
        None,
    )?;
    Ok(json!({
        "path": path,
        "question": question,
        "answer": chat_message_text(&outcome.message),
    }))
}

fn run_image_generate_job(
    prompt: &str,
    upstream: UpstreamConfig,
    output_path: &Path,
    images: &[String],
) -> Result<Value> {
    let mut content = vec![json!({
        "type": "input_text",
        "text": prompt,
    })];
    append_reference_input_images(&mut content, images)?;
    let outcome = create_image_generation(
        &upstream,
        &[ChatMessage {
            role: "user".to_string(),
            content: Some(Value::Array(content)),
            reasoning: None,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
    )?;
    let image_bytes = canonicalize_generated_image_bytes_to_png(&load_generated_image_result(
        &outcome.image_reference,
    )?)?;
    if let Some(parent) = output_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    fs::write(output_path, image_bytes)
        .with_context(|| format!("failed to write {}", output_path.display()))?;
    Ok(json!({
        "prompt": prompt,
        "path": output_path.display().to_string(),
        "kind": "synthetic_user_multimodal",
        "media": [{
            "type": "input_image",
            "path": output_path.display().to_string(),
        }],
    }))
}

fn run_file_download_job(
    download_id: &str,
    url: &str,
    path: &Path,
    temp_path: &Path,
    headers: Map<String, Value>,
    status_path: &Path,
) -> Result<()> {
    let client = build_http_client(url, None)?;
    let mut request = client.get(url);
    for (key, value) in headers {
        if let Some(value) = value.as_str() {
            request = request.header(&key, value);
        }
    }
    let mut response = request.send().context("download request failed")?;
    let status = response.status();
    let final_url = response.url().to_string();
    if !status.is_success() {
        let status_text = status.to_string();
        let body = response
            .text()
            .unwrap_or_else(|_| "<unreadable error body>".to_string());
        return write_json_file(
            status_path,
            &json!({
                "download_id": download_id,
                "url": url,
                "path": path.display().to_string(),
                "running": false,
                "completed": false,
                "cancelled": false,
                "failed": true,
                "error": format!("download failed with {}: {}", status_text, body),
            }),
        );
    }
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let total_bytes = response.content_length();
    if let Some(parent) = temp_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = fs::File::create(temp_path)
        .with_context(|| format!("failed to create {}", temp_path.display()))?;
    let mut bytes_downloaded = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = response
            .read(&mut buffer)
            .context("failed to read download response body")?;
        if read == 0 {
            break;
        }
        file.write_all(&buffer[..read])
            .context("failed to write downloaded bytes")?;
        bytes_downloaded = bytes_downloaded.saturating_add(read as u64);
        write_json_file(
            status_path,
            &json!({
                "download_id": download_id,
                "url": url,
                "path": path.display().to_string(),
                "running": true,
                "completed": false,
                "cancelled": false,
                "failed": false,
                "bytes_downloaded": bytes_downloaded,
                "total_bytes": total_bytes,
                "http_status": status.as_u16(),
                "final_url": final_url,
                "content_type": content_type,
            }),
        )?;
    }
    file.flush().context("failed to flush downloaded file")?;
    fs::rename(temp_path, path).with_context(|| {
        format!(
            "failed to move {} to {}",
            temp_path.display(),
            path.display()
        )
    })?;
    write_json_file(
        status_path,
        &json!({
            "download_id": download_id,
            "url": url,
            "path": path.display().to_string(),
            "running": false,
            "completed": true,
            "cancelled": false,
            "failed": false,
            "bytes_downloaded": bytes_downloaded,
            "size_bytes": bytes_downloaded,
            "total_bytes": total_bytes,
            "http_status": status.as_u16(),
            "final_url": final_url,
            "content_type": content_type,
        }),
    )
}

fn validate_remote_host(host: &str) -> Result<String> {
    let trimmed = host.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("remote worker SSH host must not be empty"));
    }
    if trimmed == "local" {
        return Ok("local".to_string());
    }
    if matches!(trimmed, "host" | "<host>" | "<host>|local") {
        return Err(anyhow!(
            "remote must be an actual SSH host alias or local; do not pass a placeholder"
        ));
    }
    if trimmed.starts_with('-') {
        return Err(anyhow!("remote SSH host must not start with '-'"));
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(anyhow!("remote SSH host must not contain whitespace"));
    }
    if trimmed.chars().any(|ch| ch.is_control()) {
        return Err(anyhow!(
            "remote SSH host must not contain control characters"
        ));
    }
    if trimmed.chars().any(|ch| {
        matches!(
            ch,
            '\'' | '"' | '`' | '$' | ';' | '&' | '|' | '<' | '>' | '(' | ')'
        )
    }) {
        return Err(anyhow!(
            "remote SSH host must not contain shell metacharacters"
        ));
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        return Err(anyhow!("remote SSH host must not contain path separators"));
    }
    Ok(trimmed.to_string())
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn resolve_ssh_executable() -> String {
    std::env::var("AGENT_FRAME_SSH_BIN").unwrap_or_else(|_| "ssh".to_string())
}

#[derive(Debug, Clone)]
enum ShellRequest {
    Run { process_id: String, command: String },
    Input { input: String },
    Close,
}

#[derive(Clone, Debug, Default)]
struct ShellWriterState {
    stdout_path: Option<PathBuf>,
    stderr_path: Option<PathBuf>,
}

#[derive(Debug)]
enum ShellEvent {
    ProcessDone { process_id: String, exit_code: i32 },
    ReaderFailed { stream: &'static str, error: String },
}

fn shell_request_result_path(request_path: &Path) -> PathBuf {
    let Some(file_name) = request_path.file_name().and_then(|value| value.to_str()) else {
        return request_path.with_extension("result.json");
    };
    let result_name = if let Some(stripped) = file_name.strip_suffix(".json") {
        format!("{stripped}.result.json")
    } else {
        format!("{file_name}.result.json")
    };
    request_path.with_file_name(result_name)
}

fn append_output(path: &Path, bytes: &[u8]) -> Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open {}", path.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", path.display()))?;
    file.flush()
        .with_context(|| format!("failed to flush {}", path.display()))?;
    Ok(())
}

fn write_shell_session_status(
    status_path: &Path,
    session_id: &str,
    interactive: bool,
    remote: Option<&str>,
    pid: Option<u32>,
    running: bool,
    process_id: Option<&str>,
    exit_code: Option<i32>,
    command: Option<&str>,
    stdout_path: Option<&Path>,
    stderr_path: Option<&Path>,
    error: Option<&str>,
) -> Result<()> {
    write_json_file(
        status_path,
        &json!({
            "session_id": session_id,
            "interactive": interactive,
            "remote": remote.unwrap_or("local"),
            "pid": pid,
            "running": running,
            "process_id": process_id,
            "exit_code": exit_code,
            "command": command,
            "stdout_path": stdout_path.map(|value| value.display().to_string()),
            "stderr_path": stderr_path.map(|value| value.display().to_string()),
            "error": error,
        }),
    )
}

fn file_size_if_exists(path: Option<&Path>) -> u64 {
    path.and_then(|value| fs::metadata(value).ok())
        .map(|metadata| metadata.len())
        .unwrap_or(0)
}

fn wait_for_shell_output_settle(stdout_path: Option<&Path>, stderr_path: Option<&Path>) {
    let mut last_sizes = (
        file_size_if_exists(stdout_path),
        file_size_if_exists(stderr_path),
    );
    for _ in 0..4 {
        thread::sleep(Duration::from_millis(10));
        let current_sizes = (
            file_size_if_exists(stdout_path),
            file_size_if_exists(stderr_path),
        );
        if current_sizes == last_sizes {
            return;
        }
        last_sizes = current_sizes;
    }
}

fn build_persistent_shell_command() -> Command {
    #[cfg(windows)]
    {
        let shell = std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into());
        let mut command_builder = Command::new(shell);
        command_builder.arg("/Q");
        command_builder
    }
    #[cfg(not(windows))]
    {
        Command::new("sh")
    }
}

fn build_remote_persistent_shell_command(host: &str) -> Result<Command> {
    let host = validate_remote_host(host)?;
    if host == "local" {
        return Ok(build_persistent_shell_command());
    }
    let mut command_builder = Command::new(resolve_ssh_executable());
    command_builder
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg("ConnectTimeout=10")
        .arg("-T")
        .arg(host)
        .arg("sh");
    Ok(command_builder)
}

fn spawn_persistent_shell(
    remote: Option<&str>,
    initial_cwd: &Path,
) -> Result<(
    Child,
    ChildStdin,
    std::process::ChildStdout,
    std::process::ChildStderr,
)> {
    let mut command_builder = match remote {
        Some(host) if host != "local" => build_remote_persistent_shell_command(host)?,
        _ => build_persistent_shell_command(),
    };
    if remote.is_none() || remote == Some("local") {
        command_builder.current_dir(initial_cwd);
    }
    let mut child = command_builder
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "failed to spawn persistent shell in {}",
                initial_cwd.display()
            )
        })?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow!("persistent shell stdin unavailable"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("persistent shell stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("persistent shell stderr unavailable"))?;
    Ok((child, stdin, stdout, stderr))
}

fn shell_request_paths(requests_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in fs::read_dir(requests_dir)
        .with_context(|| format!("failed to read {}", requests_dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|value| value.to_str()) else {
            continue;
        };
        if file_name.starts_with("request-")
            && file_name.ends_with(".json")
            && !file_name.ends_with(".result.json")
        {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn read_shell_request(path: &Path) -> Result<ShellRequest> {
    let raw =
        fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))?;
    let value: Value = serde_json::from_str(&raw).context("failed to parse shell request")?;
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("shell request must be an object"))?;
    let action = object
        .get("action")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("shell request missing action"))?;
    match action {
        "run" => Ok(ShellRequest::Run {
            process_id: object
                .get("process_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("shell run request missing process_id"))?
                .to_string(),
            command: object
                .get("command")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("shell run request missing command"))?
                .to_string(),
        }),
        "input" => Ok(ShellRequest::Input {
            input: object
                .get("input")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("shell input request missing input"))?
                .to_string(),
        }),
        "close" => Ok(ShellRequest::Close),
        other => Err(anyhow!("unknown shell request action {}", other)),
    }
}

fn write_shell_request_result(path: &Path, value: &Value) -> Result<()> {
    fs::write(
        path,
        serde_json::to_vec_pretty(value).context("failed to serialize shell request result")?,
    )
    .with_context(|| format!("failed to write {}", path.display()))
}

fn spawn_stdout_forwarder(
    reader: std::process::ChildStdout,
    writer_state: Arc<Mutex<ShellWriterState>>,
    event_sender: mpsc::Sender<ShellEvent>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let mut reader = reader;
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(read_len) => {
                    let path = writer_state
                        .lock()
                        .map_err(|_| anyhow!("stdout writer state poisoned"))?
                        .stdout_path
                        .clone();
                    if let Some(path) = path {
                        append_output(&path, &buffer[..read_len])?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = event_sender.send(ShellEvent::ReaderFailed {
                        stream: "stdout",
                        error: error.to_string(),
                    });
                    return Err(error).context("failed to read persistent shell stdout");
                }
            }
        }
    })
}

fn parse_shell_done_marker(line: &[u8]) -> Option<(String, i32)> {
    let text = std::str::from_utf8(line)
        .ok()?
        .trim_matches(|ch| ch == '\r' || ch == '\n');
    let rest = text.strip_prefix("__AGENT_FRAME_SHELL_DONE__:")?;
    let (process_id, exit_code) = rest.rsplit_once(':')?;
    let exit_code = exit_code.parse::<i32>().ok()?;
    Some((process_id.to_string(), exit_code))
}

fn spawn_stderr_forwarder(
    reader: std::process::ChildStderr,
    writer_state: Arc<Mutex<ShellWriterState>>,
    event_sender: mpsc::Sender<ShellEvent>,
) -> thread::JoinHandle<Result<()>> {
    thread::spawn(move || -> Result<()> {
        let mut reader = reader;
        let mut pending = Vec::<u8>::new();
        let mut buffer = [0_u8; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => {
                    if !pending.is_empty() {
                        let path = writer_state
                            .lock()
                            .map_err(|_| anyhow!("stderr writer state poisoned"))?
                            .stderr_path
                            .clone();
                        if let Some(path) = path {
                            append_output(&path, &pending)?;
                        }
                    }
                    return Ok(());
                }
                Ok(read_len) => {
                    pending.extend_from_slice(&buffer[..read_len]);
                    while let Some(pos) = pending.iter().position(|byte| *byte == b'\n') {
                        let line = pending.drain(..=pos).collect::<Vec<_>>();
                        if let Some((process_id, exit_code)) = parse_shell_done_marker(&line) {
                            let _ = event_sender.send(ShellEvent::ProcessDone {
                                process_id,
                                exit_code,
                            });
                            continue;
                        }
                        let path = writer_state
                            .lock()
                            .map_err(|_| anyhow!("stderr writer state poisoned"))?
                            .stderr_path
                            .clone();
                        if let Some(path) = path {
                            append_output(&path, &line)?;
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    let _ = event_sender.send(ShellEvent::ReaderFailed {
                        stream: "stderr",
                        error: error.to_string(),
                    });
                    return Err(error).context("failed to read persistent shell stderr");
                }
            }
        }
    })
}

fn start_shell_process(
    stdin: &mut ChildStdin,
    process_id: &str,
    command: &str,
    interactive: bool,
    stdout_path: &Path,
    stderr_path: &Path,
) -> Result<()> {
    #[cfg(windows)]
    let script = if interactive {
        format!(
            "{command}\r\nset AGENT_FRAME_SHELL_EXIT=%ERRORLEVEL%\r\necho __AGENT_FRAME_SHELL_DONE__:{process_id}:%AGENT_FRAME_SHELL_EXIT% 1>&2\r\n"
        )
    } else {
        format!(
            "({command}) 1> \"{}\" 2> \"{}\"\r\nset AGENT_FRAME_SHELL_EXIT=%ERRORLEVEL%\r\necho __AGENT_FRAME_SHELL_DONE__:{process_id}:%AGENT_FRAME_SHELL_EXIT% 1>&2\r\n",
            stdout_path.display(),
            stderr_path.display()
        )
    };
    #[cfg(not(windows))]
    let script = if interactive {
        format!(
            "{command}\n__agent_frame_shell_exit=$?\nprintf '__AGENT_FRAME_SHELL_DONE__:{process_id}:%s\\n' \"$__agent_frame_shell_exit\" >&2\n"
        )
    } else {
        format!(
            "{{ {command}; }} >{} 2>{}\n__agent_frame_shell_exit=$?\nprintf '__AGENT_FRAME_SHELL_DONE__:{process_id}:%s\\n' \"$__agent_frame_shell_exit\" >&2\n",
            shell_quote(&stdout_path.display().to_string()),
            shell_quote(&stderr_path.display().to_string())
        )
    };
    stdin
        .write_all(script.as_bytes())
        .context("failed to send shell command to persistent shell")?;
    stdin
        .flush()
        .context("failed to flush persistent shell stdin")
}

fn send_shell_input(stdin: &mut ChildStdin, input: &str) -> Result<()> {
    stdin
        .write_all(input.as_bytes())
        .context("failed to write shell input")?;
    stdin.flush().context("failed to flush shell input")
}

fn initialize_shell_session(
    stdin: &mut ChildStdin,
    remote: Option<&str>,
    initial_cwd: &str,
) -> Result<()> {
    if remote.is_some_and(|host| host != "local") {
        #[cfg(windows)]
        let command = format!("cd /d {}\r\n", initial_cwd);
        #[cfg(not(windows))]
        let command = format!("cd {}\n", shell_quote(initial_cwd));
        stdin
            .write_all(command.as_bytes())
            .context("failed to initialize remote shell cwd")?;
        stdin
            .flush()
            .context("failed to flush remote shell initialization")?;
    }
    Ok(())
}

fn run_shell_session_job(
    session_id: &str,
    interactive: bool,
    remote: Option<&str>,
    initial_cwd: &str,
    status_path: &Path,
    requests_dir: &Path,
    output_root: &Path,
) -> Result<()> {
    fs::create_dir_all(requests_dir)
        .with_context(|| format!("failed to create {}", requests_dir.display()))?;
    fs::create_dir_all(output_root)
        .with_context(|| format!("failed to create {}", output_root.display()))?;

    let initial_cwd_path = Path::new(initial_cwd);
    let (mut shell, mut stdin, stdout, stderr) = spawn_persistent_shell(remote, initial_cwd_path)?;
    initialize_shell_session(&mut stdin, remote, initial_cwd)?;
    let shell_pid = shell.id();
    write_shell_session_status(
        status_path,
        session_id,
        interactive,
        remote,
        Some(shell_pid),
        false,
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    let writer_state = Arc::new(Mutex::new(ShellWriterState::default()));
    let (event_sender, event_receiver) = mpsc::channel();
    let stdout_handle = spawn_stdout_forwarder(stdout, writer_state.clone(), event_sender.clone());
    let stderr_handle = spawn_stderr_forwarder(stderr, writer_state.clone(), event_sender);

    let mut current_process_id: Option<String> = None;
    let mut current_command: Option<String> = None;
    let mut current_stdout_path: Option<PathBuf> = None;
    let mut current_stderr_path: Option<PathBuf> = None;
    let mut running = false;
    let mut current_exit_code: Option<i32> = None;

    let run_result = (|| -> Result<()> {
        loop {
            while let Ok(event) = event_receiver.try_recv() {
                match event {
                    ShellEvent::ProcessDone {
                        process_id,
                        exit_code,
                    } if current_process_id.as_deref() == Some(process_id.as_str()) => {
                        running = false;
                        current_exit_code = Some(exit_code);
                        wait_for_shell_output_settle(
                            current_stdout_path.as_deref(),
                            current_stderr_path.as_deref(),
                        );
                        write_shell_session_status(
                            status_path,
                            session_id,
                            interactive,
                            remote,
                            Some(shell_pid),
                            false,
                            current_process_id.as_deref(),
                            current_exit_code,
                            current_command.as_deref(),
                            current_stdout_path.as_deref(),
                            current_stderr_path.as_deref(),
                            None,
                        )?;
                    }
                    ShellEvent::ReaderFailed { stream, error } => {
                        return Err(anyhow!(
                            "persistent shell {} reader failed: {}",
                            stream,
                            error
                        ));
                    }
                    _ => {}
                }
            }

            if let Some(status) = shell
                .try_wait()
                .context("failed to poll persistent shell process")?
            {
                let code = status.code().unwrap_or(-1);
                write_shell_session_status(
                    status_path,
                    session_id,
                    interactive,
                    remote,
                    Some(shell_pid),
                    false,
                    current_process_id.as_deref(),
                    current_exit_code.or(Some(code)),
                    current_command.as_deref(),
                    current_stdout_path.as_deref(),
                    current_stderr_path.as_deref(),
                    if current_process_id.is_some() {
                        Some("persistent shell exited unexpectedly")
                    } else {
                        None
                    },
                )?;
                break;
            }

            for request_path in shell_request_paths(requests_dir)? {
                let result_path = shell_request_result_path(&request_path);
                let request = read_shell_request(&request_path);
                let response = match request {
                    Ok(ShellRequest::Run {
                        process_id,
                        command,
                    }) => {
                        if running {
                            json!({"ok": false, "error": "session already has a running command"})
                        } else {
                            let process_dir = output_root.join(&process_id);
                            fs::create_dir_all(&process_dir).with_context(|| {
                                format!("failed to create {}", process_dir.display())
                            })?;
                            let stdout_path = process_dir.join("stdout");
                            let stderr_path = process_dir.join("stderr");
                            fs::write(&stdout_path, b"").with_context(|| {
                                format!("failed to create {}", stdout_path.display())
                            })?;
                            fs::write(&stderr_path, b"").with_context(|| {
                                format!("failed to create {}", stderr_path.display())
                            })?;
                            {
                                let mut writer_state = writer_state
                                    .lock()
                                    .map_err(|_| anyhow!("shell writer state poisoned"))?;
                                writer_state.stdout_path = Some(stdout_path.clone());
                                writer_state.stderr_path = Some(stderr_path.clone());
                            }
                            current_process_id = Some(process_id.clone());
                            current_command = Some(command.clone());
                            current_stdout_path = Some(stdout_path);
                            current_stderr_path = Some(stderr_path);
                            running = true;
                            current_exit_code = None;
                            start_shell_process(
                                &mut stdin,
                                &process_id,
                                &command,
                                interactive,
                                current_stdout_path.as_deref().unwrap(),
                                current_stderr_path.as_deref().unwrap(),
                            )?;
                            write_shell_session_status(
                                status_path,
                                session_id,
                                interactive,
                                remote,
                                Some(shell_pid),
                                true,
                                current_process_id.as_deref(),
                                None,
                                current_command.as_deref(),
                                current_stdout_path.as_deref(),
                                current_stderr_path.as_deref(),
                                None,
                            )?;
                            json!({"ok": true, "process_id": process_id})
                        }
                    }
                    Ok(ShellRequest::Input { input }) => {
                        if !interactive {
                            json!({"ok": false, "error": "session is not interactive"})
                        } else if !running {
                            json!({"ok": false, "error": "session does not have a running command"})
                        } else {
                            send_shell_input(&mut stdin, &input)?;
                            json!({"ok": true})
                        }
                    }
                    Ok(ShellRequest::Close) => {
                        let _ = shell.kill();
                        let _ = shell.wait();
                        write_shell_request_result(&result_path, &json!({"ok": true}))?;
                        let _ = fs::remove_file(&request_path);
                        return Ok(());
                    }
                    Err(error) => json!({"ok": false, "error": format!("{:#}", error)}),
                };
                write_shell_request_result(&result_path, &response)?;
                let _ = fs::remove_file(&request_path);
            }

            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    })();

    let _ = shell.kill();
    let _ = shell.wait();
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    if let Err(error) = run_result {
        write_shell_session_status(
            status_path,
            session_id,
            interactive,
            remote,
            Some(shell_pid),
            false,
            current_process_id.as_deref(),
            current_exit_code,
            current_command.as_deref(),
            current_stdout_path.as_deref(),
            current_stderr_path.as_deref(),
            Some(&format!("{error:#}")),
        )?;
    }

    Ok(())
}

fn strip_html_tags(body: &str) -> String {
    let mut output = String::with_capacity(body.len());
    let mut in_tag = false;
    for ch in body.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                output.push(' ');
            }
            _ if !in_tag => output.push(ch),
            _ => {}
        }
    }
    output.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn load_generated_image_result(result: &str) -> Result<Vec<u8>> {
    if result.starts_with("http://") || result.starts_with("https://") {
        let client = build_http_client(result, Some(60.0))?;
        let response = client
            .get(result)
            .send()
            .context("failed to download generated image")?;
        let status = response.status();
        let bytes = response
            .bytes()
            .context("failed to read generated image download")?;
        if !status.is_success() {
            return Err(anyhow!("generated image download failed with {}", status));
        }
        return Ok(bytes.to_vec());
    }

    let payload = if let Some(payload) = parse_base64_data_url(result) {
        payload
    } else {
        result
    };

    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .context("failed to decode generated image base64")
}

fn canonicalize_generated_image_bytes_to_png(bytes: &[u8]) -> Result<Vec<u8>> {
    let image = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .context("failed to detect generated image format")?
        .decode()
        .context("generated image payload was not a decodable image")?;
    let mut output = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut output), ImageFormat::Png)
        .context("failed to re-encode generated image as PNG")?;
    Ok(output)
}

fn parse_base64_data_url(value: &str) -> Option<&str> {
    let (metadata, payload) = value.split_once(',')?;
    if !metadata.starts_with("data:") {
        return None;
    }
    let mut metadata_parts = metadata["data:".len()..].split(';');
    metadata_parts.next()?;
    if !metadata_parts.any(|part| part.eq_ignore_ascii_case("base64")) {
        return None;
    }
    Some(payload)
}

fn infer_image_media_type(path: &Path) -> String {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        _ => "image/png",
    }
    .to_string()
}

fn image_to_data_url(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    Ok(format!(
        "data:{};base64,{}",
        infer_image_media_type(path),
        encoded
    ))
}

fn file_to_base64(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
}

fn append_reference_images(content: &mut Vec<Value>, image_paths: &[String]) -> Result<()> {
    for path in image_paths {
        content.push(json!({
            "type": "image_url",
            "image_url": {
                "url": image_to_data_url(Path::new(path))?
            }
        }));
    }
    Ok(())
}

fn append_reference_input_images(content: &mut Vec<Value>, image_paths: &[String]) -> Result<()> {
    for path in image_paths {
        content.push(json!({
            "type": "input_image",
            "image_url": image_to_data_url(Path::new(path))?
        }));
    }
    Ok(())
}

fn infer_audio_format(path: &Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "wav" => Some("wav"),
        "mp3" | "mpeg" | "mpga" => Some("mp3"),
        "ogg" | "opus" => Some("ogg"),
        "webm" => Some("webm"),
        "m4a" | "mp4" | "aac" => Some("m4a"),
        "flac" => Some("flac"),
        _ => None,
    }
}

fn chat_message_content_to_text(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let item_type = object.get("type")?.as_str()?;
                match item_type {
                    "text" | "input_text" | "output_text" => {
                        object.get("text")?.as_str().map(ToOwned::to_owned)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn chat_message_text(message: &ChatMessage) -> String {
    match &message.content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let item_type = object.get("type")?.as_str()?;
                match item_type {
                    "text" | "input_text" | "output_text" => {
                        object.get("text")?.as_str().map(ToOwned::to_owned)
                    }
                    _ => None,
                }
            })
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExternalWebSearchProvider, build_brave_web_search_request_spec,
        build_web_search_request_spec, canonicalize_generated_image_bytes_to_png,
        load_generated_image_result, parse_base64_data_url, parse_brave_web_search_response,
    };
    use crate::config::ExternalWebSearchConfig;
    use base64::Engine as _;
    use image::{GenericImageView, ImageFormat, ImageReader};
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn generated_image_result_accepts_raw_base64() {
        let expected = b"test-image-bytes";
        let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
        let decoded = load_generated_image_result(&encoded).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn generated_image_result_accepts_base64_data_url() {
        let expected = b"test-image-bytes";
        let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
        let data_url = format!("data:image/png;base64,{encoded}");
        assert_eq!(parse_base64_data_url(&data_url), Some(encoded.as_str()));
        let decoded = load_generated_image_result(&data_url).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn generated_image_result_is_canonicalized_to_png() {
        let image = image::DynamicImage::new_rgb8(2, 3);
        let mut jpeg = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut jpeg), ImageFormat::Jpeg)
            .unwrap();

        let png = canonicalize_generated_image_bytes_to_png(&jpeg).unwrap();
        let decoded = ImageReader::new(Cursor::new(&png))
            .with_guessed_format()
            .unwrap()
            .decode()
            .unwrap();

        assert_eq!(decoded.dimensions(), (2, 3));
    }

    #[test]
    fn brave_web_search_request_uses_get_query_and_subscription_header() {
        let config = ExternalWebSearchConfig {
            base_url: "https://api.search.brave.com".to_string(),
            model: "ignored".to_string(),
            supports_vision_input: false,
            api_key: Some("brave-secret".to_string()),
            api_key_env: "BRAVE_SEARCH_API_KEY".to_string(),
            chat_completions_path: "/res/v1/web/search".to_string(),
            timeout_seconds: 30.0,
            headers: serde_json::Map::from_iter([(
                "accept".to_string(),
                json!("application/json"),
            )]),
        };

        let spec = build_web_search_request_spec(&config, "rust async actors", 50, &[]).unwrap();

        assert_eq!(spec.provider, ExternalWebSearchProvider::Brave);
        assert_eq!(spec.method, reqwest::Method::GET);
        assert_eq!(spec.url, "https://api.search.brave.com/res/v1/web/search");
        assert_eq!(
            spec.query[0],
            ("q".to_string(), "rust async actors".to_string())
        );
        assert_eq!(spec.query[1], ("count".to_string(), "20".to_string()));
        assert_eq!(
            spec.auth_header,
            Some((
                "x-subscription-token".to_string(),
                "brave-secret".to_string()
            ))
        );
        assert_eq!(
            spec.headers,
            vec![("accept".to_string(), "application/json".to_string())]
        );
        assert!(spec.json_body.is_none());
    }

    #[test]
    fn brave_web_search_request_rejects_images() {
        let config = ExternalWebSearchConfig {
            base_url: "https://api.search.brave.com".to_string(),
            model: "ignored".to_string(),
            supports_vision_input: false,
            api_key: Some("brave-secret".to_string()),
            api_key_env: "BRAVE_SEARCH_API_KEY".to_string(),
            chat_completions_path: "/res/v1/web/search".to_string(),
            timeout_seconds: 30.0,
            headers: serde_json::Map::new(),
        };

        let error = build_brave_web_search_request_spec(
            &config,
            "https://api.search.brave.com/res/v1/web/search".to_string(),
            "query",
            5,
            &[String::from("diagram.png")],
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not support image inputs"));
    }

    #[test]
    fn brave_web_search_response_is_compacted_into_answer_and_citations() {
        let parsed = parse_brave_web_search_response(
            &json!({
                "web": {
                    "results": [
                        {
                            "title": "Tokio Tutorial",
                            "url": "https://tokio.rs/tutorial",
                            "description": "Learn async Rust with Tokio.",
                            "extra_snippets": [
                                "Covers tasks and channels.",
                                "Includes mini-redis examples."
                            ]
                        },
                        {
                            "title": "Actix Overview",
                            "url": "https://actix.rs",
                            "description": "Actor framework for Rust."
                        }
                    ]
                }
            }),
            "rust async actors",
            2,
        );

        assert_eq!(parsed["citations"][0], json!("https://tokio.rs/tutorial"));
        assert_eq!(parsed["citations"][1], json!("https://actix.rs"));
        assert_eq!(parsed["results"][0]["title"], json!("Tokio Tutorial"));
        assert_eq!(
            parsed["results"][0]["extra_snippets"][0],
            json!("Covers tasks and channels.")
        );
        assert!(
            parsed["answer"]
                .as_str()
                .unwrap()
                .contains("Snippet: Learn async Rust with Tokio.")
        );
    }
}
