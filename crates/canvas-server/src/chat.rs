//! The in-app chat: canvas-server is just another OpenCode client.
//!
//! It discovers the local OpenCode background service from its registration
//! file, creates one session for the canvas at the project directory, and
//! forwards prompts. That session already has the canvas MCP tools from the
//! project config, so the agent talks and draws in the same window.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Extension, Json};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{json, Value};
use tokio::sync::Mutex;

/// Where the OpenCode background service listens and how to authenticate.
#[derive(Clone, Debug, PartialEq)]
pub struct ServiceEndpoint {
    pub url: String,
    pub password: String,
}

impl ServiceEndpoint {
    /// Read the standard registration file the CLI and clients use.
    pub fn read_registration(path: &Path) -> Option<Self> {
        let json = std::fs::read_to_string(path).ok()?;
        let value: Value = serde_json::from_str(&json).ok()?;
        let url = value.get("url")?.as_str()?.to_string();
        let password = value.get("password")?.as_str()?.to_string();
        Some(Self { url, password })
    }

    pub fn discover() -> Option<Self> {
        if let Ok(path) = std::env::var("OPENCODE_SERVICE_FILE") {
            return Self::read_registration(Path::new(&path));
        }
        let home = std::env::var("HOME").ok()?;
        Self::read_registration(&PathBuf::from(home).join(".local/state/opencode/service.json"))
    }
}

/// The canvas chat is pinned to this model for now; the service default may
/// be unavailable in the user's region.
const PINNED_MODEL_ID: &str = "deepseek-v4.1-flash";
const PINNED_MODEL_PROVIDER: &str = "opencode-go";

/// One canvas chat thread, backed by an OpenCode session.
pub struct ChatBridge {
    http: reqwest::Client,
    project_dir: PathBuf,
    endpoint: Mutex<Option<ServiceEndpoint>>,
    session: Mutex<Option<String>>,
}

impl ChatBridge {
    pub fn new(project_dir: PathBuf) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            project_dir,
            endpoint: Mutex::new(None),
            session: Mutex::new(None),
        }
    }

    async fn endpoint(&self) -> Result<ServiceEndpoint, String> {
        if let Some(endpoint) = self.endpoint.lock().await.clone() {
            return Ok(endpoint);
        }
        let endpoint = ServiceEndpoint::discover().ok_or_else(|| {
            "the OpenCode service was not found; start opencode2 once so it registers".to_string()
        })?;
        *self.endpoint.lock().await = Some(endpoint.clone());
        Ok(endpoint)
    }

    /// Forget a stale endpoint so the next call rediscovers (the service
    /// restarts on a new port and with a new password).
    async fn forget_endpoint(&self) {
        *self.endpoint.lock().await = None;
        *self.session.lock().await = None;
    }

    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let endpoint = self.endpoint().await?;
        let mut request = self
            .http
            .request(method.clone(), format!("{}{path}", endpoint.url))
            .basic_auth("opencode", Some(&endpoint.password));
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => {
                self.forget_endpoint().await;
                return Err(format!("cannot reach the OpenCode service: {error}"));
            }
        };
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            // A stale registration often shows up as 401 or a refused port.
            if status == reqwest::StatusCode::UNAUTHORIZED {
                self.forget_endpoint().await;
            }
            return Err(format!(
                "OpenCode service error {status}: {}",
                truncate(&text)
            ));
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    /// The models the user can pick, trimmed for the panel, plus the default.
    pub async fn models(&self) -> Result<Value, String> {
        let list = self
            .request(reqwest::Method::GET, "/api/model", None)
            .await?;
        let models: Vec<Value> = list["data"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|model| {
                json!({
                    "id": model["id"],
                    "providerID": model["providerID"],
                    "name": model["name"],
                })
            })
            .collect();
        let default = self
            .request(reqwest::Method::GET, "/api/model/default", None)
            .await
            .unwrap_or(Value::Null);
        Ok(json!({
            "models": models,
            "default": {
                "id": default["data"]["id"],
                "providerID": default["data"]["providerID"],
                "name": default["data"]["name"],
            },
        }))
    }

    async fn ensure_session(&self) -> Result<String, String> {
        if let Some(session) = self.session.lock().await.clone() {
            if self
                .request(
                    reqwest::Method::GET,
                    &format!("/api/session/{session}"),
                    None,
                )
                .await
                .is_ok()
            {
                return Ok(session);
            }
        }
        let model = self.pinned_model().await?;
        let body = json!({
            "title": "Canvas chat",
            "location": { "directory": self.project_dir },
            "model": model,
        });
        let created = self
            .request(reqwest::Method::POST, "/api/session", Some(body))
            .await?;
        let session = created["data"]["id"]
            .as_str()
            .ok_or_else(|| "the OpenCode service did not return a session".to_string())?
            .to_string();
        *self.session.lock().await = Some(session.clone());
        Ok(session)
    }

    /// The pinned canvas-chat model; errors when the service doesn't offer it.
    async fn pinned_model(&self) -> Result<Value, String> {
        let list = self
            .request(reqwest::Method::GET, "/api/model", None)
            .await?;
        let models = list["data"].as_array().cloned().unwrap_or_default();
        let found = models
            .iter()
            .find(|model| {
                model["id"] == PINNED_MODEL_ID && model["providerID"] == PINNED_MODEL_PROVIDER
            })
            .or_else(|| models.iter().find(|model| model["id"] == PINNED_MODEL_ID))
            .ok_or_else(|| {
                format!(
                    "the pinned chat model {PINNED_MODEL_ID} is not offered by the OpenCode service"
                )
            })?;
        Ok(json!({ "id": found["id"], "providerID": found["providerID"] }))
    }

    async fn newest_message_time(&self, session: &str) -> f64 {
        let messages = self
            .request(
                reqwest::Method::GET,
                &format!("/api/session/{session}/message?order=desc&limit=1"),
                None,
            )
            .await
            .unwrap_or(Value::Null);
        messages["data"]
            .as_array()
            .and_then(|data| data.first())
            .and_then(|message| message["time"]["created"].as_f64())
            .unwrap_or(0.0)
    }

    /// Send one prompt and wait for the assistant's completed reply.
    pub async fn send(&self, text: &str) -> Result<String, String> {
        let session = self.ensure_session().await?;
        let baseline = self.newest_message_time(&session).await;
        self.request(
            reqwest::Method::POST,
            &format!("/api/session/{session}/prompt"),
            Some(json!({ "text": text, "resume": true })),
        )
        .await?;

        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if Instant::now() > deadline {
                return Err("the agent took too long to answer".into());
            }
            tokio::time::sleep(Duration::from_millis(600)).await;
            let messages = self
                .request(
                    reqwest::Method::GET,
                    &format!("/api/session/{session}/message?order=desc&limit=10"),
                    None,
                )
                .await?;
            let Some(assistant) = messages["data"].as_array().and_then(|data| {
                data.iter().find(|message| {
                    message["type"] == "assistant"
                        && message["time"]["created"].as_f64().unwrap_or(0.0) > baseline
                })
            }) else {
                continue;
            };
            if assistant["time"]["completed"].as_f64().is_none() {
                continue;
            }
            if let Some(error) = assistant.get("error").filter(|error| !error.is_null()) {
                return Err(error["message"]
                    .as_str()
                    .unwrap_or("the model request failed")
                    .to_string());
            }
            let reply = assistant["content"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .filter(|part| part["type"] == "text")
                        .filter_map(|part| part["text"].as_str())
                        .collect::<Vec<_>>()
                        .join("\n\n")
                })
                .unwrap_or_default();
            if reply.trim().is_empty() {
                // A completed turn with no text is a tool-call turn: the loop
                // is still running, so wait for the next assistant message.
                continue;
            }
            return Ok(reply);
        }
    }
}

