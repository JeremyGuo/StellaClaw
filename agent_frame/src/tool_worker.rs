use crate::config::{ExternalWebSearchConfig, UpstreamConfig};
use crate::llm::create_chat_completion;
use crate::message::ChatMessage;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::fs;
use std::io::{Read, Write};
use std::net::IpAddr;
use std::path::Path;

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
            let result = run_image_generate_job(&prompt, upstream, Path::new(&output_path), &images)?;
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
    let request_url = {
        let base = search_config.base_url.trim_end_matches('/');
        let path = if search_config.chat_completions_path.starts_with('/') {
            search_config.chat_completions_path.clone()
        } else {
            format!("/{}", search_config.chat_completions_path)
        };
        format!("{}{}", base, path)
    };
    let client = build_http_client(&request_url, Some(search_config.timeout_seconds))?;
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
    let mut request = client.post(&request_url).json(&Value::Object(payload));
    if let Some(api_key) = search_config
        .api_key
        .clone()
        .or_else(|| std::env::var(&search_config.api_key_env).ok())
    {
        request = request.bearer_auth(api_key);
    }
    for (key, value) in &search_config.headers {
        if let Some(value) = value.as_str() {
            request = request.header(key, value);
        }
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
    Ok(json!({
        "query": query,
        "answer": answer,
        "citations": citations,
    }))
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
    if upstream.api_kind != crate::config::UpstreamApiKind::Responses {
        return Err(anyhow!(
            "image_generate requires a responses-compatible upstream"
        ));
    }
    let request_url = {
        let base = upstream.base_url.trim_end_matches('/');
        let path = if upstream.chat_completions_path.starts_with('/') {
            upstream.chat_completions_path.clone()
        } else {
            format!("/{}", upstream.chat_completions_path)
        };
        format!("{}{}", base, path)
    };
    let client = build_http_client(&request_url, Some(upstream.timeout_seconds))?;
    let mut payload = Map::new();
    payload.insert("model".to_string(), Value::String(upstream.model.clone()));
    let input = if images.is_empty() {
        Value::String(prompt.to_string())
    } else {
        let mut content = vec![json!({
            "type": "input_text",
            "text": prompt,
        })];
        append_reference_input_images(&mut content, images)?;
        json!([{
            "type": "message",
            "role": "user",
            "content": content,
        }])
    };
    payload.insert("input".to_string(), input);
    payload.insert(
        "tools".to_string(),
        Value::Array(vec![json!({
            "type": "image_generation"
        })]),
    );
    payload.insert("store".to_string(), Value::Bool(false));

    let mut request = client.post(&request_url).json(&Value::Object(payload));
    if let Some(api_key) = upstream
        .api_key
        .clone()
        .or_else(|| std::env::var(&upstream.api_key_env).ok())
    {
        request = request.bearer_auth(api_key);
    } else if upstream.auth_kind == crate::config::UpstreamAuthKind::CodexSubscription {
        return Err(anyhow!(
            "image_generate does not currently support codex-subscription auth in worker mode"
        ));
    }
    for (key, value) in &upstream.headers {
        if let Some(value) = value.as_str() {
            request = request.header(key, value);
        }
    }

    let response = request
        .send()
        .context("image generation request failed")?;
    let status = response.status();
    let body = response
        .text()
        .context("failed to read image generation response body")?;
    if !status.is_success() {
        return Err(anyhow!(
            "image generation failed with {}: {}",
            status,
            body
        ));
    }
    let value: Value =
        serde_json::from_str(&body).context("failed to parse image generation response")?;
    if let Some(error_message) = crate::llm::upstream_error_from_value(&value) {
        return Err(anyhow!(
            "image generation returned an error payload: {}",
            error_message
        ));
    }
    let image_base64 = value
        .get("output")
        .and_then(Value::as_array)
        .and_then(|output| {
            output.iter().find_map(|item| {
                (item.get("type").and_then(Value::as_str) == Some("image_generation_call"))
                    .then(|| item.get("result").and_then(Value::as_str))
                    .flatten()
            })
        })
        .ok_or_else(|| anyhow!("image generation response did not contain image data"))?;
    let image_bytes = decode_generated_image_result(image_base64)?;
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

fn decode_generated_image_result(result: &str) -> Result<Vec<u8>> {
    let payload = if let Some(payload) = parse_base64_data_url(result) {
        payload
    } else {
        result
    };

    base64::engine::general_purpose::STANDARD
        .decode(payload)
        .context("failed to decode generated image base64")
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
    use super::{decode_generated_image_result, parse_base64_data_url};
    use base64::Engine as _;

    #[test]
    fn generated_image_result_accepts_raw_base64() {
        let expected = b"test-image-bytes";
        let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
        let decoded = decode_generated_image_result(&encoded).unwrap();
        assert_eq!(decoded, expected);
    }

    #[test]
    fn generated_image_result_accepts_base64_data_url() {
        let expected = b"test-image-bytes";
        let encoded = base64::engine::general_purpose::STANDARD.encode(expected);
        let data_url = format!("data:image/png;base64,{encoded}");
        assert_eq!(parse_base64_data_url(&data_url), Some(encoded.as_str()));
        let decoded = decode_generated_image_result(&data_url).unwrap();
        assert_eq!(decoded, expected);
    }
}
