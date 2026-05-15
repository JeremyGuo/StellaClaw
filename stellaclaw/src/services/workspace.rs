#![allow(dead_code)]

use std::{
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose, Engine as _};
use crossbeam_channel::select;
use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use image::GenericImageView;
use serde_json::json;
use stellaclaw_core::session_actor::{
    ChatMessage, ChatMessageItem, FileItem, FileState, ToolRemoteMode,
};

use crate::{
    conversation_new::{
        ConversationRuntimeConfig, ConversationService, ServiceCall, ServiceOutput,
        ServiceRunContext, ServiceStatusUpdate, ServiceStopped,
    },
    service_protos::workspace::{
        decode_request, encode_response, WorkspaceEntry, WorkspaceEntryKind, WorkspaceFileEncoding,
        WorkspaceRemote, WorkspaceRequest, WorkspaceResponse, WorkspaceTarget,
    },
};

const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: usize = 50 * 1024 * 1024;

pub struct WorkspaceService {
    runtime_config: ConversationRuntimeConfig,
}

impl WorkspaceService {
    pub fn new(runtime_config: ConversationRuntimeConfig) -> Self {
        Self { runtime_config }
    }
}

impl ConversationService for WorkspaceService {
    fn run(self: Box<Self>, ctx: ServiceRunContext) -> Result<()> {
        let mut runtime_config = self.runtime_config;
        loop {
            select! {
                recv(ctx.stop_rx) -> stop => {
                    ctx.outbox.send(ServiceOutput::Stopped(ServiceStopped {
                        addr: ctx.addr.clone(),
                        reason: stop.ok().map(|stop| stop.reason),
                    }))?;
                    return Ok(());
                }
                recv(ctx.inbox) -> call => {
                    let call = call?;
                    match decode_request(call.payload) {
                        Ok(WorkspaceRequest::QueryRoot) => {
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(WorkspaceResponse::Root {
                                    path: workspace_root(&ctx, &runtime_config),
                                })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::RegisterAttachment { uri }) => {
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(WorkspaceResponse::AttachmentRegistered { uri })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::MaterializeMessage { message }) => {
                            let message = materialize_message(&ctx, message)?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(WorkspaceResponse::MessageMaterialized { message })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::MaterializeFiles { files }) => {
                            let files = materialize_files(&ctx, files)?;
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(WorkspaceResponse::FilesMaterialized { files })?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::UpdateRuntimeConfig { config }) => {
                            runtime_config = config;
                            ctx.outbox.send(ServiceOutput::Status(ServiceStatusUpdate {
                                addr: ctx.addr.clone(),
                                label: "runtime_config_updated".to_string(),
                                detail: workspace_runtime_detail(&ctx, &runtime_config),
                            }))?;
                        }
                        Ok(WorkspaceRequest::List { path, target, limit }) => {
                            let response = workspace_result(list_workspace(
                                &ctx,
                                &runtime_config,
                                path.as_deref().unwrap_or_default(),
                                target,
                                limit.unwrap_or(200),
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::ReadFile {
                            path,
                            target,
                            offset,
                            limit_bytes,
                        }) => {
                            let response = workspace_result(read_workspace_file(
                                &ctx,
                                &runtime_config,
                                &path,
                                target,
                                offset.unwrap_or(0),
                                limit_bytes,
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::WriteFile {
                            path,
                            target,
                            encoding,
                            data,
                            create_parent_dirs,
                            overwrite,
                        }) => {
                            let response = workspace_result(write_workspace_file(
                                &ctx,
                                &runtime_config,
                                &path,
                                target,
                                encoding,
                                &data,
                                create_parent_dirs,
                                overwrite,
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::DeletePath { path, target }) => {
                            let response = workspace_result(delete_workspace_path(
                                &ctx,
                                &runtime_config,
                                &path,
                                target,
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::MovePath {
                            from_path,
                            to_path,
                            target,
                        }) => {
                            let response = workspace_result(move_workspace_path(
                                &ctx,
                                &runtime_config,
                                &from_path,
                                &to_path,
                                target,
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::UploadArchive {
                            dir_path,
                            target,
                            encoding,
                            data,
                        }) => {
                            let response = workspace_result(upload_workspace_archive(
                                &ctx,
                                &runtime_config,
                                &dir_path,
                                target,
                                encoding,
                                &data,
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Ok(WorkspaceRequest::DownloadArchive { paths, target }) => {
                            let response = workspace_result(download_workspace_archive(
                                &ctx,
                                &runtime_config,
                                &paths,
                                target,
                            ));
                            ctx.outbox.send(ServiceOutput::Call(reply(
                                &ctx.addr,
                                &call.source,
                                encode_response(response)?,
                                call.request_id.clone(),
                            )))?;
                        }
                        Err(error) => {
                            ctx.outbox.send(ServiceOutput::Failed(crate::conversation_new::ServiceFailure {
                                addr: ctx.addr.clone(),
                                error: format!("bad workspace payload: {error}"),
                            }))?;
                        }
                    }
                }
            }
        }
    }
}

fn workspace_result(result: Result<WorkspaceResponse>) -> WorkspaceResponse {
    match result {
        Ok(response) => response,
        Err(error) => WorkspaceResponse::Error {
            message: format!("{error:#}"),
        },
    }
}

fn workspace_root(ctx: &ServiceRunContext, runtime_config: &ConversationRuntimeConfig) -> String {
    match &runtime_config.tool_remote_mode {
        ToolRemoteMode::Selectable => ctx.conversation.conversation_root.display().to_string(),
        ToolRemoteMode::FixedSsh { .. } => ctx.conversation.conversation_root.display().to_string(),
    }
}

fn workspace_runtime_detail(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
) -> serde_json::Value {
    let fixed_ssh = match &runtime_config.tool_remote_mode {
        ToolRemoteMode::FixedSsh { host, cwd } => json!({
            "host": host,
            "cwd": cwd,
        }),
        ToolRemoteMode::Selectable => serde_json::Value::Null,
    };
    json!({
        "local_overlay_root": ctx.conversation.conversation_root,
        "tool_remote_mode": runtime_config.tool_remote_mode,
        "fixed_ssh": fixed_ssh,
    })
}

fn materialize_message(ctx: &ServiceRunContext, mut message: ChatMessage) -> Result<ChatMessage> {
    let mut materialized = Vec::with_capacity(message.data.len());
    for item in message.data {
        match item {
            ChatMessageItem::File(file) => {
                materialized.push(ChatMessageItem::File(materialize_file(ctx, file)?));
            }
            other => materialized.push(other),
        }
    }
    message.data = materialized;
    Ok(message)
}

fn materialize_files(ctx: &ServiceRunContext, files: Vec<FileItem>) -> Result<Vec<FileItem>> {
    files
        .into_iter()
        .map(|file| materialize_file(ctx, file))
        .collect()
}

fn materialize_file(ctx: &ServiceRunContext, file: FileItem) -> Result<FileItem> {
    if !file.uri.starts_with("data:") {
        return Ok(file);
    }
    match decode_data_uri(&file.uri) {
        Ok((bytes, media_type)) => {
            let mut file = file;
            let media_type = file.media_type.clone().or(media_type);
            let name = file
                .name
                .clone()
                .unwrap_or_else(|| default_attachment_name(media_type.as_deref()));
            let target = unique_attachment_path(ctx, &name);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::write(&target, &bytes)
                .with_context(|| format!("failed to write attachment {}", target.display()))?;
            file.uri = format!("file://{}", target.display());
            file.name = Some(name);
            file.media_type = media_type;
            if file.width.is_none() || file.height.is_none() {
                if let Some((width, height)) = image_dimensions(file.media_type.as_deref(), &bytes)
                {
                    file.width = Some(width);
                    file.height = Some(height);
                }
            }
            file.state = None;
            Ok(file)
        }
        Err(error) => Ok(FileItem {
            state: Some(FileState::Crashed {
                reason: format!("failed to materialize data URI: {error:#}"),
            }),
            ..file
        }),
    }
}

fn decode_data_uri(uri: &str) -> Result<(Vec<u8>, Option<String>)> {
    let Some(rest) = uri.strip_prefix("data:") else {
        return Err(anyhow!("not a data URI"));
    };
    let (metadata, data) = rest
        .split_once(',')
        .ok_or_else(|| anyhow!("data URI missing comma separator"))?;
    let mut media_type = None;
    let mut is_base64 = false;
    for (index, part) in metadata.split(';').enumerate() {
        if index == 0 && !part.is_empty() {
            media_type = Some(part.to_string());
        } else if part.eq_ignore_ascii_case("base64") {
            is_base64 = true;
        }
    }
    let bytes = if is_base64 {
        general_purpose::STANDARD
            .decode(data)
            .context("invalid base64 data URI payload")?
    } else {
        data.as_bytes().to_vec()
    };
    Ok((bytes, media_type))
}

fn unique_attachment_path(ctx: &ServiceRunContext, name: &str) -> PathBuf {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%S%3fZ");
    ctx.conversation
        .conversation_root
        .join(".stellaclaw")
        .join("attachments")
        .join("incoming")
        .join(format!("{timestamp}-{}", sanitize_filename(name)))
}

fn sanitize_filename(name: &str) -> String {
    let mut sanitized = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    while sanitized.starts_with('.') {
        sanitized.remove(0);
    }
    if sanitized.is_empty() {
        "attachment.bin".to_string()
    } else {
        sanitized
    }
}

fn default_attachment_name(media_type: Option<&str>) -> String {
    match media_type {
        Some("image/png") => "attachment.png",
        Some("image/jpeg") => "attachment.jpg",
        Some("image/gif") => "attachment.gif",
        Some("image/webp") => "attachment.webp",
        Some("application/pdf") => "attachment.pdf",
        Some("text/plain") => "attachment.txt",
        _ => "attachment.bin",
    }
    .to_string()
}

fn image_dimensions(media_type: Option<&str>, bytes: &[u8]) -> Option<(u32, u32)> {
    let media_type = media_type?;
    if !media_type.starts_with("image/") {
        return None;
    }
    image::load_from_memory(bytes)
        .ok()
        .map(|image| image.dimensions())
}

#[derive(Debug, Clone)]
enum ResolvedWorkspaceTarget {
    LocalOverlay { root: PathBuf },
    LocalWorkspace { root: PathBuf },
    Remote { host: String, cwd: Option<String> },
}

impl ResolvedWorkspaceTarget {
    fn response_target(&self) -> WorkspaceTarget {
        match self {
            Self::LocalOverlay { .. } => WorkspaceTarget::LocalOverlay,
            Self::LocalWorkspace { .. } => WorkspaceTarget::LocalWorkspace,
            Self::Remote { .. } => WorkspaceTarget::Remote,
        }
    }

    fn remote(&self) -> Option<WorkspaceRemote> {
        match self {
            Self::Remote { host, cwd } => Some(WorkspaceRemote {
                host: host.clone(),
                cwd: cwd.clone(),
            }),
            _ => None,
        }
    }

    fn root_display(&self) -> String {
        match self {
            Self::LocalOverlay { root } | Self::LocalWorkspace { root } => {
                root.display().to_string()
            }
            Self::Remote { cwd, .. } => cwd.clone().unwrap_or_default(),
        }
    }
}

fn list_workspace(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    path: &str,
    target: WorkspaceTarget,
    limit: usize,
) -> Result<WorkspaceResponse> {
    let normalized = normalize_workspace_path(path)?;
    let resolved = resolve_workspace_target(ctx, runtime_config, target, &normalized)?;
    match &resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => {
            list_local_workspace(&resolved, root, &normalized, limit)
        }
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &resolved,
                json!({
                    "op": "list",
                    "path": path_to_api_string(&normalized),
                    "limit": limit.max(1),
                }),
            )?;
            serde_json::from_value(payload).context("failed to decode remote workspace listing")
        }
    }
}

fn read_workspace_file(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    path: &str,
    target: WorkspaceTarget,
    offset: u64,
    limit_bytes: Option<usize>,
) -> Result<WorkspaceResponse> {
    let normalized = normalize_workspace_path(path)?;
    ensure_non_empty_path(&normalized, "workspace file path")?;
    let resolved = resolve_workspace_target(ctx, runtime_config, target, &normalized)?;
    match &resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => {
            read_local_workspace_file(&resolved, root, &normalized, offset, limit_bytes)
        }
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &resolved,
                json!({
                    "op": "read",
                    "path": path_to_api_string(&normalized),
                    "offset": offset,
                    "limit_bytes": limit_bytes,
                }),
            )?;
            serde_json::from_value(payload).context("failed to decode remote workspace file")
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn write_workspace_file(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    path: &str,
    target: WorkspaceTarget,
    encoding: WorkspaceFileEncoding,
    data: &str,
    create_parent_dirs: bool,
    overwrite: bool,
) -> Result<WorkspaceResponse> {
    let normalized = normalize_workspace_path(path)?;
    ensure_non_empty_path(&normalized, "workspace file path")?;
    let resolved = resolve_workspace_target(ctx, runtime_config, target, &normalized)?;
    let bytes = decode_file_data(encoding, data)?;
    match &resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => write_local_workspace_file(
            &resolved,
            root,
            &normalized,
            &bytes,
            create_parent_dirs,
            overwrite,
        ),
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &resolved,
                json!({
                    "op": "write",
                    "path": path_to_api_string(&normalized),
                    "encoding": "base64",
                    "data": general_purpose::STANDARD.encode(bytes),
                    "create_parent_dirs": create_parent_dirs,
                    "overwrite": overwrite,
                }),
            )?;
            serde_json::from_value(payload)
                .context("failed to decode remote workspace write result")
        }
    }
}

fn delete_workspace_path(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    path: &str,
    target: WorkspaceTarget,
) -> Result<WorkspaceResponse> {
    let normalized = normalize_workspace_path(path)?;
    ensure_non_empty_path(&normalized, "workspace path")?;
    let resolved = resolve_workspace_target(ctx, runtime_config, target, &normalized)?;
    match &resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => {
            let absolute = root.join(&normalized);
            let metadata = fs::symlink_metadata(&absolute)
                .with_context(|| format!("failed to inspect {}", absolute.display()))?;
            if metadata.is_dir() {
                fs::remove_dir_all(&absolute)
                    .with_context(|| format!("failed to delete {}", absolute.display()))?;
            } else {
                fs::remove_file(&absolute)
                    .with_context(|| format!("failed to delete {}", absolute.display()))?;
            }
            Ok(WorkspaceResponse::Deleted {
                target: resolved.response_target(),
                path: path_to_api_string(&normalized),
            })
        }
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &resolved,
                json!({
                    "op": "delete",
                    "path": path_to_api_string(&normalized),
                }),
            )?;
            serde_json::from_value(payload)
                .context("failed to decode remote workspace delete result")
        }
    }
}

fn move_workspace_path(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    from_path: &str,
    to_path: &str,
    target: WorkspaceTarget,
) -> Result<WorkspaceResponse> {
    let from = normalize_workspace_path(from_path)?;
    let to = normalize_workspace_path(to_path)?;
    ensure_non_empty_path(&from, "workspace source path")?;
    ensure_non_empty_path(&to, "workspace destination path")?;
    let from_resolved = resolve_workspace_target(ctx, runtime_config, target, &from)?;
    let to_resolved = resolve_workspace_target(ctx, runtime_config, target, &to)?;
    if std::mem::discriminant(&from_resolved) != std::mem::discriminant(&to_resolved) {
        return Err(anyhow!(
            "cannot move paths across local overlay and remote workspace"
        ));
    }

    match &from_resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => {
            let source = root.join(&from);
            let destination = root.join(&to);
            if destination.exists() {
                return Err(anyhow!(
                    "workspace destination already exists: {}",
                    path_to_api_string(&to)
                ));
            }
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
            fs::rename(&source, &destination).with_context(|| {
                format!(
                    "failed to move {} to {}",
                    source.display(),
                    destination.display()
                )
            })?;
            Ok(WorkspaceResponse::Moved {
                target: from_resolved.response_target(),
                from_path: path_to_api_string(&from),
                to_path: path_to_api_string(&to),
            })
        }
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &from_resolved,
                json!({
                    "op": "move",
                    "from_path": path_to_api_string(&from),
                    "to_path": path_to_api_string(&to),
                }),
            )?;
            serde_json::from_value(payload).context("failed to decode remote workspace move result")
        }
    }
}

fn upload_workspace_archive(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    dir_path: &str,
    target: WorkspaceTarget,
    encoding: WorkspaceFileEncoding,
    data: &str,
) -> Result<WorkspaceResponse> {
    let normalized = normalize_workspace_path(dir_path)?;
    let archive_data = decode_file_data(encoding, data)?;
    if archive_data.len() > MAX_UPLOAD_BYTES {
        return Err(anyhow!(
            "upload exceeds {} byte limit (got {} bytes)",
            MAX_UPLOAD_BYTES,
            archive_data.len()
        ));
    }
    let resolved = resolve_workspace_target(ctx, runtime_config, target, &normalized)?;
    match &resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => {
            let entries_extracted =
                upload_local_workspace_archive(root, &normalized, &archive_data)?;
            Ok(WorkspaceResponse::ArchiveUploaded {
                target: resolved.response_target(),
                path: path_to_api_string(&normalized),
                entries_extracted,
            })
        }
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &resolved,
                json!({
                    "op": "upload_archive",
                    "path": path_to_api_string(&normalized),
                    "encoding": "base64",
                    "data": general_purpose::STANDARD.encode(archive_data),
                }),
            )?;
            serde_json::from_value(payload)
                .context("failed to decode remote workspace archive upload result")
        }
    }
}