fn truncate(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= 300 {
        trimmed.to_string()
    } else {
        format!("{}…", &trimmed[..300.min(trimmed.len())])
    }
}

/// `POST /chat` — send one message to the canvas chat session.
pub async fn chat_handler(
    Extension(bridge): Extension<Arc<ChatBridge>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let text = body
        .get("text")
        .and_then(|text| text.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    if text.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "the message is empty" })),
        );
    }
    // The chat is pinned to one model for now; any client-sent model is ignored.
    match bridge.send(&text).await {
        Ok(reply) => (StatusCode::OK, Json(json!({ "reply": reply }))),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))),
    }
}

/// `GET /chat/models` — the models the user can pick for the chat.
pub async fn models_handler(Extension(bridge): Extension<Arc<ChatBridge>>) -> impl IntoResponse {
    match bridge.models().await {
        Ok(models) => (StatusCode::OK, Json(models)),
        Err(error) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": error }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_files_are_parsed() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-chat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("service.json");
        std::fs::write(
            &path,
            r#"{"url":"http://127.0.0.1:9999","password":"secret","pid":1}"#,
        )
        .unwrap();
        let endpoint = ServiceEndpoint::read_registration(&path).unwrap();
        assert_eq!(endpoint.url, "http://127.0.0.1:9999");
        assert_eq!(endpoint.password, "secret");

        assert!(ServiceEndpoint::read_registration(&dir.join("missing.json")).is_none());
        std::fs::write(&path, "not json").unwrap();
        assert!(ServiceEndpoint::read_registration(&path).is_none());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn chat_reports_a_missing_service_readably() {
        let dir = std::env::temp_dir().join(format!("rs-canvas-chat-miss-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("service.json");
        std::env::set_var("OPENCODE_SERVICE_FILE", &missing);
        let bridge = ChatBridge::new(PathBuf::from("."));
        let error = bridge.send("hello").await.unwrap_err();
        std::env::remove_var("OPENCODE_SERVICE_FILE");
        assert!(error.contains("OpenCode service"), "{error}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
