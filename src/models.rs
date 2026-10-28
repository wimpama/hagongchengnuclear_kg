use crate::{auth::require_admin, error::ApiError, state::AppState};
use aes_gcm::{
    aead::{rand_core::RngCore, Aead, KeyInit, OsRng},
    Aes256Gcm, Nonce,
};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::sync::OnceLock;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelConfig {
    pub kind: String,
    #[serde(default = "default_provider")]
    pub provider: String,
    pub base_url: String,
    pub model: String,
    #[serde(default = "default_dimension")]
    pub dimension: usize,
    #[serde(default = "default_protocol")]
    pub protocol: String,
    #[serde(default)]
    pub vision_enabled: bool,
    #[serde(default = "default_thinking")]
    pub thinking: String,
    #[serde(default = "default_reasoning_effort")]
    pub reasoning_effort: String,
    /// Whether generated answers must pass the citation and independent
    /// support review pipeline. Older persisted JSON predates this setting,
    /// so the default intentionally keeps the strict path enabled.
    #[serde(default = "default_answer_verification")]
    pub answer_verification_enabled: bool,
    /// When verification is disabled, whether to expose the course evidence
    /// actually cited by the answer as provenance. Retrieval itself is
    /// always performed regardless of this setting.
    #[serde(default = "default_answer_provenance_enabled")]
    pub answer_provenance_enabled: bool,
    /// Presentation for provenance when it is enabled: inline source chips or
    /// a source list below the answer.
    #[serde(default = "default_answer_provenance_mode")]
    pub answer_provenance_mode: String,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub instructions: String,
    #[serde(default = "default_fps")]
    pub fps: f64,
    #[serde(default, skip_serializing)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub configured: bool,
}
fn default_provider() -> String {
    "openai".into()
}
fn default_thinking() -> String {
    "disabled".into()
}
fn default_reasoning_effort() -> String {
    "high".into()
}
fn default_answer_verification() -> bool {
    true
}
fn default_answer_provenance_enabled() -> bool {
    true
}
fn default_answer_provenance_mode() -> String {
    "inline".into()
}
fn default_dimension() -> usize {
    1024
}
fn default_protocol() -> String {
    "responses".into()
}
fn default_fps() -> f64 {
    1.0
}
fn enabled() -> bool {
    true
}