fn upload_local_workspace_archive(
    root: &Path,
    normalized: &Path,
    archive_data: &[u8],
) -> Result<usize> {
    let target_dir = root.join(normalized);
    fs::create_dir_all(&target_dir)
        .with_context(|| format!("failed to create {}", target_dir.display()))?;

    let decoder = GzDecoder::new(archive_data);
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(true);
    let mut count = 0_usize;
    for entry in archive
        .entries()
        .with_context(|| "failed to read tar entries")?
    {
        let mut entry = entry.with_context(|| "failed to read tar entry")?;
        let entry_path = entry
            .path()
            .with_context(|| "failed to read entry path")?
            .into_owned();
        if entry_path.is_absolute()
            || entry_path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
            || entry.header().entry_type().is_symlink()
            || entry.header().entry_type().is_hard_link()
        {
            continue;
        }
        entry
            .unpack_in(&target_dir)
            .with_context(|| format!("failed to unpack {}", entry_path.display()))?;
        count += 1;
    }
    Ok(count)
}

fn download_workspace_archive(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    paths: &[String],
    target: WorkspaceTarget,
) -> Result<WorkspaceResponse> {
    if paths.is_empty() {
        return Err(anyhow!("at least one path is required for download"));
    }
    let normalized = paths
        .iter()
        .map(|path| normalize_workspace_path(path))
        .collect::<Result<Vec<_>>>()?;
    let first = normalized
        .first()
        .ok_or_else(|| anyhow!("at least one path is required for download"))?;
    let resolved = resolve_workspace_target(ctx, runtime_config, target, first)?;
    for path in &normalized[1..] {
        let next = resolve_workspace_target(ctx, runtime_config, target, path)?;
        if std::mem::discriminant(&resolved) != std::mem::discriminant(&next) {
            return Err(anyhow!(
                "cannot download local overlay paths together with remote workspace paths"
            ));
        }
    }
    let archive_data = match &resolved {
        ResolvedWorkspaceTarget::LocalOverlay { root }
        | ResolvedWorkspaceTarget::LocalWorkspace { root } => {
            download_local_workspace_archive(root, &normalized)?
        }
        ResolvedWorkspaceTarget::Remote { .. } => {
            let payload = remote_workspace_request(
                &resolved,
                json!({
                    "op": "download_archive",
                    "paths": normalized.iter().map(|path| path_to_api_string(path)).collect::<Vec<_>>(),
                }),
            )?;
            return serde_json::from_value(payload)
                .context("failed to decode remote workspace archive download result");
        }
    };
    if archive_data.len() > MAX_DOWNLOAD_BYTES {
        return Err(anyhow!(
            "download archive exceeds {} byte limit (got {} bytes)",
            MAX_DOWNLOAD_BYTES,
            archive_data.len()
        ));
    }
    Ok(WorkspaceResponse::ArchiveDownloaded {
        target: resolved.response_target(),
        paths: normalized
            .iter()
            .map(|path| path_to_api_string(path))
            .collect(),
        encoding: WorkspaceFileEncoding::Base64,
        bytes: archive_data.len(),
        data: general_purpose::STANDARD.encode(archive_data),
    })
}

