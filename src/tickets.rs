//! Short-lived capabilities for published knowledge media and admin previews. Ticket URLs are
//! credentials: do not log their paths, persist them in chat history, or cache
//! their responses. A withdrawal invalidates every later range request.

#[cfg(test)]
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use axum::{body::Body, http::StatusCode, response::IntoResponse};
use axum::{
    extract::{Path, State},
    http::{header, HeaderMap, HeaderValue, Method},
    response::Response,
    routing::{get, post},
    Json, Router,
};
#[cfg(test)]
use bytes::Bytes;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;
#[cfg(test)]
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use uuid::Uuid;

use crate::{
    auth::{bearer_token, require_admin, require_user},
    catalog::resolve_scope,
    error::ApiError,
    state::AppState,
};

const TICKET_TTL_SECONDS: i64 = 60;
const MEDIA_PATH: &str = "/kb-api/v1/knowledge/media/";
const INVALID_TICKET: &str = "媒体取件票据无效或已过期";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/kb-api/v1/knowledge/media/{ticket}", get(read_media))
        .route(
            "/kb-api/v1/knowledge/assets/{asset_id}/access-ticket",
            post(access_ticket),
        )
        .route(
            "/kb-api/v1/admin/knowledge/assets/{asset_id}/preview-ticket",
            post(preview_ticket),
        )
        .route(
            "/kb-api/v1/admin/knowledge/parts/{part_id}/page-preview-ticket",
            post(page_preview_ticket),
        )
}

fn now_seconds() -> Result<i64, ApiError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|value| i64::try_from(value.as_secs()).ok())
        .ok_or_else(ApiError::internal)
}