impl ModelConfig {
    pub fn fingerprint(&self) -> String {
        format!(
            "{:x}",
            Sha256::digest(
                format!(
                    "{}|{}|{}|{}|{}|{}",
                    self.base_url,
                    self.model,
                    self.dimension,
                    self.instructions,
                    self.fps,
                    "semantic-v1"
                )
                .as_bytes()
            )
        )
    }
    pub fn endpoint(&self, suffix: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), suffix)
    }
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/kb-api/v1/admin/ai/config", get(list))
        .route("/kb-api/v1/admin/ai/config/{kind}", get(get_one).put(save))
        .route("/kb-api/v1/admin/ai/config/test", post(test))
}
fn default_config(kind: &str) -> Result<ModelConfig, ApiError> {
    let provider = if kind == "llm" {
        std::env::var("LLM_PROVIDER").unwrap_or_else(|_| "deepseek".into())
    } else if kind == "embedding" {
        "ark".into()
    } else {
        default_provider()
    };
    let (url, model, key) = match kind {
        "embedding" => (
            std::env::var("ARK_EMBEDDING_BASE_URL").unwrap_or_else(|_| {
                "https://ark.cn-beijing.volces.com/api/v3/embeddings/multimodal".into()
            }),
            std::env::var("ARK_EMBEDDING_MODEL")
                .unwrap_or_else(|_| "doubao-embedding-vision-251215".into()),
            "ARK_API_KEY",
        ),
        "llm" => (
            std::env::var("LLM_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".into()),
            std::env::var("LLM_MODEL").unwrap_or_else(|_| "deepseek-flash".into()),
            if provider == "deepseek" {
                "DEEPSEEK_API_KEY"
            } else {
                "LLM_API_KEY"
            },
        ),
        "asr" => (
            std::env::var("ASR_BASE_URL").unwrap_or_default(),
            std::env::var("ASR_MODEL").unwrap_or_default(),
            "ASR_API_KEY",
        ),
        _ => return Err(ApiError::bad_request("未知模型配置类别")),
    };
    let api_key = std::env::var(key).ok().filter(|k| !k.trim().is_empty());
    Ok(ModelConfig {
        kind: kind.into(),
        provider: provider.clone(),
        base_url: url,
        model,
        dimension: 1024,
        protocol: if kind == "llm" {
            std::env::var("LLM_PROTOCOL").unwrap_or_else(|_| {
                if provider == "deepseek" {
                    "chat".into()
                } else {
                    default_protocol()
                }
            })
        } else {
            default_protocol()
        },
        vision_enabled: kind == "llm"
            && std::env::var("LLM_VISION_ENABLED").unwrap_or_else(|_| "true".into()) == "true",
        thinking: if kind == "llm" {
            std::env::var("LLM_THINKING").unwrap_or_else(|_| {
                if provider == "deepseek" {
                    "enabled".into()
                } else {
                    default_thinking()
                }
            })
        } else {
            default_thinking()
        },
        reasoning_effort: std::env::var("LLM_REASONING_EFFORT")
            .unwrap_or_else(|_| default_reasoning_effort()),
        answer_verification_enabled: true,
        answer_provenance_enabled: true,
        answer_provenance_mode: default_answer_provenance_mode(),
        enabled: kind != "asr",
        instructions: String::new(),
        fps: 1.0,
        configured: api_key.is_some(),
        api_key,
    })
}
pub async fn load(state: &AppState, kind: &str) -> Result<ModelConfig, ApiError> {
    let mut config = default_config(kind)?;
    if let Some(row) =
        sqlx::query("SELECT config_json,encrypted_secret FROM kb_runtime_configs WHERE kind=?")
            .bind(kind)
            .fetch_optional(&state.db)
            .await?
    {
        let env_key = config.api_key.take();
        let env_provider = config.provider.clone();
        config = serde_json::from_str(row.get("config_json")).map_err(|_| ApiError::internal())?;
        let encrypted: Option<Vec<u8>> = row.get("encrypted_secret");
        config.api_key = match encrypted {
            Some(cipher) => Some(decrypt(&cipher)?),
            None if config.provider == env_provider => env_key,
            None => None,
        };
        config.configured = config.api_key.is_some();
    }
    Ok(config)
}
fn cipher() -> Result<Aes256Gcm, ApiError> {
    let key = std::env::var("ENCRYPTION_MASTER_KEY")
        .map_err(|_| ApiError::unavailable("请先设置服务端 ENCRYPTION_MASTER_KEY"))?;
    let bytes = STANDARD
        .decode(key.trim())
        .map_err(|_| ApiError::bad_request("加密主密钥应为32字节的base64"))?;
    Aes256Gcm::new_from_slice(&bytes)
        .map_err(|_| ApiError::bad_request("加密主密钥应为32字节的base64"))
}
fn encrypt(secret: &str) -> Result<Vec<u8>, ApiError> {
    encrypt_using(secret, &cipher()?)
}
fn encrypt_using(secret: &str, cipher: &Aes256Gcm) -> Result<Vec<u8>, ApiError> {
    let mut nonce = [0u8; 12];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(Nonce::from_slice(&nonce), secret.as_bytes())
        .map_err(|_| ApiError::internal())?;
    Ok([nonce.to_vec(), ciphertext].concat())
}
fn decrypt(bytes: &[u8]) -> Result<String, ApiError> {
    decrypt_using(bytes, &cipher()?)
}
fn decrypt_using(bytes: &[u8], cipher: &Aes256Gcm) -> Result<String, ApiError> {
    if bytes.len() < 28 {
        return Err(ApiError::internal());
    }
    let plain = cipher
        .decrypt(Nonce::from_slice(&bytes[..12]), &bytes[12..])
        .map_err(|_| ApiError::unavailable("模型密钥解密失败，请检查主密钥"))?;
    String::from_utf8(plain).map_err(|_| ApiError::internal())
}
fn validate(config: &ModelConfig) -> Result<(), ApiError> {
    if config.kind == "asr" && !config.enabled && config.base_url.trim().is_empty() {
        return Ok(());
    }
    if config.base_url.len() > 2000 || config.model.len() > 160 {
        return Err(ApiError::bad_request("配置过长"));
    }
    let url = reqwest::Url::parse(&config.base_url)
        .map_err(|_| ApiError::bad_request("模型地址不正确"))?;
    let allowed = std::env::var("KB_MODEL_ALLOWED_HOSTS")
        .unwrap_or_else(|_| "ark.cn-beijing.volces.com,cn.bestpudding.xyz,api.deepseek.com".into());
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || !allowed.split(',').any(|v| Some(v.trim()) == url.host_str())
    {
        return Err(ApiError::bad_request(
            "仅允许服务端 KB_MODEL_ALLOWED_HOSTS 中的 HTTPS 模型地址",
        ));
    }
    if config.model.trim().is_empty()
        || ![1024, 2048].contains(&config.dimension)
        || !(0.2..=5.0).contains(&config.fps)
        || !["responses", "chat"].contains(&config.protocol.as_str())
        || !["openai", "deepseek", "ark"].contains(&config.provider.as_str())
        || !["enabled", "disabled"].contains(&config.thinking.as_str())
        || !["none", "low", "high", "max"].contains(&config.reasoning_effort.as_str())
        || !["inline", "append"].contains(&config.answer_provenance_mode.as_str())
    {
        return Err(ApiError::bad_request("模型名、维度、采样率或协议不正确"));
    }
    if config.provider == "deepseek"
        && (config.kind != "llm"
            || config.protocol != "chat"
            || url.host_str() != Some("api.deepseek.com"))
    {
        return Err(ApiError::bad_request(
            "DeepSeek 请使用 api.deepseek.com 的 Chat Completions 协议",
        ));
    }
    Ok(())
}
async fn list(State(state): State<AppState>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers).await?;
    let mut configs = Vec::new();
    for kind in ["embedding", "llm", "asr"] {
        configs.push(load(&state, kind).await?)
    }
    Ok(Json(json!(configs)))
}
async fn get_one(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
) -> Result<Response, ApiError> {
    require_admin(&state, &headers).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(admin_config_view(load(&state, &kind).await?)?),
    )
        .into_response())
}