fn download_local_workspace_archive(root: &Path, normalized_paths: &[PathBuf]) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    {
        let encoder = GzEncoder::new(&mut output, Compression::fast());
        let mut builder = tar::Builder::new(encoder);
        for normalized in normalized_paths {
            let target = root.join(normalized);
            let metadata = fs::metadata(&target)
                .with_context(|| format!("failed to inspect {}", target.display()))?;
            let archive_name = if normalized.as_os_str().is_empty() {
                PathBuf::from("workspace")
            } else {
                normalized.clone()
            };
            if metadata.is_dir() {
                builder
                    .append_dir_all(&archive_name, &target)
                    .with_context(|| format!("failed to archive directory {}", target.display()))?;
            } else if metadata.is_file() {
                let mut file = fs::File::open(&target)
                    .with_context(|| format!("failed to open {}", target.display()))?;
                builder
                    .append_file(&archive_name, &mut file)
                    .with_context(|| format!("failed to archive file {}", target.display()))?;
            }
        }
        builder
            .finish()
            .with_context(|| "failed to finalize tar archive")?;
    }
    Ok(output)
}

fn resolve_workspace_target(
    ctx: &ServiceRunContext,
    runtime_config: &ConversationRuntimeConfig,
    target: WorkspaceTarget,
    path: &Path,
) -> Result<ResolvedWorkspaceTarget> {
    let local_root = ctx.conversation.conversation_root.clone();
    match (&runtime_config.tool_remote_mode, target) {
        (ToolRemoteMode::Selectable, WorkspaceTarget::Auto | WorkspaceTarget::LocalWorkspace) => {
            Ok(ResolvedWorkspaceTarget::LocalWorkspace { root: local_root })
        }
        (ToolRemoteMode::Selectable, WorkspaceTarget::LocalOverlay) => {
            ensure_local_overlay_path(path)?;
            Ok(ResolvedWorkspaceTarget::LocalOverlay { root: local_root })
        }
        (ToolRemoteMode::Selectable, WorkspaceTarget::Remote) => Err(anyhow!(
            "remote workspace target is unavailable in selectable mode"
        )),
        (ToolRemoteMode::FixedSsh { .. }, WorkspaceTarget::LocalWorkspace) => Err(anyhow!(
            "local workspace target is unavailable in fixed ssh mode; use local_overlay for .stellaclaw paths"
        )),
        (ToolRemoteMode::FixedSsh { host, cwd }, WorkspaceTarget::Auto) => {
            if is_local_overlay_path(path) {
                Ok(ResolvedWorkspaceTarget::LocalOverlay { root: local_root })
            } else {
                resolve_remote_target(host, cwd)
            }
        }
        (ToolRemoteMode::FixedSsh { .. }, WorkspaceTarget::LocalOverlay) => {
            ensure_local_overlay_path(path)?;
            Ok(ResolvedWorkspaceTarget::LocalOverlay { root: local_root })
        }
        (ToolRemoteMode::FixedSsh { host, cwd }, WorkspaceTarget::Remote) => {
            if is_local_overlay_path(path) {
                return Err(anyhow!(
                    "remote target cannot access local .stellaclaw overlay paths"
                ));
            }
            resolve_remote_target(host, cwd)
        }
    }
}

