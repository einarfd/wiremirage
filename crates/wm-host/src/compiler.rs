//! HTTP client for the compiler sidecar.
//!
//! The sidecar takes handler source over `POST /compile` and returns
//! componentized wasm bytes. This client wraps that interface, validates
//! the bindings_version, and surfaces compile diagnostics back to the API
//! layer.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::SUPPORTED_BINDINGS_VERSION;

/// Sidecar HTTP client. Cheap to clone (the inner reqwest client is
/// internally Arc-backed).
#[derive(Clone)]
pub struct CompilerClient {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum CompilerError {
    #[error("compiler request failed: {0}")]
    Network(String),
    #[error("compiler returned an unexpected response: {0}")]
    BadResponse(String),
    #[error("compile failed: {message}")]
    CompileFailed {
        message: String,
        diagnostics: Vec<String>,
    },
    #[error("bindings_version mismatch: compiler returned {got:?}, host expects {expected:?}")]
    BindingsMismatch { got: String, expected: String },
}

#[derive(Debug, Clone)]
pub struct CompiledArtifact {
    pub component: Vec<u8>,
    pub bindings_version: String,
}

#[derive(Debug, Serialize)]
struct CompileRequest<'a> {
    language: &'a str,
    source: &'a str,
}

#[derive(Debug, Deserialize)]
struct CompileSuccess {
    compiled_wasm: String,
    bindings_version: String,
}

#[derive(Debug, Deserialize)]
struct CompileErrorBody {
    error: CompileErrorDetail,
}

#[derive(Debug, Deserialize)]
struct CompileErrorDetail {
    code: String,
    message: String,
    #[serde(default)]
    diagnostics: Vec<String>,
}

impl CompilerClient {
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            base_url: base_url.into(),
            http: reqwest::Client::new(),
        }
    }

    /// Construct a client from `WM_COMPILER_URL`. Returns `None` if the
    /// env var is not set; the API layer turns that into a fail-fast
    /// `compile_failed` response when a source-based request lands.
    pub fn from_env() -> Option<Self> {
        std::env::var("WM_COMPILER_URL").ok().map(Self::new)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub async fn compile(
        &self,
        language: &str,
        source: &str,
    ) -> Result<CompiledArtifact, CompilerError> {
        let url = format!("{}/compile", self.base_url.trim_end_matches('/'));
        let response = self
            .http
            .post(&url)
            .json(&CompileRequest { language, source })
            .send()
            .await
            .map_err(|e| CompilerError::Network(format!("{e}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| CompilerError::Network(format!("{e}")))?;

        if status.is_success() {
            let ok: CompileSuccess = serde_json::from_slice(&bytes)
                .map_err(|e| CompilerError::BadResponse(format!("decode 200 body: {e}")))?;
            if ok.bindings_version != SUPPORTED_BINDINGS_VERSION {
                return Err(CompilerError::BindingsMismatch {
                    got: ok.bindings_version,
                    expected: SUPPORTED_BINDINGS_VERSION.to_string(),
                });
            }
            let component = B64
                .decode(ok.compiled_wasm.as_bytes())
                .map_err(|e| CompilerError::BadResponse(format!("compiled_wasm base64: {e}")))?;
            return Ok(CompiledArtifact {
                component,
                bindings_version: ok.bindings_version,
            });
        }

        // Try to parse the structured error envelope; fall back to raw
        // bytes if the sidecar returned something unexpected.
        match serde_json::from_slice::<CompileErrorBody>(&bytes) {
            Ok(err) if err.error.code == "compile_failed" => Err(CompilerError::CompileFailed {
                message: err.error.message,
                diagnostics: err.error.diagnostics,
            }),
            Ok(err) => Err(CompilerError::BadResponse(format!(
                "{}: {}",
                err.error.code, err.error.message
            ))),
            Err(_) => Err(CompilerError::BadResponse(format!(
                "non-JSON {} body: {}",
                status,
                String::from_utf8_lossy(&bytes)
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tier-1 tests against an in-process axum server standing in for the
    //! sidecar. These cover the client's request shape, the success
    //! decoding path, the various error paths, and the bindings_version
    //! check — without depending on Docker or Node.

    use super::*;
    use axum::Router;
    use axum::extract::State;
    use axum::routing::post;
    use std::sync::Arc;

    /// A minimal mock sidecar that returns a canned response for every
    /// `/compile` call. Use `with_response` to control the response body
    /// and status per test.
    #[derive(Clone)]
    struct MockSidecar {
        response_status: axum::http::StatusCode,
        response_body: serde_json::Value,
    }

    async fn handler(State(state): State<Arc<MockSidecar>>) -> axum::response::Response {
        use axum::Json;
        use axum::response::IntoResponse;
        (state.response_status, Json(state.response_body.clone())).into_response()
    }

    async fn start_mock(state: MockSidecar) -> (CompilerClient, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route("/compile", post(handler))
            .with_state(Arc::new(state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.expect("axum::serve");
        });
        let client = CompilerClient::new(format!("http://{addr}"));
        (client, server)
    }

    #[tokio::test]
    async fn returns_decoded_component_on_success() {
        let canned = b"AGFzbQ"; // wasm magic prefix; decodes to a few bytes
        let (client, server) = start_mock(MockSidecar {
            response_status: axum::http::StatusCode::OK,
            response_body: serde_json::json!({
                "compiled_wasm": B64.encode(canned),
                "bindings_version": SUPPORTED_BINDINGS_VERSION,
            }),
        })
        .await;

        let out = client.compile("typescript", "src").await.unwrap();
        assert_eq!(out.component, canned);
        assert_eq!(out.bindings_version, SUPPORTED_BINDINGS_VERSION);
        server.abort();
    }

    #[tokio::test]
    async fn surfaces_compile_failed_with_diagnostics() {
        let (client, server) = start_mock(MockSidecar {
            response_status: axum::http::StatusCode::BAD_REQUEST,
            response_body: serde_json::json!({
                "error": {
                    "code": "compile_failed",
                    "message": "transpile failed",
                    "diagnostics": ["unexpected token", "missing brace"],
                }
            }),
        })
        .await;

        let err = client.compile("typescript", "bad").await.unwrap_err();
        match err {
            CompilerError::CompileFailed {
                message,
                diagnostics,
            } => {
                assert_eq!(message, "transpile failed");
                assert_eq!(diagnostics.len(), 2);
            }
            other => panic!("expected CompileFailed, got {other:?}"),
        }
        server.abort();
    }

    #[tokio::test]
    async fn rejects_mismatched_bindings_version() {
        let (client, server) = start_mock(MockSidecar {
            response_status: axum::http::StatusCode::OK,
            response_body: serde_json::json!({
                "compiled_wasm": B64.encode(b"AGFzbQ"),
                "bindings_version": "9.9.9",
            }),
        })
        .await;
        let err = client.compile("typescript", "src").await.unwrap_err();
        assert!(matches!(err, CompilerError::BindingsMismatch { .. }));
        server.abort();
    }

    #[tokio::test]
    async fn unknown_error_code_becomes_bad_response() {
        let (client, server) = start_mock(MockSidecar {
            response_status: axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            response_body: serde_json::json!({
                "error": { "code": "internal_error", "message": "boom" }
            }),
        })
        .await;
        let err = client.compile("typescript", "src").await.unwrap_err();
        assert!(matches!(err, CompilerError::BadResponse(_)));
        server.abort();
    }
}