// Full-key display is confined to the authenticated administrator editor.
// ModelConfig's ordinary serialization (storage, logs, save results) omits it.
fn admin_config_view(config: ModelConfig) -> Result<Value, ApiError> {
    let mut view = serde_json::to_value(&config).map_err(|_| ApiError::internal())?;
    view["apiKey"] = json!(config.api_key.unwrap_or_default());
    Ok(view)
}
async fn save(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(kind): Path<String>,
    Json(mut config): Json<ModelConfig>,
) -> Result<Json<ModelConfig>, ApiError> {
    let user = require_admin(&state, &headers).await?;
    default_config(&kind)?;
    config.kind = kind.clone();
    validate(&config)?;
    let secret = config
        .api_key
        .take()
        .filter(|s| !s.trim().is_empty())
        .map(|s| encrypt(s.trim()))
        .transpose()?;
    let mut tx = state.db.begin().await?;
    let previous: Option<String> =
        sqlx::query_scalar("SELECT config_json FROM kb_runtime_configs WHERE kind=?")
            .bind(&kind)
            .fetch_optional(&mut *tx)
            .await?;
    if kind == "llm" && secret.is_none() && config.enabled {
        let previous_provider = previous
            .as_ref()
            .map(|raw| serde_json::from_str::<ModelConfig>(raw))
            .transpose()
            .map_err(|_| ApiError::internal())?
            .map(|old| old.provider)
            .unwrap_or(default_config(&kind)?.provider);
        if previous_provider != config.provider {
            return Err(ApiError::bad_request(
                "更换模型供应商时请填写该供应商的 API 密钥",
            ));
        }
    }
    let old_fp = match previous {
        Some(raw) => serde_json::from_str::<ModelConfig>(&raw)
            .map_err(|_| ApiError::internal())?
            .fingerprint(),
        None => default_config(&kind)?.fingerprint(),
    };
    if kind == "embedding" && old_fp != config.fingerprint() {
        let published:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_assets WHERE published_revision_id IS NOT NULL AND deleted_at IS NULL").fetch_one(&mut *tx).await?;
        if published > 0 {
            return Err(ApiError::bad_request("已有已发布课程索引，不能直接改变向量模型、维度或切片配置。请先撤回全部资料，再保存配置并重新审核、索引和发布。"));
        }
    }
    let serialized = serde_json::to_string(&config).map_err(|_| ApiError::internal())?;
    sqlx::query("INSERT INTO kb_runtime_configs(kind,config_json,encrypted_secret,updated_by) VALUES(?,?,?,?) ON CONFLICT(kind) DO UPDATE SET config_json=excluded.config_json,encrypted_secret=COALESCE(excluded.encrypted_secret,kb_runtime_configs.encrypted_secret),updated_by=excluded.updated_by,updated_at=CURRENT_TIMESTAMP")
        .bind(&kind).bind(serialized).bind(secret).bind(&user.id).execute(&mut *tx).await?;
    // With no published sources the next publication can activate a fresh space.
    if kind == "embedding" && old_fp != config.fingerprint() {
        sqlx::query("DELETE FROM kb_active_indexes WHERE course_id NOT IN (SELECT n.course_id FROM kb_assets a JOIN kb_course_nodes n ON n.node_id=a.node_id WHERE a.published_revision_id IS NOT NULL AND a.deleted_at IS NULL)").execute(&mut *tx).await?;
    }
    sqlx::query("INSERT INTO kb_audit_events(audit_id,actor_id,action,resource_type,resource_id,detail_json) VALUES(?,?,'model.config','model',?,?)").bind(uuid::Uuid::new_v4().to_string()).bind(&user.id).bind(&kind).bind(json!({"fingerprint":config.fingerprint(),"enabled":config.enabled}).to_string()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(load(&state, &kind).await?))
}
#[derive(Deserialize)]
struct TestRequest {
    kind: String,
}
async fn test(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(input): Json<TestRequest>,
) -> Result<Json<Value>, ApiError> {
    require_admin(&state, &headers).await?;
    let config = load(&state, &input.kind).await?;
    validate(&config)?;
    let started = std::time::Instant::now();
    match input.kind.as_str() {
        "embedding" => {
            let vector = crate::embedding::embed(
                &config,
                vec![json!({"type":"text","text":"科学课程知识检索连接测试"})],
            )
            .await?;
            Ok(Json(
                json!({"ok":true,"dimension":vector.len(),"elapsedMs":started.elapsed().as_millis()}),
            ))
        }
        "llm" => {
            let text = llm_json(
                &config,
                "只返回 JSON：{\"ok\":true}",
                json!([{"role":"user","content":"连接测试"}]),
                json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false}),
            )
            .await?;
            if serde_json::from_str::<Value>(&text)
                .ok()
                .and_then(|v| v.get("ok").and_then(Value::as_bool))
                != Some(true)
            {
                return Err(ApiError::unavailable("网关返回结构不符合预期"));
            }
            Ok(Json(
                json!({"ok":true,"protocol":config.protocol,"visionVerified":false,"elapsedMs":started.elapsed().as_millis()}),
            ))
        }
        _ => Err(ApiError::bad_request(
            "ASR 需要配置供应商适配器；可先上传教师字幕",
        )),
    }
}
pub fn client() -> Result<reqwest::Client, ApiError> {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    if let Some(client) = CLIENT.get() {
        return Ok(client.clone());
    }
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(180))
        .build()
        .map_err(|_| ApiError::internal())?;
    let _ = CLIENT.set(client.clone());
    Ok(CLIENT.get().cloned().unwrap_or(client))
}
pub async fn llm_json(
    config: &ModelConfig,
    instructions: &str,
    input: Value,
    schema: Value,
) -> Result<String, ApiError> {
    if !config.enabled {
        return Err(ApiError::unavailable("回答模型未启用"));
    }
    let key = config
        .api_key
        .as_ref()
        .ok_or_else(|| ApiError::unavailable("请配置回答模型密钥"))?;
    let (url, body) = llm_request(config, instructions, input, schema);
    let response = client()?
        .post(url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            ApiError::unavailable(if error.is_timeout() {
                "回答模型处理超时，请重试"
            } else {
                "回答模型连接失败，请重试"
            })
        })?;
    if !response.status().is_success() {
        return Err(model_http_error(response.status().as_u16()));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| ApiError::unavailable("回答网关返回格式不正确"))?;
    llm_response_text(config, &body)
}

