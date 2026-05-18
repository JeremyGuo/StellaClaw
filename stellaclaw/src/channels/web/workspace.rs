use std::{collections::HashMap, time::Duration};

use crate::service_protos::{
    channel::{ChannelEvent as KernelChannelEvent, ChannelIngress},
    workspace::{WorkspaceFileEncoding, WorkspaceRequest, WorkspaceResponse, WorkspaceTarget},
};
use base64::{engine::general_purpose, Engine as _};

use super::{
    channel::{wait_for_event, MoveWorkspacePathRequest, WebChannel},
    http::{parse_json, query_u64, HttpError, HttpResponse, HttpResult},
    time_utils::generated_request_id,
};

impl WebChannel {
    pub(super) fn list_workspace(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> HttpResult {
        let path = query.get("path").filter(|path| !path.is_empty()).cloned();
        let limit = query.get("limit").and_then(|value| value.parse().ok());
        self.workspace_response(
            conversation_id,
            WorkspaceRequest::List {
                path,
                target: WorkspaceTarget::Auto,
                limit,
            },
        )
    }

    pub(super) fn read_workspace_file(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> HttpResult {
        let path = query
            .get("path")
            .filter(|path| !path.is_empty())
            .cloned()
            .ok_or_else(|| HttpError::new(400, "path is required"))?;
        self.workspace_response(
            conversation_id,
            WorkspaceRequest::ReadFile {
                path,
                target: WorkspaceTarget::Auto,
                offset: query_u64(query, "offset"),
                limit_bytes: query
                    .get("limit_bytes")
                    .and_then(|value| value.parse().ok()),
            },
        )
    }

    pub(super) fn delete_workspace_path(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> HttpResult {
        let path = query
            .get("path")
            .filter(|path| !path.is_empty())
            .cloned()
            .ok_or_else(|| HttpError::new(400, "path is required"))?;
        self.workspace_response(
            conversation_id,
            WorkspaceRequest::DeletePath {
                path,
                target: WorkspaceTarget::Auto,
            },
        )
    }

    pub(super) fn move_workspace_path(&self, conversation_id: &str, body: &[u8]) -> HttpResult {
        let request: MoveWorkspacePathRequest = parse_json(body)?;
        self.workspace_response(
            conversation_id,
            WorkspaceRequest::MovePath {
                from_path: request.path,
                to_path: request.new_path,
                target: WorkspaceTarget::Auto,
            },
        )
    }

    pub(super) fn upload_workspace_archive(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
        body: &[u8],
    ) -> HttpResult {
        let dir_path = query.get("path").cloned().unwrap_or_default();
        self.workspace_response(
            conversation_id,
            WorkspaceRequest::UploadArchive {
                dir_path,
                target: WorkspaceTarget::Auto,
                encoding: WorkspaceFileEncoding::Base64,
                data: general_purpose::STANDARD.encode(body),
            },
        )
    }

    pub(super) fn download_workspace_archive(
        &self,
        conversation_id: &str,
        query: &HashMap<String, String>,
    ) -> HttpResult {
        let path = query
            .get("path")
            .filter(|path| !path.is_empty())
            .cloned()
            .ok_or_else(|| HttpError::new(400, "path is required"))?;
        let response = self.workspace_response_value(
            conversation_id,
            WorkspaceRequest::DownloadArchive {
                paths: vec![path],
                target: WorkspaceTarget::Auto,
            },
        )?;
        let WorkspaceResponse::ArchiveDownloaded { encoding, data, .. } = response else {
            return Err(HttpError::new(
                500,
                "workspace service returned unexpected download response",
            ));
        };
        let body = match encoding {
            WorkspaceFileEncoding::Base64 => general_purpose::STANDARD
                .decode(data)
                .map_err(HttpError::internal)?,
            WorkspaceFileEncoding::Utf8 => data.into_bytes(),
        };
        Ok(HttpResponse::bytes(200, "application/gzip", body))
    }

    fn workspace_response(&self, conversation_id: &str, request: WorkspaceRequest) -> HttpResult {
        let response = self.workspace_response_value(conversation_id, request)?;
        let status = if matches!(response, WorkspaceResponse::Error { .. }) {
            400
        } else {
            200
        };
        Ok(HttpResponse::json(
            status,
            serde_json::to_value(response).map_err(HttpError::internal)?,
        ))
    }

    fn workspace_response_value(
        &self,
        conversation_id: &str,
        request: WorkspaceRequest,
    ) -> HttpResult<WorkspaceResponse> {
        self.conversation_runtime
            .ensure_conversation_started(conversation_id)
            .map_err(HttpError::internal)?;
        let request_id = generated_request_id("workspace");
        let rx = self
            .conversation_runtime
            .send_main_channel_ingress_subscribed(
                conversation_id,
                ChannelIngress::Workspace {
                    request_id: request_id.clone(),
                    request,
                },
            )
            .map_err(HttpError::internal)?;
        wait_for_event(&rx, Duration::from_secs(30), |event| match event {
            KernelChannelEvent::Workspace {
                request_id: id,
                response,
            } if id == request_id => Some(response),
            _ => None,
        })
    }
}
