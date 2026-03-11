use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post, put};
use axum::Router;
use rust_embed::Embed;
use serde::{Deserialize, Serialize};
use specta::Type;
use tokio::task::JoinHandle;
use tower_http::cors::{Any, CorsLayer};

use crate::common::chatlog::ChatLog;
use crate::common::config::DaemonConfig;
use crate::common::status::StatusTracker;
use crate::scheduler::Scheduler;

// ---------------------------------------------------------------------------
// Embedded React build
// ---------------------------------------------------------------------------

#[derive(Embed)]
#[folder = "web/dist"]
struct WebAssets;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

pub struct WebState {
    pub status: Arc<StatusTracker>,
    pub scheduler: Arc<Scheduler>,
    pub chat_log: Arc<ChatLog>,
    pub config: Arc<DaemonConfig>,
    pub data_dir: Arc<Path>,
}

// ---------------------------------------------------------------------------
// Spawn
// ---------------------------------------------------------------------------

pub fn spawn(
    port: u16,
    status: Arc<StatusTracker>,
    scheduler: Arc<Scheduler>,
    chat_log: Arc<ChatLog>,
    config: Arc<DaemonConfig>,
    data_dir: Arc<Path>,
) -> JoinHandle<()> {
    let state = Arc::new(WebState {
        status,
        scheduler,
        chat_log,
        config,
        data_dir,
    });

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let api = Router::new()
        .route("/status", get(api_status))
        .route("/chat", get(api_chat))
        .route("/jobs", get(api_list_jobs))
        .route("/jobs", post(api_create_job))
        .route("/jobs/{id}", delete(api_delete_job))
        .route("/jobs/{id}", put(api_update_job))
        .route("/soul", get(api_get_soul))
        .route("/soul", put(api_put_soul));

    let app = Router::new()
        .nest("/api", api)
        .fallback(static_handler)
        .layer(cors)
        .with_state(state);

    tokio::spawn(async move {
        let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
        tracing::info!(%addr, "Web UI server starting");
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => l,
            Err(e) => {
                tracing::error!(error = %e, "Failed to bind web server");
                return;
            }
        };
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!(error = %e, "Web server error");
        }
    })
}

// ---------------------------------------------------------------------------
// Static file handler (serves embedded React build, SPA fallback)
// ---------------------------------------------------------------------------

async fn static_handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');

    // Try exact file match first
    if let Some(file) = WebAssets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        return (
            StatusCode::OK,
            [(header::CONTENT_TYPE, mime.as_ref())],
            file.data,
        )
            .into_response();
    }

    // SPA fallback: serve index.html for non-file routes
    match WebAssets::get("index.html") {
        Some(file) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "text/html")],
            file.data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "Web UI not built. Run: cd web && npm run build").into_response(),
    }
}

// ---------------------------------------------------------------------------
// API handlers
// ---------------------------------------------------------------------------