/// Generate ordinary answer text through the provider's SSE endpoint.
///
/// This path is used only when the administrator explicitly disables the
/// post-generation knowledge-base verification pipeline. Retrieval and the
/// evidence boundary still happen before this function is called; the model's
/// visible text is forwarded as soon as the provider emits it, while the full
/// answer is accumulated for persistence.
pub async fn llm_stream_text(
    config: &ModelConfig,
    instructions: &str,
    input: Value,
    deltas: &tokio::sync::mpsc::UnboundedSender<String>,
) -> Result<String, ApiError> {
    if !config.enabled {
        return Err(ApiError::unavailable("回答模型未启用"));
    }
    let key = config
        .api_key
        .as_ref()
        .ok_or_else(|| ApiError::unavailable("请配置回答模型密钥"))?;
    let (url, body) = llm_stream_request(config, instructions, input);
    let response = client()?
        .post(url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|error| {
            ApiError::unavailable(if error.is_timeout() {
                "回答模型处理超时，请重试"
            } else {
                "回答模型连接失败，请重试"
            })
        })?;
    if !response.status().is_success() {
        return Err(model_http_error(response.status().as_u16()));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = Vec::<u8>::new();
    let mut content = String::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ApiError::unavailable("回答模型流式连接中断，请重试"))?;
        buffer.extend_from_slice(&chunk);
        while let Some(event_bytes) = take_sse_event(&mut buffer) {
            collect_stream_event(config, &event_bytes, deltas, &mut content)?;
        }
    }
    if !buffer.is_empty() {
        collect_stream_event(config, &buffer, deltas, &mut content)?;
    }
    if content.is_empty() {
        return Err(ApiError::unavailable("回答模型未返回可读内容，请重试"));
    }
    Ok(content)
}