fn valid_ticket(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_storage_key(value: &str) -> bool {
    let Some((id, extension)) = value.rsplit_once('.') else {
        return false;
    };
    id.len() == 36
        && Uuid::parse_str(id).is_ok_and(|uuid| uuid.hyphenated().to_string() == id)
        && !extension.is_empty()
        && extension.len() <= 10
        && extension
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
}

fn safe_mime(value: &str) -> &'static str {
    match value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
        .as_str()
    {
        "image/jpeg" => "image/jpeg",
        "image/png" => "image/png",
        "image/webp" => "image/webp",
        "image/gif" => "image/gif",
        "audio/mpeg" => "audio/mpeg",
        "audio/mp3" => "audio/mpeg",
        "audio/wav" | "audio/x-wav" => "audio/wav",
        "audio/ogg" => "audio/ogg",
        "audio/mp4" => "audio/mp4",
        "audio/flac" => "audio/flac",
        "video/mp4" => "video/mp4",
        "video/webm" => "video/webm",
        "video/ogg" => "video/ogg",
        "video/quicktime" => "video/quicktime",
        "video/x-msvideo" => "video/x-msvideo",
        "application/pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

fn preview_mime(value: &str) -> Option<&'static str> {
    let mime = safe_mime(value);
    if mime.starts_with("image/") || mime.starts_with("audio/") || mime.starts_with("video/") {
        Some(mime)
    } else {
        None
    }
}

/// The HTTP handler verifies the administrator before invoking this helper.
/// A preview binds only the asset's original file and latest revision; the
/// caller cannot select another revision, derived file, MIME or ticket purpose.
async fn issue_preview(state: &AppState, user_id: &str, asset: &str) -> Result<String, ApiError> {
    if user_id.is_empty() || user_id.len() > 256 {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    let row = sqlx::query(
        "SELECT a.storage_key,a.mime_type,r.revision_id FROM kb_assets a
         JOIN kb_asset_revisions r ON r.asset_id=a.asset_id
           AND r.revision_id=(SELECT revision_id FROM kb_asset_revisions WHERE asset_id=a.asset_id ORDER BY revision_number DESC LIMIT 1)
         JOIN kb_course_nodes n ON n.node_id=a.node_id
         JOIN kb_courses c ON c.course_id=n.course_id
         LEFT JOIN kb_course_nodes parent ON parent.node_id=n.parent_node_id
         WHERE a.asset_id=? AND a.deleted_at IS NULL
           AND n.is_active=1 AND n.level=2 AND c.is_active=1
           AND parent.is_active=1 AND parent.level=1 AND parent.course_id=n.course_id",
    )
    .bind(asset)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| ApiError::not_found("资料不存在或课程节点已失效"))?;
    let key: String = row.get("storage_key");
    let mime: Option<String> = row.get("mime_type");
    let mime = mime
        .as_deref()
        .and_then(preview_mime)
        .ok_or_else(|| ApiError::bad_request("仅支持图片、视频和音频原件预览"))?;
    if !valid_storage_key(&key) {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    let revision: String = row.get("revision_id");
    let ticket = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let now = now_seconds()?;
    // Repeat every authorization/key check within INSERT to avoid issuing a
    // stale capability if the asset changes after the SELECT above.
    let inserted = sqlx::query(
        "INSERT INTO kb_media_tickets(ticket,asset_id,revision_id,storage_key,mime_type,user_id,purpose,expires_at)
         SELECT ?,a.asset_id,r.revision_id,a.storage_key,a.mime_type,?,'admin_preview',?
         FROM kb_assets a
         JOIN kb_asset_revisions r ON r.asset_id=a.asset_id
           AND r.revision_id=(SELECT revision_id FROM kb_asset_revisions WHERE asset_id=a.asset_id ORDER BY revision_number DESC LIMIT 1)
         JOIN kb_course_nodes n ON n.node_id=a.node_id
         JOIN kb_courses c ON c.course_id=n.course_id
         LEFT JOIN kb_course_nodes parent ON parent.node_id=n.parent_node_id
         WHERE a.asset_id=? AND r.revision_id=? AND a.storage_key=? AND a.mime_type=? AND a.deleted_at IS NULL
           AND n.is_active=1 AND n.level=2 AND c.is_active=1
           AND parent.is_active=1 AND parent.level=1 AND parent.course_id=n.course_id",
    )
    .bind(&ticket)
    .bind(user_id)
    .bind(now + TICKET_TTL_SECONDS)
    .bind(asset)
    .bind(revision)
    .bind(key)
    .bind(mime)
    .execute(&state.db)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    sqlx::query("DELETE FROM kb_media_tickets WHERE expires_at<=?")
        .bind(now)
        .execute(&state.db)
        .await?;
    Ok(format!("{MEDIA_PATH}{ticket}"))
}

async fn preview_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = require_admin(&state, &headers).await?;
    let path = issue_preview(&state, &user.id, &asset_id).await?;
    Ok(Json(json!({"url":path,"expiresIn":TICKET_TTL_SECONDS})))
}

async fn issue_page_preview(
    state: &AppState,
    user_id: &str,
    part: &str,
) -> Result<String, ApiError> {
    if user_id.is_empty() || user_id.len() > 256 {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    let ticket = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let now = now_seconds()?;
    let inserted = sqlx::query(
        "INSERT INTO kb_media_tickets(ticket,asset_id,revision_id,storage_key,mime_type,user_id,purpose,expires_at)
         SELECT ?,a.asset_id,r.revision_id,p.media_storage_key,CASE WHEN p.media_storage_key LIKE '%.png' THEN 'image/png' ELSE 'image/jpeg' END,?,'admin_page_preview',?
         FROM kb_content_parts p JOIN kb_asset_revisions r USING(revision_id)
         JOIN kb_assets a USING(asset_id) JOIN kb_course_nodes n ON n.node_id=a.node_id
         JOIN kb_courses c ON c.course_id=n.course_id
         JOIN kb_course_nodes parent ON parent.node_id=n.parent_node_id AND parent.course_id=n.course_id
         WHERE p.part_id=? AND a.deleted_at IS NULL AND n.is_active=1 AND n.level=2
           AND c.is_active=1 AND parent.is_active=1 AND parent.level=1
           AND p.modality='image' AND (p.media_storage_key LIKE '%.jpg' OR p.media_storage_key LIKE '%.png')
           AND json_extract(p.metadata_json,'$.documentPage')=1
           AND r.revision_id=(SELECT revision_id FROM kb_asset_revisions WHERE asset_id=a.asset_id ORDER BY revision_number DESC LIMIT 1)"
    ).bind(&ticket).bind(user_id).bind(now + TICKET_TTL_SECONDS).bind(part).execute(&state.db).await?;
    if inserted.rows_affected() != 1 {
        return Err(ApiError::not_found("页面不存在或资料已生成新版，请刷新"));
    }
    sqlx::query("DELETE FROM kb_media_tickets WHERE expires_at<=?")
        .bind(now)
        .execute(&state.db)
        .await?;
    Ok(format!("{MEDIA_PATH}{ticket}"))
}

async fn page_preview_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(part_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let user = require_admin(&state, &headers).await?;
    let path = issue_page_preview(&state, &user.id, &part_id).await?;
    Ok(Json(json!({"url":path,"expiresIn":TICKET_TTL_SECONDS})))
}

/// Issues a 60-second student capability only for an actively published asset
/// and media key belonging to that exact revision (or its original file).
pub async fn issue(
    state: &AppState,
    user_id: &str,
    asset: &str,
    revision: &str,
    key: &str,
    mime: &str,
) -> Result<String, ApiError> {
    if user_id.is_empty() || user_id.len() > 256 || !valid_storage_key(key) {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    let now = now_seconds()?;
    let ticket = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    // INSERT SELECT makes the publication/key check part of the write itself.
    // GET repeats the check so withdrawal after issuance also takes effect.
    let inserted = sqlx::query(
        "INSERT INTO kb_media_tickets(ticket,asset_id,revision_id,storage_key,mime_type,user_id,purpose,expires_at)
         SELECT ?,a.asset_id,r.revision_id,?,?,?,'student',?
         FROM kb_assets a
         JOIN kb_asset_revisions r ON r.asset_id=a.asset_id AND r.revision_id=a.published_revision_id
         JOIN kb_course_nodes n ON n.node_id=a.node_id
         JOIN kb_courses c ON c.course_id=n.course_id
         LEFT JOIN kb_course_nodes parent ON parent.node_id=n.parent_node_id
         WHERE a.asset_id=? AND r.revision_id=? AND a.deleted_at IS NULL
           AND r.status='published' AND n.is_active=1 AND n.level=2 AND c.is_active=1
           AND parent.is_active=1 AND parent.level=1 AND parent.course_id=n.course_id
           AND (a.storage_key=? OR EXISTS(SELECT 1 FROM kb_content_parts p WHERE p.revision_id=r.revision_id AND p.media_storage_key=?))",
    )
    .bind(&ticket).bind(key).bind(safe_mime(mime)).bind(user_id).bind(now + TICKET_TTL_SECONDS)
    .bind(asset).bind(revision).bind(key).bind(key)
    .execute(&state.db).await?;
    if inserted.rows_affected() != 1 {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    sqlx::query("DELETE FROM kb_media_tickets WHERE expires_at<=?")
        .bind(now)
        .execute(&state.db)
        .await?;
    Ok(format!("{MEDIA_PATH}{ticket}"))
}

async fn access_ticket(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(asset_id): Path<String>,
    input: Option<Json<AccessTicketInput>>,
) -> Result<Json<Value>, ApiError> {
    let user = require_user(&state, &headers).await?;
    let token = bearer_token(&headers).ok_or_else(ApiError::unauthorized)?;
    let row = sqlx::query(
        "SELECT node_id,published_revision_id,storage_key,mime_type FROM kb_assets WHERE asset_id=? AND deleted_at IS NULL AND published_revision_id IS NOT NULL",
    ).bind(&asset_id).fetch_optional(&state.db).await?
        .ok_or_else(|| ApiError::not_found("资料不存在或未发布"))?;
    let node: String = row.get("node_id");
    let scope = resolve_scope(&state, token, "node", Some(&node)).await?;
    if scope.level != 2 || !scope.node_ids.contains(&node) {
        return Err(ApiError::forbidden("无权访问该知识资料"));
    }
    let revision: String = row.get("published_revision_id");
    let (key, mime) = if let Some(part_id) = input.and_then(|Json(input)| input.part_id) {
        if part_id.is_empty() || part_id.len() > 160 {
            return Err(ApiError::bad_request("无效媒体片段"));
        }
        let part = sqlx::query("SELECT media_storage_key,modality FROM kb_content_parts WHERE part_id=? AND revision_id=? AND media_storage_key IS NOT NULL")
            .bind(part_id).bind(&revision).fetch_optional(&state.db).await?
            .ok_or_else(|| ApiError::not_found("媒体片段不存在或已经撤回"))?;
        let key: String = part.get("media_storage_key");
        let modality: String = part.get("modality");
        let original: String = row.get("storage_key");
        let mime = if key == original {
            row.get::<Option<String>, _>("mime_type")
        } else {
            Some(
                match key.rsplit_once('.').map(|(_, ext)| ext) {
                    Some("jpg" | "jpeg") if modality == "image" => "image/jpeg",
                    Some("png") if modality == "image" => "image/png",
                    Some("webp") if modality == "image" => "image/webp",
                    Some("mp4") if modality == "video" => "video/mp4",
                    Some("mov") if modality == "video" => "video/quicktime",
                    Some("mp3") if modality == "audio" => "audio/mpeg",
                    Some("wav") if modality == "audio" => "audio/wav",
                    _ => "application/octet-stream",
                }
                .to_owned(),
            )
        };
        (key, mime)
    } else {
        (
            row.get::<String, _>("storage_key"),
            row.get::<Option<String>, _>("mime_type"),
        )
    };
    let path = issue(
        &state,
        &user.id,
        &asset_id,
        &revision,
        &key,
        mime.as_deref().unwrap_or(""),
    )
    .await?;
    Ok(Json(json!({"url":path,"expiresIn":TICKET_TTL_SECONDS})))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AccessTicketInput {
    part_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(test)]
struct ByteRange {
    start: u64,
    end: u64,
}

#[cfg(test)]
impl ByteRange {
    fn len(self) -> u64 {
        self.end - self.start + 1
    }
}

/// RFC byte ranges: a single inclusive interval, open end, or suffix. Multiple
/// ranges are rejected rather than combining content into an unbounded buffer.
#[cfg(test)]
fn parse_range(value: &str, size: u64) -> Result<ByteRange, ()> {
    if size == 0 {
        return Err(());
    }
    let value = value.strip_prefix("bytes=").ok_or(())?;
    if value.contains(',') {
        return Err(());
    }
    let (start, end) = value.split_once('-').ok_or(())?;
    fn number(value: &str) -> Result<u64, ()> {
        if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(());
        }
        value.parse().map_err(|_| ())
    }
    if start.is_empty() {
        let suffix = number(end)?;
        if suffix == 0 {
            return Err(());
        }
        return Ok(ByteRange {
            start: size.saturating_sub(suffix),
            end: size - 1,
        });
    }
    let start = number(start)?;
    if start >= size {
        return Err(());
    }
    let end = if end.is_empty() {
        size - 1
    } else {
        number(end)?.min(size - 1)
    };
    if end < start {
        return Err(());
    }
    Ok(ByteRange { start, end })
}

pub(crate) fn add_security_headers(response: &mut Response) {
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store, max-age=0"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; sandbox"),
    );
    headers.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
}

async fn read_media(
    State(state): State<AppState>,
    Path(ticket): Path<String>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (key, mime) = authorized_media(&state, &ticket).await?;
    state
        .storage
        .media_response(&key, &mime, method, headers)
        .await
}

/// Rechecked for every GET/HEAD/range. Admin preview and student publication
/// predicates remain separate: possessing one purpose never changes the other.
async fn authorized_media(state: &AppState, ticket: &str) -> Result<(String, String), ApiError> {
    if !valid_ticket(ticket) {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    let now = now_seconds()?;
    let row = sqlx::query(
        "SELECT t.storage_key,t.mime_type,t.purpose FROM kb_media_tickets t
         JOIN kb_assets a ON a.asset_id=t.asset_id
         JOIN kb_asset_revisions r ON r.revision_id=t.revision_id AND r.asset_id=a.asset_id
         JOIN kb_course_nodes n ON n.node_id=a.node_id
         JOIN kb_courses c ON c.course_id=n.course_id
         LEFT JOIN kb_course_nodes parent ON parent.node_id=n.parent_node_id
         WHERE t.ticket=? AND t.user_id IS NOT NULL AND t.expires_at>?
           AND a.deleted_at IS NULL
           AND n.is_active=1 AND n.level=2 AND c.is_active=1
           AND parent.is_active=1 AND parent.level=1 AND parent.course_id=n.course_id
           AND (
             (t.purpose='student' AND a.published_revision_id=t.revision_id AND r.status='published'
              AND (a.storage_key=t.storage_key OR EXISTS(SELECT 1 FROM kb_content_parts p WHERE p.revision_id=t.revision_id AND p.media_storage_key=t.storage_key)))
             OR
             (t.purpose='admin_preview' AND length(t.user_id)>0 AND a.storage_key=t.storage_key AND a.mime_type=t.mime_type
              AND r.revision_id=(SELECT revision_id FROM kb_asset_revisions WHERE asset_id=a.asset_id ORDER BY revision_number DESC LIMIT 1))
             OR
             (t.purpose='admin_page_preview' AND t.mime_type IN ('image/jpeg','image/png')
              AND r.revision_id=(SELECT revision_id FROM kb_asset_revisions WHERE asset_id=a.asset_id ORDER BY revision_number DESC LIMIT 1)
              AND EXISTS(SELECT 1 FROM kb_content_parts p WHERE p.revision_id=t.revision_id AND p.media_storage_key=t.storage_key
                AND p.modality='image' AND json_extract(p.metadata_json,'$.documentPage')=1))
           )",
    ).bind(ticket).bind(now).fetch_optional(&state.db).await?
        .ok_or_else(|| ApiError::not_found(INVALID_TICKET))?;
    let key: String = row.get("storage_key");
    if !valid_storage_key(&key) {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    let mime: String = row.get("mime_type");
    if row.get::<String, _>("purpose") == "admin_preview" && preview_mime(&mime).is_none() {
        return Err(ApiError::not_found(INVALID_TICKET));
    }
    Ok((key, mime))
}

#[cfg(test)]
pub(crate) async fn file_response(
    mut file: tokio::fs::File,
    mime: &str,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let size = file
        .metadata()
        .await
        .map_err(|_| ApiError::internal())?
        .len();
    let selected = if let Some(value) = headers.get(header::RANGE) {
        match value
            .to_str()
            .ok()
            .and_then(|value| parse_range(value, size).ok())
        {
            Some(range) => Some(range),
            None => {
                let mut response =
                    (StatusCode::RANGE_NOT_SATISFIABLE, Body::empty()).into_response();
                add_security_headers(&mut response);
                response.headers_mut().insert(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{size}"))
                        .map_err(|_| ApiError::internal())?,
                );
                response
                    .headers_mut()
                    .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
                return Ok(response);
            }
        }
    } else {
        None
    };
    let (start, length) = selected
        .map(|range| (range.start, range.len()))
        .unwrap_or((0, size));
    let body = if method == Method::HEAD {
        Body::empty()
    } else {
        if start != 0 {
            file.seek(io::SeekFrom::Start(start))
                .await
                .map_err(|_| ApiError::internal())?;
        }
        // Fixed 64 KiB chunks, capped to the selected byte range. In particular,
        // full-video responses never allocate an entire uploaded file.
        let mut reader = file.take(length);
        let stream = async_stream::try_stream! {
            let mut buffer = vec![0u8; 64 * 1024];
            loop {
                let count = reader.read(&mut buffer).await?;
                if count == 0 { break }
                yield Bytes::copy_from_slice(&buffer[..count]);
            }
        };
        let stream: std::pin::Pin<
            Box<dyn futures_core::Stream<Item = Result<Bytes, io::Error>> + Send>,
        > = Box::pin(stream);
        Body::from_stream(stream)
    };
    let mut response = (
        if selected.is_some() {
            StatusCode::PARTIAL_CONTENT
        } else {
            StatusCode::OK
        },
        body,
    )
        .into_response();
    add_security_headers(&mut response);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(safe_mime(mime)),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static(if safe_mime(mime) == "application/octet-stream" {
            "attachment"
        } else {
            "inline"
        }),
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).map_err(|_| ApiError::internal())?,
    );
    if let Some(range) = selected {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{size}", range.start, range.end))
                .map_err(|_| ApiError::internal())?,
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn streams_only_requested_bytes_and_head_has_no_body() {
        let path = std::env::temp_dir().join(format!("kb-ticket-test-{}.bin", Uuid::new_v4()));
        tokio::fs::write(&path, b"0123456789").await.unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=2-4"));
        let response = file_response(
            tokio::fs::File::open(&path).await.unwrap(),
            "video/mp4",
            Method::GET,
            headers,
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "private, no-store, max-age=0"
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 10)
            .await
            .unwrap();
        assert_eq!(&bytes[..], b"234");

        let mut headers = HeaderMap::new();
        headers.insert(header::RANGE, HeaderValue::from_static("bytes=-2"));
        let response = file_response(
            tokio::fs::File::open(&path).await.unwrap(),
            "video/mp4",
            Method::GET,
            headers,
        )
        .await
        .unwrap();
        assert_eq!(
            &axum::body::to_bytes(response.into_body(), 10)
                .await
                .unwrap()[..],
            b"89"
        );

        let response = file_response(
            tokio::fs::File::open(&path).await.unwrap(),
            "video/mp4",
            Method::HEAD,
            HeaderMap::new(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "10");
        assert!(axum::body::to_bytes(response.into_body(), 10)
            .await
            .unwrap()
            .is_empty());
        tokio::fs::remove_file(path).await.unwrap();
    }

    async fn fixture() -> (AppState, String) {
        use crate::{
            auth::AuthUpstream,
            config::{AppConfig, AuthConfig, QdrantConfig, StorageConfig},
            qdrant::QdrantAdapter,
        };
        let db = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&db).await.unwrap();
        let qdrant_config = QdrantConfig {
            http_url: reqwest::Url::parse("http://127.0.0.1:6333").unwrap(),
            grpc_url: "http://127.0.0.1:6334".into(),
            api_key: None,
        };
        let auth_config = AuthConfig {
            base_url: None,
            me_path: "/auth/me".into(),
        };
        let config = AppConfig {
            environment: crate::config::Environment::Development,
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            database_url: "sqlite::memory:".into(),
            qdrant: qdrant_config.clone(),
            auth: auth_config.clone(),
            storage: StorageConfig::Local {
                local_dir: std::path::PathBuf::from("data/uploads"),
            },
        };
        let state = AppState::new(
            config,
            db,
            QdrantAdapter::new(qdrant_config).unwrap(),
            AuthUpstream::new(auth_config).unwrap(),
        );
        let key = format!("{}.mp4", Uuid::new_v4());
        sqlx::query("INSERT INTO kb_courses(course_id,title,is_default,is_active) VALUES('course','Course',1,1)").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO kb_course_nodes(node_id,course_id,parent_node_id,level,title) VALUES('parent','course',NULL,1,'Parent'),('leaf','course','parent',2,'Leaf')").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,mime_type,created_by) VALUES('asset','leaf','video','Video',?,100,'video/mp4','user')")
            .bind(&key).execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by) VALUES('revision','asset',1,'hash','test','published','user')").execute(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_assets SET published_revision_id='revision' WHERE asset_id='asset'")
            .execute(&state.db)
            .await
            .unwrap();
        (state, key)
    }

    async fn get_error(state: &AppState, path: &str) -> ApiError {
        read_media(
            State(state.clone()),
            Path(path.trim_start_matches(MEDIA_PATH).to_owned()),
            Method::GET,
            HeaderMap::new(),
        )
        .await
        .unwrap_err()
    }

    #[tokio::test]
    async fn revoked_expired_or_inactive_media_cannot_be_read() {
        let (state, key) = fixture().await;
        let path = issue(&state, "user", "asset", "revision", &key, "video/mp4")
            .await
            .unwrap();
        assert!(path.starts_with(MEDIA_PATH));
        assert!(valid_ticket(path.trim_start_matches(MEDIA_PATH)));
        sqlx::query("UPDATE kb_media_tickets SET expires_at=0")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(get_error(&state, &path).await.status, StatusCode::NOT_FOUND);

        let path = issue(&state, "user", "asset", "revision", &key, "video/mp4")
            .await
            .unwrap();
        sqlx::query("UPDATE kb_assets SET published_revision_id=NULL")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(get_error(&state, &path).await.message, INVALID_TICKET);
        assert!(
            issue(&state, "user", "asset", "revision", &key, "video/mp4")
                .await
                .is_err()
        );

        sqlx::query("UPDATE kb_assets SET published_revision_id='revision'")
            .execute(&state.db)
            .await
            .unwrap();
        let path = issue(&state, "user", "asset", "revision", &key, "video/mp4")
            .await
            .unwrap();
        sqlx::query("UPDATE kb_course_nodes SET is_active=0 WHERE node_id='parent'")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(get_error(&state, &path).await.message, INVALID_TICKET);
    }

    #[tokio::test]
    async fn rejects_unrelated_file_and_revision() {
        let (state, key) = fixture().await;
        let unrelated = format!("{}.mp4", Uuid::new_v4());
        assert!(
            issue(&state, "user", "asset", "revision", &unrelated, "video/mp4")
                .await
                .is_err()
        );
        assert!(
            issue(&state, "user", "asset", "old-revision", &key, "video/mp4")
                .await
                .is_err()
        );
        assert!(issue(&state, "", "asset", "revision", &key, "video/mp4")
            .await
            .is_err());
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_media_tickets")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn page_preview_is_bound_to_latest_document_page_and_rechecks_withdrawal() {
        let (state, _) = fixture().await;
        sqlx::query("UPDATE kb_assets SET asset_type='document',mime_type='application/pdf',published_revision_id=NULL WHERE asset_id='asset'").execute(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_asset_revisions SET status='draft' WHERE revision_id='revision'")
            .execute(&state.db)
            .await
            .unwrap();
        let page_key = format!("{}.jpg", Uuid::new_v4());
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,media_storage_key,page_number,metadata_json,content_sha256) VALUES('page','revision',0,'image',?,1,'{\"documentPage\":true}','hash')").bind(&page_key).execute(&state.db).await.unwrap();
        let path = issue_page_preview(&state, "admin", "page").await.unwrap();
        let ticket = path.strip_prefix(MEDIA_PATH).unwrap();
        assert_eq!(
            authorized_media(&state, ticket).await.unwrap(),
            (page_key, "image/jpeg".into())
        );
        // Draft admin access never makes this page student-readable.
        let key: String = sqlx::query_scalar(
            "SELECT media_storage_key FROM kb_content_parts WHERE part_id='page'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert!(
            issue(&state, "student", "asset", "revision", &key, "image/jpeg")
                .await
                .is_err()
        );
        assert!(issue_page_preview(&state, "admin", "missing")
            .await
            .is_err());
        sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by) VALUES('new','asset',2,'hash','test','draft','admin')").execute(&state.db).await.unwrap();
        assert!(authorized_media(&state, ticket).await.is_err());
        assert!(issue_page_preview(&state, "admin", "page").await.is_err());
    }

    #[tokio::test]
    async fn admin_preview_of_draft_does_not_authorize_student_media() {
        let (state, key) = fixture().await;
        sqlx::query("UPDATE kb_assets SET published_revision_id=NULL")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("UPDATE kb_asset_revisions SET status='review'")
            .execute(&state.db)
            .await
            .unwrap();
        assert!(
            issue(&state, "student", "asset", "revision", &key, "video/mp4")
                .await
                .is_err()
        );
        // The handler, rather than a caller-supplied role or purpose, performs
        // authentication before the private preview issuer is reached.
        let error = preview_ticket(State(state.clone()), HeaderMap::new(), Path("asset".into()))
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        assert!(issue_preview(&state, "", "asset").await.is_err());

        let path = issue_preview(&state, "admin", "asset").await.unwrap();
        let ticket = path.trim_start_matches(MEDIA_PATH);
        assert_eq!(
            authorized_media(&state, ticket).await.unwrap(),
            (key, "video/mp4".into())
        );
        let row =
            sqlx::query("SELECT purpose,user_id,expires_at FROM kb_media_tickets WHERE ticket=?")
                .bind(ticket)
                .fetch_one(&state.db)
                .await
                .unwrap();
        assert_eq!(row.get::<String, _>("purpose"), "admin_preview");
        assert_eq!(row.get::<String, _>("user_id"), "admin");
        let ttl = row.get::<i64, _>("expires_at") - now_seconds().unwrap();
        assert!(ttl > 0 && ttl <= TICKET_TTL_SECONDS);

        sqlx::query("UPDATE kb_media_tickets SET purpose='student' WHERE ticket=?")
            .bind(ticket)
            .execute(&state.db)
            .await
            .unwrap();
        assert!(authorized_media(&state, ticket).await.is_err());
    }

    #[tokio::test]
    async fn authenticated_student_cannot_issue_admin_preview() {
        use crate::{auth::AuthUpstream, config::AuthConfig};

        // A local identity stub exercises require_admin without real account
        // credentials or any call to the user's course service.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let identity = Router::new().route(
            "/auth/me",
            get(|| async { Json(json!({"id":"student","role":"STUDENT"})) }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, identity).await.unwrap() });
        let (mut state, _) = fixture().await;
        state.auth = AuthUpstream::new(AuthConfig {
            base_url: Some(reqwest::Url::parse(&format!("http://{address}")).unwrap()),
            me_path: "/auth/me".into(),
        })
        .unwrap();
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer local-test-student"),
        );
        let error = preview_ticket(State(state.clone()), headers.clone(), Path("asset".into()))
            .await
            .unwrap_err();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        let error = page_preview_ticket(State(state.clone()), headers, Path("page".into()))
            .await
            .unwrap_err();
        server.abort();
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_media_tickets")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn admin_preview_rejects_unrelated_or_derived_keys_and_document_mime() {
        let (state, original) = fixture().await;
        let path = issue_preview(&state, "admin", "asset").await.unwrap();
        let ticket = path.trim_start_matches(MEDIA_PATH);
        let derived = format!("{}.mp4", Uuid::new_v4());
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,media_storage_key,content_sha256) VALUES('derived','revision',0,'video',?,'hash')")
            .bind(&derived).execute(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_media_tickets SET storage_key=? WHERE ticket=?")
            .bind(&derived)
            .bind(ticket)
            .execute(&state.db)
            .await
            .unwrap();
        // Even a valid derivative belonging to this revision is outside the
        // original-only admin preview contract.
        assert!(authorized_media(&state, ticket).await.is_err());
        sqlx::query("UPDATE kb_media_tickets SET storage_key=?,user_id=NULL WHERE ticket=?")
            .bind(&original)
            .bind(ticket)
            .execute(&state.db)
            .await
            .unwrap();
        assert!(authorized_media(&state, ticket).await.is_err());
        sqlx::query("UPDATE kb_media_tickets SET user_id='admin',mime_type='application/pdf' WHERE ticket=?")
            .bind(ticket).execute(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_assets SET mime_type='application/pdf'")
            .execute(&state.db)
            .await
            .unwrap();
        assert!(authorized_media(&state, ticket).await.is_err());
        assert!(issue_preview(&state, "admin", "asset").await.is_err());
    }

    #[tokio::test]
    async fn admin_preview_expires_after_new_revision_deletion_or_inactive_node() {
        let (state, key) = fixture().await;
        let old_preview = issue_preview(&state, "admin", "asset").await.unwrap();
        sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by) VALUES('new-revision','asset',2,'hash','test','review','admin')")
            .execute(&state.db).await.unwrap();
        assert_eq!(
            get_error(&state, &old_preview).await.message,
            INVALID_TICKET
        );
        // Creating a draft invalidates old admin previews but keeps the
        // unchanged published student's revision available.
        assert!(
            issue(&state, "student", "asset", "revision", &key, "video/mp4")
                .await
                .is_ok()
        );
        let preview = issue_preview(&state, "admin", "asset").await.unwrap();
        let ticket = preview.trim_start_matches(MEDIA_PATH);
        assert!(authorized_media(&state, ticket).await.is_ok());
        sqlx::query("UPDATE kb_assets SET deleted_at=CURRENT_TIMESTAMP")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(get_error(&state, &preview).await.message, INVALID_TICKET);
        assert!(issue_preview(&state, "admin", "asset").await.is_err());

        sqlx::query("UPDATE kb_assets SET deleted_at=NULL")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("UPDATE kb_course_nodes SET is_active=0 WHERE node_id='parent'")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(get_error(&state, &preview).await.message, INVALID_TICKET);
        assert!(issue_preview(&state, "admin", "asset").await.is_err());
    }

    #[test]
    fn supports_closed_open_and_suffix_ranges() {
        assert_eq!(
            parse_range("bytes=0-0", 100),
            Ok(ByteRange { start: 0, end: 0 })
        );
        assert_eq!(
            parse_range("bytes=10-", 100),
            Ok(ByteRange { start: 10, end: 99 })
        );
        assert_eq!(
            parse_range("bytes=90-500", 100),
            Ok(ByteRange { start: 90, end: 99 })
        );
        assert_eq!(
            parse_range("bytes=-10", 100),
            Ok(ByteRange { start: 90, end: 99 })
        );
        assert_eq!(
            parse_range("bytes=-500", 100),
            Ok(ByteRange { start: 0, end: 99 })
        );
    }

    #[test]
    fn rejects_unsatisfiable_and_multi_ranges() {
        for value in [
            "bytes=100-",
            "bytes=20-10",
            "bytes=-0",
            "bytes=0-1,3-4",
            "bytes=+1-2",
            "items=0-1",
            "bytes=-",
            "bytes=0-18446744073709551616",
        ] {
            assert!(parse_range(value, 100).is_err(), "{value}");
        }
        assert!(parse_range("bytes=0-", 0).is_err());
    }

    #[test]
    fn rejects_paths_and_active_document_types() {
        let id = Uuid::new_v4().to_string();
        assert!(valid_storage_key(&format!("{id}.mp4")));
        for key in [
            format!("../{id}.mp4"),
            format!("folder/{id}.png"),
            format!("{id}.../txt"),
            format!("{id}."),
            format!("{id}.MP4"),
        ] {
            assert!(!valid_storage_key(&key));
        }
        assert_eq!(safe_mime("image/svg+xml"), "application/octet-stream");
        assert_eq!(safe_mime("text/html"), "application/octet-stream");
        assert_eq!(safe_mime("video/mp4; charset=utf-8"), "video/mp4");
        assert!(!valid_ticket("../../etc/passwd"));
        assert!(valid_ticket(&format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        )));
    }
}