async fn api_status(State(state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let snap = state.status.snapshot();
    Json(serde_json::to_value(snap).unwrap_or_default())
}

async fn api_chat(State(state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let entries = state.chat_log.entries();
    Json(serde_json::to_value(entries).unwrap_or_default())
}

async fn api_list_jobs(State(state): State<Arc<WebState>>) -> Json<serde_json::Value> {
    let jobs = state.scheduler.list_jobs().await;
    Json(serde_json::to_value(jobs).unwrap_or_default())
}

#[derive(Deserialize)]
struct CreateJobRequest {
    name: String,
    cron_expression: String,
    #[serde(flatten)]
    action_input: JobActionInput,
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum JobActionInput {
    ClaudePrompt { prompt: String },
    TelegramMessage { text: String },
}

async fn api_create_job(
    State(state): State<Arc<WebState>>,
    Json(body): Json<CreateJobRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let admin = state.config.admin_chat_id;
    let action = match body.action_input {
        JobActionInput::ClaudePrompt { prompt } => {
            crate::scheduler::JobAction::ClaudePrompt { prompt, chat_id: admin }
        }
        JobActionInput::TelegramMessage { text } => {
            crate::scheduler::JobAction::TelegramAdmin { text }
        }
    };
    match state
        .scheduler
        .add_job(body.name, body.cron_expression, action, false)
        .await
    {
        Ok(id) => Ok(Json(serde_json::json!({ "id": id }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

#[derive(Deserialize)]
struct UpdateJobRequest {
    #[serde(default)]
    cron_expression: Option<String>,
    #[serde(flatten)]
    action_input: Option<JobActionInput>,
}

async fn api_update_job(
    State(state): State<Arc<WebState>>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
    Json(body): Json<UpdateJobRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let admin = state.config.admin_chat_id;
    let action = body.action_input.map(|a| match a {
        JobActionInput::ClaudePrompt { prompt } => {
            crate::scheduler::JobAction::ClaudePrompt { prompt, chat_id: admin }
        }
        JobActionInput::TelegramMessage { text } => {
            crate::scheduler::JobAction::TelegramAdmin { text }
        }
    });
    match state.scheduler.update_job(id, body.cron_expression, action).await {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => Err((StatusCode::BAD_REQUEST, e.to_string())),
    }
}

async fn api_delete_job(
    State(state): State<Arc<WebState>>,
    axum::extract::Path(id): axum::extract::Path<uuid::Uuid>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    match state.scheduler.remove_job(id).await {
        Ok(()) => Ok(Json(serde_json::json!({ "ok": true }))),
        Err(e) => Err((StatusCode::NOT_FOUND, e.to_string())),
    }
}

#[derive(Serialize, Type)]
pub struct SoulResponse {
    pub files: Vec<SoulFile>,
}

#[derive(Serialize, Deserialize, Type)]
pub struct SoulFile {
    pub name: String,
    pub content: String,
}

async fn api_get_soul(State(state): State<Arc<WebState>>) -> Json<SoulResponse> {
    let prompts_dir = state.data_dir.join("prompts");
    let mut files = Vec::new();

    if let Ok(mut rd) = tokio::fs::read_dir(&prompts_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(content) = tokio::fs::read_to_string(&path).await {
                    let name = path
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    files.push(SoulFile { name, content });
                }
            }
        }
    }

    files.sort_by(|a, b| a.name.cmp(&b.name));
    Json(SoulResponse { files })
}

#[derive(Deserialize)]
struct PutSoulRequest {
    files: Vec<SoulFile>,
}

async fn api_put_soul(
    State(state): State<Arc<WebState>>,
    Json(body): Json<PutSoulRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let prompts_dir = state.data_dir.join("prompts");
    tokio::fs::create_dir_all(&prompts_dir)
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;

    // Remove existing .md files not in the new set
    if let Ok(mut rd) = tokio::fs::read_dir(&prompts_dir).await {
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                let name = path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned();
                if !body.files.iter().any(|f| f.name == name) {
                    let _ = tokio::fs::remove_file(&path).await;
                }
            }
        }
    }

    // Write new/updated files
    for file in &body.files {
        // Sanitize filename to prevent path traversal
        let name = Path::new(&file.name)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.is_empty() || !name.ends_with(".md") {
            continue;
        }
        let path = prompts_dir.join(&name);
        tokio::fs::write(&path, &file.content)
            .await
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    }

    Ok(Json(serde_json::json!({ "ok": true })))
}

#[cfg(test)]
mod tests {
    /// Run `cargo test export_bindings` to regenerate `web/src/types/bindings.ts`.
    #[test]
    fn export_bindings() {
        use specta::TypeCollection;
        use specta_typescript::{BigIntExportBehavior, Typescript};

        let types = TypeCollection::default()
            .register::<crate::common::status::RuntimeStatus>()
            .register::<crate::common::chatlog::ChatEntry>()
            .register::<crate::scheduler::JobRecord>()
            .register::<super::SoulResponse>();

        let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("web/src/types/bindings.ts");

        std::fs::create_dir_all(out.parent().unwrap()).unwrap();
        Typescript::new()
            .bigint(BigIntExportBehavior::Number)
            .header("// This file is auto-generated by specta. Do not edit.")
            .export_to(&out, &types)
            .expect("Failed to export TypeScript bindings");
    }
}