fn llm_stream_request(config: &ModelConfig, instructions: &str, input: Value) -> (String, Value) {
    let deepseek = config.provider == "deepseek";
    if config.protocol == "chat" {
        let mut messages = vec![json!({"role":"system","content":instructions})];
        messages.extend(input.as_array().cloned().unwrap_or_default());
        let mut body = json!({
            "model": config.model,
            "messages": messages,
            "stream": true,
        });
        if deepseek {
            body["max_tokens"] = json!(16000);
            body["thinking"] = json!({"type": config.thinking});
            if config.thinking == "enabled" {
                body["reasoning_effort"] = json!(config.reasoning_effort);
            }
        } else {
            body["max_completion_tokens"] = json!(5000);
        }
        (config.endpoint("chat/completions"), body)
    } else {
        (
            config.endpoint("responses"),
            json!({
                "model": config.model,
                "instructions": instructions,
                "input": input,
                "stream": true,
                "store": false,
                "max_output_tokens": 5000,
            }),
        )
    }
}

fn take_sse_event(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    let position = buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| (position, 4))
        .or_else(|| {
            buffer
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|position| (position, 2))
        })?;
    Some(buffer.drain(..position.0 + position.1).collect())
}

fn collect_stream_event(
    config: &ModelConfig,
    event_bytes: &[u8],
    deltas: &tokio::sync::mpsc::UnboundedSender<String>,
    content: &mut String,
) -> Result<(), ApiError> {
    let event = String::from_utf8_lossy(event_bytes);
    for line in event.lines() {
        let Some(data) = line.strip_prefix("data:").map(str::trim) else {
            continue;
        };
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let payload: Value = serde_json::from_str(data)
            .map_err(|_| ApiError::unavailable("回答模型流式返回格式不正确，请重试"))?;
        if let Some(message) = payload.pointer("/error/message").and_then(Value::as_str) {
            return Err(ApiError::unavailable(format!(
                "回答模型调用失败：{message}"
            )));
        }
        let delta = if config.protocol == "chat" {
            payload
                .pointer("/choices/0/delta/content")
                .and_then(Value::as_str)
        } else {
            payload
                .pointer("/delta")
                .and_then(Value::as_str)
                .or_else(|| {
                    payload
                        .pointer("/response/output_text/delta")
                        .and_then(Value::as_str)
                })
        };
        // DeepSeek emits reasoning_content in thinking mode. It is
        // deliberately ignored; only the answer content reaches the client.
        if let Some(delta) = delta.filter(|value| !value.is_empty()) {
            content.push_str(delta);
            let _ = deltas.send(delta.to_owned());
        }
    }
    Ok(())
}

