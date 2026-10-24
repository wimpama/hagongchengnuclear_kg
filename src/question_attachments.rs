//! Private question material. These files never enter the published course index.
use axum::{
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::HeaderMap,
    response::Response,
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use crate::{auth::require_user, error::ApiError, ingest, knowledge::owned_scope, state::AppState};

const MAX_BYTES: usize = 20 * 1024 * 1024;
const MAX_TEXT_BYTES: usize = 8_000;
const TRUNCATED_NOTICE: &str = "【长文档：仅提供前约8KB正文，后续内容未提供，请勿推测】\n";

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/kb-api/v1/knowledge/conversations/{id}/attachments",
            post(upload),
        )
        .route(
            "/kb-api/v1/knowledge/conversations/{id}/attachments/{attachment}",
            get(preview),
        )
        .route(
            "/kb-api/v1/knowledge/conversations/{id}/attachments/{attachment}/image",
            get(image),
        )
        .layer(DefaultBodyLimit::max(MAX_BYTES + 64 * 1024))
}

pub fn bounded_text(text: &str, max_bytes: usize) -> String {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

struct PendingFile(Option<String>, crate::storage::BlobStore);
impl Drop for PendingFile {
    fn drop(&mut self) {
        if let Some(key) = self.0.take() {
            let storage = self.1.clone();
            tokio::spawn(async move {
                if storage.delete(&key).await.is_err() {
                    tracing::warn!(storage_key = %key, "failed to clean up uncommitted question attachment");
                }
            });
        }
    }
}

async fn upload(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    mut form: Multipart,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers).await?;
    owned_scope(&state, &headers, &id, &user.id).await?;
    let bytes_used: i64 = sqlx::query_scalar("SELECT COALESCE(SUM(byte_size),0) FROM kb_question_attachments WHERE user_id=? AND expires_at>CURRENT_TIMESTAMP")
        .bind(&user.id).fetch_one(&state.db).await?;
    if bytes_used > 120 * 1024 * 1024 {
        return Err(ApiError::bad_request("提问附件空间已满，请等待过期清理"));
    }
    let mut file = None;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|_| ApiError::bad_request("附件表单无效"))?
    {
        if field.name() != Some("file") || file.is_some() {
            return Err(ApiError::bad_request("每次上传一个 file 附件"));
        }
        let name = ingest::safe_display_name(field.file_name().unwrap_or(""))?;
        let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        if !matches!(
            ext.as_str(),
            "png" | "jpg" | "jpeg" | "webp" | "pdf" | "docx" | "pptx" | "txt" | "md" | "csv"
        ) {
            return Err(ApiError::bad_request(
                "提问支持 PNG/JPEG/WebP 图片及 PDF、DOCX、PPTX、TXT、MD、CSV 文档",
            ));
        }
        let (_, mime) =
            ingest::file_type(&ext).ok_or_else(|| ApiError::bad_request("不支持的附件格式"))?;
        let supplied = field
            .content_type()
            .unwrap_or("application/octet-stream")
            .to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|_| ApiError::bad_request("附件读取失败或超过20MB"))?;
        let limit = if mime.starts_with("image/") {
            6 * 1024 * 1024
        } else {
            MAX_BYTES
        };
        if bytes.is_empty()
            || bytes.len() > limit
            || !ingest::mime_matches(&ext, &supplied, mime)
            || !ingest::magic_matches(&ext, &bytes)
        {
            return Err(ApiError::bad_request(
                "附件内容与格式不符，或超出大小限制（图片6MB，文档20MB）",
            ));
        }
        file = Some((name, ext, mime.to_owned(), bytes));
    }
    let (name, ext, mime, bytes) = file.ok_or_else(|| ApiError::bad_request("请选择附件"))?;
    let attachment = Uuid::new_v4().to_string();
    let key = format!("question-{attachment}.{ext}");
    let mut pending = PendingFile(Some(key.clone()), state.storage.clone());
    state.storage.put_bytes(&key, &bytes, &mime).await?;
    let text = if mime.starts_with("image/") {
        String::new()
    } else if matches!(ext.as_str(), "txt" | "md" | "csv") {
        String::from_utf8(bytes.to_vec())
            .map_err(|_| ApiError::bad_request("文本附件请使用UTF-8编码"))?
    } else {
        let document =
            crate::extract::extract_document(state.storage.local_dir(), &key, &mime).await?;
        if document.pages.is_empty() {
            return Err(ApiError::bad_request(
                "未能提取附件文字；扫描文档请以图片上传，或补充可复制的题目正文",
            ));
        }
        document
            .pages
            .into_iter()
            .map(|p| {
                format!(
                    "{}{}",
                    p.page.map(|n| format!("第{n}页：\n")).unwrap_or_default(),
                    p.text
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    if !mime.starts_with("image/") && text.trim().is_empty() {
        return Err(ApiError::bad_request("附件中没有可读取的题目文字"));
    }
    let truncated = text.len() > MAX_TEXT_BYTES;
    let text = if truncated {
        format!(
            "{TRUNCATED_NOTICE}{}",
            bounded_text(&text, MAX_TEXT_BYTES - TRUNCATED_NOTICE.len())
        )
    } else {
        text
    };
    // Recheck ownership and permissions after potentially slow document parsing.
    owned_scope(&state, &headers, &id, &user.id).await?;
    let inserted = sqlx::query("INSERT INTO kb_question_attachments(attachment_id,conversation_id,user_id,original_name,mime_type,storage_key,byte_size,extracted_text,expires_at) SELECT ?,?,?,?,?,?,?,?,datetime('now','+30 days') WHERE EXISTS(SELECT 1 FROM kb_conversations WHERE conversation_id=? AND user_id=? AND deleted_at IS NULL)")
        .bind(&attachment).bind(&id).bind(&user.id).bind(&name).bind(&mime).bind(&key).bind(bytes.len() as i64).bind(&text).bind(&id).bind(&user.id).execute(&state.db).await?;
    if inserted.rows_affected() != 1 {
        return Err(ApiError::not_found("会话已删除"));
    }
    pending.0 = None;
    let mut view = views(&state, &id, &user.id, &[attachment]).await?.remove(0);
    view["textTruncated"] = json!(truncated);
    Ok(Json(view))
}

pub async fn views(
    state: &AppState,
    conversation: &str,
    user: &str,
    ids: &[String],
) -> Result<Vec<Value>, ApiError> {
    let mut result = Vec::new();
    for id in ids.iter().take(3) {
        let row = sqlx::query("SELECT original_name,mime_type,byte_size,expires_at,extracted_text,expires_at>CURRENT_TIMESTAMP AS available FROM kb_question_attachments WHERE attachment_id=? AND conversation_id=? AND user_id=?")
            .bind(id).bind(conversation).bind(user).fetch_optional(&state.db).await?;
        result.push(if let Some(row) = row {
            let available = row.get::<i64,_>("available") != 0;
            json!({"id":id,"name":row.get::<String,_>("original_name"),"mimeType":row.get::<String,_>("mime_type"),"sizeBytes":row.get::<i64,_>("byte_size"),"expiresAt":row.get::<String,_>("expires_at"),"available":available,"textPreview":if available { row.get::<String,_>("extracted_text") } else { String::new() }})
        } else { json!({"id":id,"name":"已过期附件","available":false}) });
    }
    Ok(result)
}

pub async fn validate_ids(
    state: &AppState,
    conversation: &str,
    user: &str,
    ids: &[String],
) -> Result<(), ApiError> {
    if ids.len() > 3 || ids.iter().collect::<std::collections::HashSet<_>>().len() != ids.len() {
        return Err(ApiError::bad_request("每条问题最多附3个不同附件"));
    }
    for view in views(state, conversation, user, ids).await? {
        if view["available"] != true {
            return Err(ApiError::bad_request(
                "提问附件已过期或不属于当前会话，请重新上传",
            ));
        }
    }
    Ok(())
}

pub struct QuestionMaterial {
    pub data: Vec<Value>,
    pub images: Vec<(String, String)>,
    pub retrieval_inputs: Vec<Value>,
    pub retrieval_text: String,
}

pub async fn material(
    state: &AppState,
    conversation: &str,
    ids: &[String],
    vision: bool,
) -> Result<QuestionMaterial, ApiError> {
    let mut result = QuestionMaterial {
        data: vec![],
        images: vec![],
        retrieval_inputs: vec![],
        retrieval_text: String::new(),
    };
    for id in ids.iter().take(3) {
        let row = sqlx::query("SELECT original_name,mime_type,storage_key,extracted_text FROM kb_question_attachments WHERE attachment_id=? AND conversation_id=? AND expires_at>CURRENT_TIMESTAMP")
            .bind(id).bind(conversation).fetch_optional(&state.db).await?.ok_or_else(|| ApiError::bad_request("提问附件已过期，请重新上传"))?;
        let mime: String = row.get("mime_type");
        let text: String = row.get("extracted_text");
        result.data.push(json!({"id":id,"name":row.get::<String,_>("original_name"),"text":bounded_text(&text,MAX_TEXT_BYTES),"textTruncated":text.starts_with(TRUNCATED_NOTICE)||text.len()>MAX_TEXT_BYTES,"trust":"student_question_only_not_course_evidence"}));
        if mime.starts_with("image/") {
            if !vision {
                return Err(ApiError::bad_request(
                    "当前回答模型未启用视觉理解，请管理员启用后再上传题图",
                ));
            }
            let storage_key: String = row.get("storage_key");
            state.storage.ensure_local(&storage_key).await?;
            let input =
                crate::embedding::media_input(state.storage.local_dir(), &storage_key, "image", 1.)
                    .await?;
            let url = input
                .pointer("/image_url/url")
                .and_then(Value::as_str)
                .ok_or_else(ApiError::internal)?
                .to_owned();
            result
                .images
                .push((format!("question_attachment:{id}"), url));
            // Keep image inputs separate from the resolved text query so the
            // visual similarity signal survives generic question wording.
            result.retrieval_inputs.push(input);
        } else {
            result.retrieval_text.push_str(&bounded_text(&text, 1500));
            result.retrieval_text.push('\n');
        }
    }
    Ok(result)
}

async fn preview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, attachment)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers).await?;
    owned_scope(&state, &headers, &id, &user.id).await?;
    validate_ids(&state, &id, &user.id, std::slice::from_ref(&attachment)).await?;
    Ok(Json(
        views(&state, &id, &user.id, &[attachment]).await?.remove(0),
    ))
}