fn resolve_remote_target(host: &str, cwd: &Option<String>) -> Result<ResolvedWorkspaceTarget> {
    let host = host.trim();
    if host.is_empty() {
        return Err(anyhow!("remote host must not be empty"));
    }
    if !host
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    {
        return Err(anyhow!(
            "remote host must be a safe ~/.ssh/config Host alias"
        ));
    }
    let cwd = cwd
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("remote workspace path must not be empty"))?
        .to_string();
    Ok(ResolvedWorkspaceTarget::Remote {
        host: host.to_string(),
        cwd: Some(cwd),
    })
}

fn list_local_workspace(
    resolved: &ResolvedWorkspaceTarget,
    root: &Path,
    normalized: &Path,
    limit: usize,
) -> Result<WorkspaceResponse> {
    let absolute = root.join(normalized);
    let metadata = fs::metadata(&absolute)
        .with_context(|| format!("failed to inspect {}", absolute.display()))?;
    if !metadata.is_dir() {
        return Err(anyhow!(
            "workspace path is not a directory: {}",
            path_to_api_string(normalized)
        ));
    }
    let mut entries = Vec::new();
    for entry in
        fs::read_dir(&absolute).with_context(|| format!("failed to read {}", absolute.display()))?
    {
        let entry = entry.with_context(|| format!("failed to enumerate {}", absolute.display()))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let relative_path = normalized.join(&name);
        let metadata = fs::symlink_metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        entries.push(WorkspaceEntry {
            hidden: name.starts_with('.'),
            path: path_to_api_string(&relative_path),
            name,
            kind: entry_kind(&metadata),
            size_bytes: metadata.is_file().then_some(metadata.len()),
            modified_ms: metadata.modified().ok().and_then(system_time_ms),
            readonly: metadata.permissions().readonly(),
        });
    }
    entries.sort_by(|left, right| {
        left.kind
            .cmp(&right.kind)
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
            .then_with(|| left.name.cmp(&right.name))
    });
    let total_entries = entries.len();
    let effective_limit = limit.max(1);
    let truncated = total_entries > effective_limit;
    entries.truncate(effective_limit);
    Ok(WorkspaceResponse::Listing {
        target: resolved.response_target(),
        remote: resolved.remote(),
        workspace_root: resolved.root_display(),
        path: path_to_api_string(normalized),
        parent: parent_api_path(normalized),
        total_entries,
        returned_entries: entries.len(),
        truncated,
        entries,
    })
}