fn llm_request(
    config: &ModelConfig,
    instructions: &str,
    input: Value,
    schema: Value,
) -> (String, Value) {
    let deepseek = config.provider == "deepseek";
    let (url, mut body) = if config.protocol == "chat" {
        // DeepSeek supports JSON object mode, not OpenAI's strict JSON schema.
        // Keep our local parsing, exact-quote checks and independent review.
        let instructions = if deepseek {
            format!("{instructions}\n仅输出合法 JSON 对象，必须符合此 JSON Schema：\n{schema}")
        } else {
            instructions.to_owned()
        };
        let mut messages = vec![json!({"role":"system","content":instructions})];
        messages.extend(input.as_array().cloned().unwrap_or_default());
        (
            config.endpoint("chat/completions"),
            json!({"model":config.model,"messages":messages,"stream":false}),
        )
    } else {
        (
            config.endpoint("responses"),
            json!({"model":config.model,"instructions":instructions,"input":input,"stream":false,"store":false,"max_output_tokens":5000}),
        )
    };
    if config.protocol == "chat" {
        if deepseek {
            body["max_tokens"] = json!(16000);
            body["thinking"] = json!({"type":config.thinking});
            if config.thinking == "enabled" {
                body["reasoning_effort"] = json!(config.reasoning_effort);
            }
            body["response_format"] = json!({"type":"json_object"});
        } else {
            body["max_completion_tokens"] = json!(5000);
            body["response_format"] = json!({"type":"json_schema","json_schema":{"name":"knowledge_result","strict":true,"schema":schema}});
        }
    } else {
        body["text"] = json!({"format":{"type":"json_schema","name":"knowledge_result","strict":true,"schema":schema}});
    }
    (url, body)
}

fn model_http_error(status: u16) -> ApiError {
    let message = match status {
        400 => "请求格式或模型参数不正确，请检查后台模型配置",
        401 | 403 => "模型密钥无效或没有访问权限，请管理员检查密钥",
        402 => "模型账户余额不足，请充值后重试",
        422 => "模型不支持当前参数或输入，请检查模型名称和调用协议",
        429 => "模型请求过于频繁或额度已用尽，请稍后重试",
        500 => "模型服务内部错误，请稍后重试",
        502..=504 => "模型服务暂时繁忙或不可用，请稍后重试",
        _ => "模型调用失败，请检查配置或稍后重试",
    };
    ApiError::unavailable(format!("{message}（HTTP {status}）"))
}