async fn image(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((id, attachment)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    let user = require_user(&state, &headers).await?;
    owned_scope(&state, &headers, &id, &user.id).await?;
    let row = sqlx::query("SELECT storage_key,mime_type FROM kb_question_attachments WHERE attachment_id=? AND conversation_id=? AND user_id=? AND expires_at>CURRENT_TIMESTAMP")
        .bind(attachment).bind(id).bind(user.id).fetch_optional(&state.db).await?.ok_or_else(||ApiError::not_found("附件已过期"))?;
    let mime: String = row.get("mime_type");
    if !mime.starts_with("image/") {
        return Err(ApiError::bad_request("仅支持题图预览"));
    }
    let key: String = row.get("storage_key");
    state
        .storage
        .media_response(&key, &mime, axum::http::Method::GET, headers)
        .await
}

pub fn start_cleanup(state: AppState) {
    tokio::spawn(async move {
        loop {
            if cleanup_expired(&state).await.is_err() {
                tracing::warn!("question attachment cleanup will retry");
            }
            let _ = crate::storage::drain_cleanup(&state).await;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
}

async fn cleanup_expired(state: &AppState) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    sqlx::query("INSERT OR IGNORE INTO kb_blob_cleanup(storage_key) SELECT storage_key FROM kb_question_attachments WHERE expires_at<=CURRENT_TIMESTAMP")
        .execute(&mut *tx).await?;
    // Access is already denied by expires_at. Removing extracted text here also
    // prevents it from being retained after the original has expired.
    sqlx::query("DELETE FROM kb_question_attachments WHERE expires_at<=CURRENT_TIMESTAMP")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn byte_budget_preserves_unicode() {
        assert_eq!(bounded_text("核反应截面", 7), "核反");
        assert_eq!(bounded_text("abc", 10), "abc");
    }
    #[tokio::test]
    async fn expiration_queues_delete_before_removing_extracted_private_content() {
        let state = crate::db::test_state().await;
        sqlx::query("INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id) VALUES('expire','student','course','course','course')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO kb_question_attachments(attachment_id,conversation_id,user_id,original_name,mime_type,storage_key,byte_size,extracted_text,expires_at) VALUES('old','expire','student','a.txt','text/plain','question-expired.txt',1,'private extracted text',datetime('now','-1 second')),('live','expire','student','b.txt','text/plain','question-still-live.txt',1,'current text',datetime('now','+30 days'))").execute(&state.db).await.unwrap();
        cleanup_expired(&state).await.unwrap();
        let queued: Vec<String> = sqlx::query_scalar("SELECT storage_key FROM kb_blob_cleanup")
            .fetch_all(&state.db)
            .await
            .unwrap();
        assert_eq!(queued, vec!["question-expired.txt"]);
        let text: Vec<String> =
            sqlx::query_scalar("SELECT extracted_text FROM kb_question_attachments")
                .fetch_all(&state.db)
                .await
                .unwrap();
        assert_eq!(text, vec!["current text"]);
        // Repeated sweeps are idempotent and do not discard a failed delete.
        cleanup_expired(&state).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_blob_cleanup")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(count, 1);
    }
    #[tokio::test]
    async fn attachments_are_private_to_owner_and_conversation_and_expire() {
        let state = crate::db::test_state().await;
        for (id, user) in [("one", "student"), ("two", "other")] {
            sqlx::query("INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id) VALUES(?,?,'course','course','course')").bind(id).bind(user).execute(&state.db).await.unwrap();
        }
        sqlx::query("INSERT INTO kb_question_attachments(attachment_id,conversation_id,user_id,original_name,mime_type,storage_key,byte_size,extracted_text) VALUES('a','one','student','题目.txt','text/plain','question-test.txt',6,'私有题目')").execute(&state.db).await.unwrap();
        let ids = vec!["a".into()];
        assert!(validate_ids(&state, "one", "student", &ids).await.is_ok());
        assert!(validate_ids(&state, "one", "other", &ids).await.is_err());
        assert!(validate_ids(&state, "two", "student", &ids).await.is_err());
        assert!(
            validate_ids(&state, "one", "student", &["a".into(), "a".into()])
                .await
                .is_err()
        );
        assert!(material(&state, "two", &ids, true).await.is_err());
        sqlx::query("UPDATE kb_question_attachments SET expires_at=datetime('now','-1 second')")
            .execute(&state.db)
            .await
            .unwrap();
        assert!(validate_ids(&state, "one", "student", &ids).await.is_err());
        let history = views(&state, "one", "student", &ids).await.unwrap();
        assert_eq!(history[0]["available"], false);
        assert_eq!(history[0]["textPreview"], "");
    }
}