fn read_local_workspace_file(
    resolved: &ResolvedWorkspaceTarget,
    root: &Path,
    normalized: &Path,
    offset: u64,
    limit_bytes: Option<usize>,
) -> Result<WorkspaceResponse> {
    let absolute = root.join(normalized);
    let metadata = fs::metadata(&absolute)
        .with_context(|| format!("failed to inspect {}", absolute.display()))?;
    if !metadata.is_file() {
        return Err(anyhow!(
            "workspace path is not a file: {}",
            path_to_api_string(normalized)
        ));
    }
    let file_size = metadata.len();
    let mut file = fs::File::open(&absolute)
        .with_context(|| format!("failed to open {}", absolute.display()))?;
    let start = offset.min(file_size);
    file.seek(SeekFrom::Start(start))
        .with_context(|| format!("failed to seek {}", absolute.display()))?;
    let mut bytes = Vec::new();
    let returned_bytes = if let Some(limit_bytes) = limit_bytes {
        let limit = limit_bytes.max(1);
        bytes.resize(limit, 0);
        let read = file
            .read(&mut bytes)
            .with_context(|| format!("failed to read {}", absolute.display()))?;
        bytes.truncate(read);
        read
    } else {
        file.read_to_end(&mut bytes)
            .with_context(|| format!("failed to read {}", absolute.display()))?
    };
    let (encoding, data) = encode_file_data(bytes);
    Ok(WorkspaceResponse::File {
        target: resolved.response_target(),
        remote: resolved.remote(),
        workspace_root: resolved.root_display(),
        path: path_to_api_string(normalized),
        name: normalized
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default(),
        size_bytes: file_size,
        modified_ms: metadata.modified().ok().and_then(system_time_ms),
        offset: start,
        returned_bytes,
        truncated: start.saturating_add(returned_bytes as u64) < file_size,
        encoding,
        data,
    })
}

fn write_local_workspace_file(
    resolved: &ResolvedWorkspaceTarget,
    root: &Path,
    normalized: &Path,
    bytes: &[u8],
    create_parent_dirs: bool,
    overwrite: bool,
) -> Result<WorkspaceResponse> {
    let absolute = root.join(normalized);
    if absolute.exists() && !overwrite {
        return Err(anyhow!(
            "workspace path already exists: {}",
            path_to_api_string(normalized)
        ));
    }
    if let Some(parent) = absolute.parent() {
        if create_parent_dirs {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
    }
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true);
    if overwrite {
        options.truncate(true);
    } else {
        options.create_new(true);
    }
    let mut file = options
        .open(&absolute)
        .with_context(|| format!("failed to open {}", absolute.display()))?;
    file.write_all(bytes)
        .with_context(|| format!("failed to write {}", absolute.display()))?;
    Ok(WorkspaceResponse::WriteCompleted {
        target: resolved.response_target(),
        path: path_to_api_string(normalized),
        bytes_written: bytes.len(),
    })
}