fn llm_response_text(config: &ModelConfig, body: &Value) -> Result<String, ApiError> {
    if config.protocol == "chat" {
        if body
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
            != Some("stop")
        {
            return Err(ApiError::unavailable("回答尚未完整生成，请重试"));
        }
    } else if body.get("status").and_then(Value::as_str) != Some("completed") {
        return Err(ApiError::unavailable("回答尚未完整生成，请重试"));
    }
    if let Some(text) = body
        .pointer("/choices/0/message/content")
        .and_then(Value::as_str)
    {
        return Ok(text.into());
    }
    if let Some(text) = body.get("output_text").and_then(Value::as_str) {
        return Ok(text.into());
    }
    if body
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| s != "completed")
    {
        return Err(ApiError::unavailable("回答尚未完整生成，请重试"));
    }
    let text = body
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("content").and_then(Value::as_array))
        .flatten()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("output_text"))
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("");
    if text.is_empty() {
        return Err(ApiError::unavailable("网关未返回可读回答"));
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "calls configured DeepSeek service using a minimal JSON connection probe"]
    async fn live_deepseek_connection() {
        let config = default_config("llm").unwrap();
        assert_eq!(config.provider, "deepseek");
        validate(&config).unwrap();
        let text = llm_json(&config,"仅输出 JSON 对象 {\"ok\":true}",json!([{"role":"user","content":"连接测试"}]),json!({"type":"object","properties":{"ok":{"type":"boolean"}},"required":["ok"],"additionalProperties":false})).await.unwrap();
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap()["ok"], true);
    }
    #[test]
    fn deepseek_json_and_vision_use_supported_protocol_and_ignore_reasoning_output() {
        let mut config = default_config("llm").unwrap();
        config.provider = "deepseek".into();
        config.base_url = "https://api.deepseek.com".into();
        config.protocol = "chat".into();
        config.thinking = "enabled".into();
        let schema =
            json!({"type":"object","required":["ok"],"properties":{"ok":{"type":"boolean"}}});
        let (url, body) = llm_request(
            &config,
            "只输出JSON",
            json!([{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,test"}}]}]),
            schema.clone(),
        );
        assert_eq!(url, "https://api.deepseek.com/chat/completions");
        assert_eq!(body["response_format"], json!({"type":"json_object"}));
        assert!(body["messages"][0]["content"]
            .as_str()
            .unwrap()
            .contains(&schema.to_string()));
        assert_eq!(body["messages"][1]["content"][0]["type"], "image_url");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
        assert!(body.get("max_completion_tokens").is_none());
        let response = json!({"choices":[{"finish_reason":"stop","message":{"content":"{\"ok\":true}","reasoning_content":"private reasoning"}}]});
        assert_eq!(
            llm_response_text(&config, &response).unwrap(),
            "{\"ok\":true}"
        );
        let truncated =
            json!({"choices":[{"finish_reason":"length","message":{"content":"{\"ok\":"}}]});
        assert!(llm_response_text(&config, &truncated).is_err());
    }

    #[test]
    fn legacy_model_config_defaults_to_strict_answer_verification() {
        let config: ModelConfig = serde_json::from_value(json!({
            "kind": "llm",
            "baseUrl": "https://api.deepseek.com",
            "model": "deepseek-flash"
        }))
        .unwrap();
        assert!(config.answer_verification_enabled);
        assert!(config.answer_provenance_enabled);
        assert_eq!(config.answer_provenance_mode, "inline");
        assert_eq!(
            serde_json::to_value(&config).unwrap()["answerVerificationEnabled"],
            true
        );

        let disabled: ModelConfig = serde_json::from_value(json!({
            "kind": "llm",
            "baseUrl": "https://api.deepseek.com",
            "model": "deepseek-flash",
            "answerVerificationEnabled": false
        }))
        .unwrap();
        assert!(!disabled.answer_verification_enabled);
        assert!(disabled.answer_provenance_enabled);
        assert_eq!(disabled.answer_provenance_mode, "inline");
    }

    #[test]
    fn streaming_request_and_parser_forward_answer_content_only() {
        let mut config = default_config("llm").unwrap();
        config.provider = "deepseek".into();
        config.base_url = "https://api.deepseek.com".into();
        config.protocol = "chat".into();
        config.thinking = "enabled".into();
        let (url, body) = llm_stream_request(
            &config,
            "只回答资料内内容",
            json!([{"role":"user","content":"问题"}]),
        );
        assert_eq!(url, "https://api.deepseek.com/chat/completions");
        assert_eq!(body["stream"], true);
        assert_eq!(body["thinking"]["type"], "enabled");

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut answer = String::new();
        collect_stream_event(
            &config,
            r#"data: {"choices":[{"delta":{"reasoning_content":"内部思考"}}]}

"#
            .as_bytes(),
            &tx,
            &mut answer,
        )
        .unwrap();
        collect_stream_event(
            &config,
            r#"data: {"choices":[{"delta":{"content":"答案"}}]}

"#
            .as_bytes(),
            &tx,
            &mut answer,
        )
        .unwrap();
        assert_eq!(answer, "答案");
        assert_eq!(rx.try_recv().unwrap(), "答案");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn encrypted_keys_are_randomized_authenticated_and_never_serialized() {
        let cipher = Aes256Gcm::new_from_slice(&[9; 32]).unwrap();
        let first = encrypt_using("unit-test-key", &cipher).unwrap();
        let second = encrypt_using("unit-test-key", &cipher).unwrap();
        assert_ne!(first, second);
        assert_eq!(decrypt_using(&first, &cipher).unwrap(), "unit-test-key");
        let mut tampered = first;
        tampered[20] ^= 1;
        assert!(decrypt_using(&tampered, &cipher).is_err());
        let mut config = default_config("llm").unwrap();
        config.api_key = Some("unit-test-key".into());
        let json = serde_json::to_string(&config).unwrap();
        assert!(!json.contains("unit-test-key"));
        assert!(!json.contains("apiKey"));
    }
    #[test]
    fn explicit_admin_editor_view_includes_effective_config() {
        let mut config = default_config("llm").unwrap();
        config.api_key = Some("unit-test-editor-key".into());
        let ordinary = serde_json::to_value(&config).unwrap();
        assert!(ordinary.get("apiKey").is_none());
        let view = admin_config_view(config).unwrap();
        assert_eq!(view["apiKey"], "unit-test-editor-key");
        assert!(view["baseUrl"].as_str().unwrap().starts_with("https://"));
    }
}
