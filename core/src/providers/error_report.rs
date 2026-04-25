use serde::{Deserialize, Serialize};

use super::{ProviderError, ProviderFailureKind};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderErrorReport {
    MissingApiKeyEnv {
        env: String,
        message: String,
    },
    BuildHttpClient {
        message: String,
    },
    Request {
        message: String,
    },
    HttpStatus {
        url: String,
        status: u16,
        body: String,
        message: String,
    },
    DecodeResponse {
        message: String,
    },
    DecodeJson {
        message: String,
    },
    InvalidResponse {
        message: String,
    },
    ProviderFailure {
        failure_kind: ProviderFailureKind,
        message: String,
        body: String,
    },
    WebSocket {
        message: String,
    },
    PersistOutput {
        message: String,
    },
    EmptyChoices {
        message: String,
    },
    Subprocess {
        message: String,
    },
}

impl ProviderErrorReport {
    pub fn from_provider_error(error: ProviderError) -> Self {
        match error {
            ProviderError::MissingApiKeyEnv(env) => Self::MissingApiKeyEnv {
                message: format!("missing api key in environment variable {env}"),
                env,
            },
            ProviderError::BuildHttpClient(error) => Self::BuildHttpClient {
                message: error.to_string(),
            },
            ProviderError::Request(message) => Self::Request { message },
            ProviderError::HttpStatus { url, status, body } => Self::HttpStatus {
                message: format!("request to {url} failed with status {status}: {body}"),
                url,
                status,
                body,
            },
            ProviderError::DecodeResponse(error) => Self::DecodeResponse {
                message: error.to_string(),
            },
            ProviderError::DecodeJson(error) => Self::DecodeJson {
                message: error.to_string(),
            },
            ProviderError::InvalidResponse(message) => Self::InvalidResponse { message },
            ProviderError::ProviderFailure {
                kind,
                message,
                body,
            } => Self::ProviderFailure {
                failure_kind: kind,
                message,
                body,
            },
            ProviderError::WebSocket(message) => Self::WebSocket { message },
            ProviderError::PersistOutput(error) => Self::PersistOutput {
                message: error.to_string(),
            },
            ProviderError::EmptyChoices => Self::EmptyChoices {
                message: "provider response did not include any completion choices".to_string(),
            },
            ProviderError::Subprocess(message) => Self::Subprocess { message },
        }
    }

    pub fn into_provider_error(self) -> ProviderError {
        match self {
            Self::MissingApiKeyEnv { env, .. } => ProviderError::MissingApiKeyEnv(env),
            Self::BuildHttpClient { message }
            | Self::DecodeResponse { message }
            | Self::DecodeJson { message }
            | Self::PersistOutput { message } => ProviderError::Subprocess(message),
            Self::Request { message } => ProviderError::Request(message),
            Self::HttpStatus {
                url, status, body, ..
            } => ProviderError::HttpStatus { url, status, body },
            Self::InvalidResponse { message } => ProviderError::InvalidResponse(message),
            Self::ProviderFailure {
                failure_kind,
                message,
                body,
            } => ProviderError::ProviderFailure {
                kind: failure_kind,
                message,
                body,
            },
            Self::WebSocket { message } => ProviderError::WebSocket(message),
            Self::EmptyChoices { .. } => ProviderError::EmptyChoices,
            Self::Subprocess { message } => ProviderError::Subprocess(message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderError, ProviderErrorReport, ProviderFailureKind};

    #[test]
    fn provider_error_report_round_trips_request_too_large_kind() {
        let error = ProviderError::ProviderFailure {
            kind: ProviderFailureKind::RequestTooLarge,
            message: "payload too large".to_string(),
            body: "{\"error\":{\"code\":413}}".to_string(),
        };

        let report = ProviderErrorReport::from_provider_error(error);
        let restored = report.into_provider_error();

        assert!(restored.is_request_too_large());
        assert!(matches!(
            restored,
            ProviderError::ProviderFailure {
                kind: ProviderFailureKind::RequestTooLarge,
                ..
            }
        ));
    }

    #[test]
    fn provider_error_report_preserves_http_status() {
        let error = ProviderError::HttpStatus {
            url: "https://example.invalid".to_string(),
            status: 413,
            body: "request exceeds the maximum size".to_string(),
        };

        let report = ProviderErrorReport::from_provider_error(error);
        let restored = report.into_provider_error();

        assert!(restored.is_request_too_large());
        assert!(matches!(
            restored,
            ProviderError::HttpStatus { status: 413, .. }
        ));
    }
}