fn decode_file_data(encoding: WorkspaceFileEncoding, data: &str) -> Result<Vec<u8>> {
    match encoding {
        WorkspaceFileEncoding::Utf8 => Ok(data.as_bytes().to_vec()),
        WorkspaceFileEncoding::Base64 => general_purpose::STANDARD
            .decode(data)
            .context("failed to decode base64 workspace file data"),
    }
}

fn encode_file_data(bytes: Vec<u8>) -> (WorkspaceFileEncoding, String) {
    match String::from_utf8(bytes) {
        Ok(text) => (WorkspaceFileEncoding::Utf8, text),
        Err(error) => (
            WorkspaceFileEncoding::Base64,
            general_purpose::STANDARD.encode(error.into_bytes()),
        ),
    }
}

fn normalize_workspace_path(path: &str) -> Result<PathBuf> {
    let path = path.trim();
    let path = Path::new(path);
    if path.is_absolute() {
        return Err(anyhow!("workspace path must be relative"));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(value) => normalized.push(value),
            Component::ParentDir => {
                return Err(anyhow!("workspace path must not contain parent components"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(anyhow!("workspace path must be relative"));
            }
        }
    }
    Ok(normalized)
}

fn ensure_non_empty_path(path: &Path, label: &str) -> Result<()> {
    if path.as_os_str().is_empty() {
        return Err(anyhow!("{label} must not be empty"));
    }
    Ok(())
}

fn is_local_overlay_path(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == ".stellaclaw")
}

fn ensure_local_overlay_path(path: &Path) -> Result<()> {
    if is_local_overlay_path(path) {
        Ok(())
    } else {
        Err(anyhow!(
            "local overlay target is limited to .stellaclaw paths"
        ))
    }
}

fn entry_kind(metadata: &fs::Metadata) -> WorkspaceEntryKind {
    if metadata.is_dir() {
        WorkspaceEntryKind::Directory
    } else if metadata.is_file() {
        WorkspaceEntryKind::File
    } else if metadata.file_type().is_symlink() {
        WorkspaceEntryKind::Symlink
    } else {
        WorkspaceEntryKind::Other
    }
}

fn system_time_ms(value: SystemTime) -> Option<u64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| duration.as_millis().try_into().ok())
}

fn parent_api_path(path: &Path) -> Option<String> {
    if path.as_os_str().is_empty() {
        return None;
    }
    path.parent().map(path_to_api_string)
}

fn path_to_api_string(path: &Path) -> String {
    path.components()
        .filter_map(|component| match component {
            Component::Normal(value) => Some(value.to_string_lossy().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn remote_workspace_request(
    resolved: &ResolvedWorkspaceTarget,
    payload: serde_json::Value,
) -> Result<serde_json::Value> {
    let ResolvedWorkspaceTarget::Remote { host, cwd } = resolved else {
        return Err(anyhow!("remote workspace request requires remote target"));
    };
    let request = json!({
        "cwd": cwd,
        "payload": payload,
    });
    let remote_command = remote_workspace_helper_command();
    let mut child = Command::new("ssh")
        .arg("-T")
        .arg(host)
        .arg(remote_command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start remote workspace helper on {host}"))?;
    {
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("remote workspace helper stdin unavailable"))?;
        stdin
            .write_all(serde_json::to_string(&request)?.as_bytes())
            .context("failed to write remote workspace helper request")?;
    }
    let output = child
        .wait_with_output()
        .context("failed to wait for remote workspace helper")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(anyhow!("remote workspace helper failed: {}", stderr.trim()));
    }
    let response: serde_json::Value = serde_json::from_slice(&output.stdout)
        .context("remote workspace helper returned bad JSON")?;
    if response["ok"].as_bool() == Some(true) {
        let mut payload = response
            .get("payload")
            .cloned()
            .ok_or_else(|| anyhow!("remote workspace helper omitted payload"))?;
        if let Some(object) = payload.as_object_mut() {
            object.insert(
                "remote".to_string(),
                serde_json::to_value(resolved.remote())?,
            );
        }
        Ok(payload)
    } else {
        Err(anyhow!(
            "{}",
            response["error"]
                .as_str()
                .unwrap_or("remote workspace helper failed")
        ))
    }
}

fn remote_workspace_helper_command() -> String {
    let encoded_script = general_purpose::STANDARD.encode(REMOTE_WORKSPACE_SCRIPT.as_bytes());
    format!(
        "python3 -c 'import base64,sys;exec(base64.b64decode(sys.argv[1]).decode(\"utf-8\"))' {encoded_script}"
    )
}

const REMOTE_WORKSPACE_SCRIPT: &str = r#"
import base64
import io
import json
import os
import pathlib
import shutil
import sys
import tarfile

def fail(message):
    print(json.dumps({"ok": False, "error": message}))
    sys.exit(0)

def norm(value):
    value = value or ""
    path = pathlib.PurePosixPath(value)
    if path.is_absolute():
        fail("workspace path must be relative")
    parts = []
    for part in path.parts:
        if part in ("", "."):
            continue
        if part == "..":
            fail("workspace path must not contain parent components")
        parts.append(part)
    return pathlib.Path(*parts)

def api(path):
    return "/".join(path.parts)

def kind(path):
    if path.is_dir():
        return "directory"
    if path.is_file():
        return "file"
    if path.is_symlink():
        return "symlink"
    return "other"

def modified_ms(path):
    try:
        return int(path.stat().st_mtime * 1000)
    except Exception:
        return None

def entry(path, rel):
    stat = path.stat()
    return {
        "name": path.name,
        "path": api(rel),
        "kind": kind(path),
        "size_bytes": stat.st_size if path.is_file() else None,
        "modified_ms": modified_ms(path),
        "hidden": path.name.startswith("."),
        "readonly": not os.access(path, os.W_OK),
    }

def response(payload):
    print(json.dumps({"ok": True, "payload": payload}))

request = json.loads(sys.stdin.read())
root = pathlib.Path(request.get("cwd") or "")
if not root.is_absolute():
    fail("remote workspace root must be absolute")
payload = request["payload"]
op = payload["op"]

if op == "list":
    rel = norm(payload.get("path"))
    target = root / rel
    if not target.is_dir():
        fail("workspace path is not a directory: " + api(rel))
    entries = [entry(child, rel / child.name) for child in target.iterdir()]
    entries.sort(key=lambda item: (item["kind"], item["name"].lower(), item["name"]))
    total = len(entries)
    limit = max(int(payload.get("limit") or 1), 1)
    entries = entries[:limit]
    parent = None if not rel.parts else api(pathlib.Path(*rel.parts[:-1]))
    response({
        "type": "listing",
        "target": "remote",
        "remote": None,
        "workspace_root": str(root),
        "path": api(rel),
        "parent": parent,
        "total_entries": total,
        "returned_entries": len(entries),
        "truncated": total > limit,
        "entries": entries,
    })
elif op == "read":
    rel = norm(payload.get("path"))
    target = root / rel
    if not target.is_file():
        fail("workspace path is not a file: " + api(rel))
    size = target.stat().st_size
    offset = min(int(payload.get("offset") or 0), size)
    limit = payload.get("limit_bytes")
    with target.open("rb") as handle:
        handle.seek(offset)
        data = handle.read(max(int(limit), 1)) if limit is not None else handle.read()
    try:
        text = data.decode("utf-8")
        encoding = "utf8"
    except UnicodeDecodeError:
        text = base64.b64encode(data).decode("ascii")
        encoding = "base64"
    response({
        "type": "file",
        "target": "remote",
        "remote": None,
        "workspace_root": str(root),
        "path": api(rel),
        "name": target.name,
        "size_bytes": size,
        "modified_ms": modified_ms(target),
        "offset": offset,
        "returned_bytes": len(data),
        "truncated": offset + len(data) < size,
        "encoding": encoding,
        "data": text,
    })
elif op == "write":
    rel = norm(payload.get("path"))
    if not rel.parts:
        fail("workspace file path must not be empty")
    target = root / rel
    if target.exists() and not payload.get("overwrite", False):
        fail("workspace path already exists: " + api(rel))
    if payload.get("create_parent_dirs", True):
        target.parent.mkdir(parents=True, exist_ok=True)
    if payload.get("encoding") == "base64":
        data = base64.b64decode(payload.get("data") or "")
    else:
        data = (payload.get("data") or "").encode("utf-8")
    target.write_bytes(data)
    response({"type": "write_completed", "target": "remote", "path": api(rel), "bytes_written": len(data)})
elif op == "delete":
    rel = norm(payload.get("path"))
    if not rel.parts:
        fail("workspace path must not be empty")
    target = root / rel
    if target.is_dir():
        shutil.rmtree(target)
    else:
        target.unlink()
    response({"type": "deleted", "target": "remote", "path": api(rel)})
elif op == "move":
    source_rel = norm(payload.get("from_path"))
    target_rel = norm(payload.get("to_path"))
    if not source_rel.parts or not target_rel.parts:
        fail("workspace source and destination paths must not be empty")
    source = root / source_rel
    target = root / target_rel
    if target.exists():
        fail("workspace destination already exists: " + api(target_rel))
    target.parent.mkdir(parents=True, exist_ok=True)
    source.rename(target)
    response({"type": "moved", "target": "remote", "from_path": api(source_rel), "to_path": api(target_rel)})
elif op == "upload_archive":
    rel = norm(payload.get("path"))
    target = root / rel
    target.mkdir(parents=True, exist_ok=True)
    data = base64.b64decode(payload.get("data") or "")
    count = 0
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        for member in archive.getmembers():
            member_path = pathlib.PurePosixPath(member.name)
            if member_path.is_absolute() or ".." in member_path.parts or member.issym() or member.islnk():
                continue
            archive.extract(member, target)
            count += 1
    response({"type": "archive_uploaded", "target": "remote", "path": api(rel), "entries_extracted": count})
elif op == "download_archive":
    rels = [norm(path) for path in payload.get("paths") or []]
    if not rels:
        fail("at least one path is required for download")
    buffer = io.BytesIO()
    with tarfile.open(fileobj=buffer, mode="w:gz") as archive:
        for rel in rels:
            target = root / rel
            if not target.exists():
                fail("workspace path does not exist: " + api(rel))
            archive_name = "workspace" if not rel.parts else api(rel)
            archive.add(target, arcname=archive_name)
    data = buffer.getvalue()
    response({
        "type": "archive_downloaded",
        "target": "remote",
        "paths": [api(rel) for rel in rels],
        "encoding": "base64",
        "data": base64.b64encode(data).decode("ascii"),
        "bytes": len(data),
    })
else:
    fail("unknown workspace op: " + op)
"#;

fn reply(
    source: &crate::conversation_new::ServiceAddr,
    target: &crate::conversation_new::ServiceAddr,
    payload: serde_json::Value,
    response_id: Option<String>,
) -> ServiceCall {
    ServiceCall::response_to(source.clone(), target.clone(), payload, response_id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation_new::{ConversationRef, ServiceRefs};

    fn test_ctx(name: &str) -> ServiceRunContext {
        let root = std::env::temp_dir().join(format!(
            "stellaclaw-workspace-service-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("conversation/.stellaclaw"))
            .expect("conversation root should exist");
        let (_inbox_tx, inbox) = crossbeam_channel::unbounded();
        let (outbox, _outbox_rx) = crossbeam_channel::unbounded();
        let (_stop_tx, stop_rx) = crossbeam_channel::unbounded();
        ServiceRunContext {
            addr: crate::conversation_new::ServiceAddr::workspace(),
            conversation: ConversationRef {
                conversation_id: name.to_string(),
                workdir: root.clone(),
                conversation_root: root.join("conversation"),
            },
            storage: root.join("storage"),
            refs: ServiceRefs::default(),
            inbox,
            outbox,
            stop_rx,
        }
    }

    #[test]
    fn selectable_workspace_can_write_read_and_list_local_files() {
        let ctx = test_ctx("selectable_local_ops");
        let config = ConversationRuntimeConfig::for_conversation(&ctx.conversation);

        let write = write_workspace_file(
            &ctx,
            &config,
            "notes/today.txt",
            WorkspaceTarget::Auto,
            WorkspaceFileEncoding::Utf8,
            "hello",
            true,
            false,
        )
        .expect("write should succeed");
        assert!(matches!(
            write,
            WorkspaceResponse::WriteCompleted {
                bytes_written: 5,
                ..
            }
        ));

        let read = read_workspace_file(
            &ctx,
            &config,
            "notes/today.txt",
            WorkspaceTarget::Auto,
            0,
            None,
        )
        .expect("read should succeed");
        assert!(matches!(
            read,
            WorkspaceResponse::File {
                encoding: WorkspaceFileEncoding::Utf8,
                data,
                ..
            } if data == "hello"
        ));

        let list =
            list_workspace(&ctx, &config, "notes", WorkspaceTarget::Auto, 20).expect("list works");
        assert!(matches!(
            list,
            WorkspaceResponse::Listing { entries, .. }
                if entries.iter().any(|entry| entry.name == "today.txt")
        ));
    }

    #[test]
    fn fixed_ssh_auto_routes_stellaclaw_paths_to_local_overlay() {
        let ctx = test_ctx("fixed_overlay_routing");
        let mut config = ConversationRuntimeConfig::for_conversation(&ctx.conversation);
        config.tool_remote_mode = ToolRemoteMode::FixedSsh {
            host: "devbox".to_string(),
            cwd: Some("/repo".to_string()),
        };

        let overlay = resolve_workspace_target(
            &ctx,
            &config,
            WorkspaceTarget::Auto,
            &normalize_workspace_path(".stellaclaw/attachments/a.txt").unwrap(),
        )
        .expect("overlay should resolve");
        assert!(matches!(
            overlay,
            ResolvedWorkspaceTarget::LocalOverlay { .. }
        ));

        let remote = resolve_workspace_target(
            &ctx,
            &config,
            WorkspaceTarget::Auto,
            &normalize_workspace_path("src/main.rs").unwrap(),
        )
        .expect("remote should resolve");
        assert!(matches!(remote, ResolvedWorkspaceTarget::Remote { .. }));
    }

    #[test]
    fn remote_workspace_helper_command_shell_quotes_python_script() {
        let command = remote_workspace_helper_command();
        assert!(command.starts_with("python3 -c '"));
        assert!(!command.contains("def entry(path, rel):"));
        assert!(!command.contains("if op == \"list\":"));
        assert!(command.contains(&general_purpose::STANDARD.encode(REMOTE_WORKSPACE_SCRIPT)));
    }

    #[test]
    fn workspace_paths_reject_parent_traversal() {
        let error = normalize_workspace_path("../secret.txt").expect_err("path should fail");
        assert!(error.to_string().contains("parent"));
    }

    #[test]
    fn local_archive_upload_and_download_round_trip() {
        let ctx = test_ctx("archive_round_trip");
        let config = ConversationRuntimeConfig::for_conversation(&ctx.conversation);
        let archive = test_tar_gz("report.txt", b"hello archive");

        let upload = upload_workspace_archive(
            &ctx,
            &config,
            "uploads",
            WorkspaceTarget::Auto,
            WorkspaceFileEncoding::Base64,
            &general_purpose::STANDARD.encode(&archive),
        )
        .expect("upload should succeed");
        assert!(matches!(
            upload,
            WorkspaceResponse::ArchiveUploaded {
                entries_extracted: 1,
                ..
            }
        ));
        assert_eq!(
            fs::read_to_string(
                ctx.conversation
                    .conversation_root
                    .join("uploads/report.txt")
            )
            .expect("uploaded file should exist"),
            "hello archive"
        );

        let download = download_workspace_archive(
            &ctx,
            &config,
            &["uploads/report.txt".to_string()],
            WorkspaceTarget::Auto,
        )
        .expect("download should succeed");
        let WorkspaceResponse::ArchiveDownloaded { data, bytes, .. } = download else {
            panic!("expected downloaded archive");
        };
        assert!(bytes > 0);
        let downloaded = general_purpose::STANDARD
            .decode(data)
            .expect("archive should be base64");
        assert!(archive_contains_file(
            &downloaded,
            "uploads/report.txt",
            b"hello archive"
        ));
    }

    fn test_tar_gz(path: &str, data: &[u8]) -> Vec<u8> {
        let mut output = Vec::new();
        {
            let encoder = GzEncoder::new(&mut output, Compression::fast());
            let mut builder = tar::Builder::new(encoder);
            let mut header = tar::Header::new_gnu();
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, data)
                .expect("archive entry should append");
            builder.finish().expect("archive should finish");
        }
        output
    }

    fn archive_contains_file(archive_data: &[u8], path: &str, expected: &[u8]) -> bool {
        let decoder = GzDecoder::new(archive_data);
        let mut archive = tar::Archive::new(decoder);
        let entries = archive.entries().expect("archive entries should read");
        for entry in entries {
            let mut entry = entry.expect("archive entry should read");
            if entry.path().expect("path should read").as_ref() != Path::new(path) {
                continue;
            }
            let mut data = Vec::new();
            entry
                .read_to_end(&mut data)
                .expect("entry data should read");
            return data == expected;
        }
        false
    }
}
