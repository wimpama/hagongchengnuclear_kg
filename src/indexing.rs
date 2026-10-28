//! Durable, single-worker indexing with immutable reviewed evidence and an
//! explicit SQLite publication barrier. Qdrant is never an authorization DB.
use std::{collections::HashSet, sync::atomic::Ordering, time::Duration};

use axum::{
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{patch, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

use crate::{
    auth::require_admin,
    embedding,
    error::ApiError,
    ingest::{self, AssetView},
    media,
    models::{self, ModelConfig},
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/kb-api/v1/admin/knowledge/assets/{asset_id}/review",
            post(review_asset),
        )
        .route(
            "/kb-api/v1/admin/knowledge/assets/{asset_id}/reindex",
            post(reindex_asset),
        )
        .route(
            "/kb-api/v1/admin/knowledge/assets/{asset_id}/status",
            patch(update_status),
        )
}

async fn latest_revision(db: &SqlitePool, asset: &str) -> Result<String, ApiError> {
    ingest::load_asset(db, asset)
        .await?
        .revision_id
        .ok_or_else(|| ApiError::bad_request("资料没有版本"))
}

struct Revision {
    asset: String,
    node: String,
    course: String,
    status: String,
    review: String,
    index: String,
    collection: Option<String>,
    published: Option<String>,
}

async fn revision(db: &SqlitePool, id: &str) -> Result<Revision, ApiError> {
    let row=sqlx::query("SELECT r.asset_id,r.status,r.review_status,r.index_status,r.collection_id,a.node_id,a.published_revision_id,n.course_id FROM kb_asset_revisions r JOIN kb_assets a USING(asset_id) JOIN kb_course_nodes n ON n.node_id=a.node_id WHERE r.revision_id=? AND a.deleted_at IS NULL AND n.is_active=1 AND n.level=2")
        .bind(id).fetch_optional(db).await?.ok_or_else(||ApiError::not_found("资料版本或课程节点已失效"))?;
    Ok(Revision {
        asset: row.get("asset_id"),
        node: row.get("node_id"),
        course: row.get("course_id"),
        status: row.get("status"),
        review: row.get("review_status"),
        index: row.get("index_status"),
        collection: row.get("collection_id"),
        published: row.get("published_revision_id"),
    })
}

/// Review only the current draft. Video derivatives are made before approval;
/// an approved or published revision is never changed in place.
async fn review_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(asset): Path<String>,
) -> Result<Json<AssetView>, ApiError> {
    let user = require_admin(&state, &headers).await?;
    let _guard = state.review_lock.try_lock().map_err(|_| {
        ApiError::new(
            StatusCode::CONFLICT,
            "review_busy",
            "另一个资料正在审核处理，请稍后重试；本次审核尚未开始",
        )
    })?;
    let rev = latest_revision(&state.db, &asset).await?;
    let current = revision(&state.db, &rev).await?;
    if current.review == "approved" {
        return Ok(Json(ingest::load_asset(&state.db, &asset).await?));
    }
    if current.status == "published"
        || current.status == "withdrawn"
        || current.published.as_deref() == Some(&rev)
    {
        return Err(ApiError::bad_request(
            "已发布或撤回版本不可修改，请先创建新修订",
        ));
    }
    let progress = ReviewProgress {
        db: &state.db,
        revision: &rev,
        started_at: chrono::Utc::now().to_rfc3339(),
    };
    let started = std::time::Instant::now();
    progress
        .set("running", "checking", "正在检查资料片段")
        .await?;
    let result = tokio::time::timeout(
        Duration::from_secs(540),
        review_revision(&state, &asset, &rev, &user.id, &progress),
    )
    .await
    .unwrap_or_else(|_| {
        Err(ApiError::unavailable(
            "视频审核处理超时，请重试；可先将较长的视频拆分后上传",
        ))
    });
    let (status, message) = match &result {
        Ok(()) => ("succeeded", "资料审核已通过".to_owned()),
        Err(error) => ("failed", error.message.clone()),
    };
    if progress.set(status, status, message).await.is_err() {
        tracing::warn!(asset_id=%asset, revision_id=%rev, "could not update review progress");
    }
    tracing::info!(asset_id=%asset, revision_id=%rev, elapsed_ms=started.elapsed().as_millis() as u64, status, "knowledge review finished");
    result?;
    Ok(Json(ingest::load_asset(&state.db, &asset).await?))
}

struct ReviewProgress<'a> {
    db: &'a SqlitePool,
    revision: &'a str,
    started_at: String,
}

impl ReviewProgress<'_> {
    async fn set(
        &self,
        state: &str,
        stage: &str,
        message: impl Into<String>,
    ) -> Result<(), ApiError> {
        let progress = json!({"state":state,"stage":stage,"message":message.into(),"startedAt":self.started_at});
        sqlx::query("UPDATE kb_asset_revisions SET extraction_json=json_set(COALESCE(extraction_json,'{}'),'$.reviewProgress',json(?)) WHERE revision_id=?")
            .bind(progress.to_string()).bind(self.revision).execute(self.db).await?;
        Ok(())
    }
}

async fn review_revision(
    state: &AppState,
    asset: &str,
    rev: &str,
    actor: &str,
    progress: &ReviewProgress<'_>,
) -> Result<(), ApiError> {
    let rows =
        sqlx::query("SELECT * FROM kb_content_parts WHERE revision_id=? ORDER BY part_index")
            .bind(rev)
            .fetch_all(&state.db)
            .await?;
    if rows.is_empty() {
        return Err(ApiError::bad_request(
            "没有可审核内容，请先补充教师确认的正文或字幕",
        ));
    }
    let mut derived = Vec::new();
    let mut files = DerivativeFiles {
        storage: Some(state.storage.clone()),
        keys: Vec::new(),
        retained: false,
    };
    for row in &rows {
        let modality: String = row.get("modality");
        let metadata = parse_metadata(row.get("metadata_json"));
        if modality != "video"
            || metadata.get("videoPrepared").and_then(Value::as_bool) == Some(true)
        {
            continue;
        }
        let original: String = row
            .get::<Option<String>, _>("media_storage_key")
            .ok_or_else(|| ApiError::bad_request("视频片段缺少原件"))?;
        progress
            .set("running", "loading_original", "正在读取视频原件")
            .await?;
        state.storage.ensure_local(&original).await?;
        // The upload/import already persisted the original in OSS. Reading it
        // for FFmpeg must not upload the unchanged original a second time.
        let range = row
            .get::<Option<i64>, _>("start_ms")
            .zip(row.get::<Option<i64>, _>("end_ms"));
        progress
            .set(
                "running",
                "preparing_video",
                "正在裁剪视频分段并抽取预览画面",
            )
            .await?;
        let media_started = std::time::Instant::now();
        let segments =
            media::prepare_video_range(state.storage.local_dir(), &original, range).await?;
        tracing::info!(asset_id=%asset, revision_id=%rev, segment_count=segments.len(), elapsed_ms=media_started.elapsed().as_millis() as u64, "review video preparation finished");
        for segment in &segments {
            if segment.storage_key != original {
                files.keys.push(segment.storage_key.clone());
            }
            if let Some(frame) = &segment.frame_storage_key {
                files.keys.push(frame.clone());
            }
        }
        derived.push((row.get::<String, _>("part_id"), original, segments));
    }
    let upload_started = std::time::Instant::now();
    for (index, key) in files.keys.iter().enumerate() {
        progress
            .set(
                "running",
                "uploading_derivatives",
                format!(
                    "正在上传视频分段和预览图到 OSS（{}/{}）",
                    index + 1,
                    files.keys.len()
                ),
            )
            .await?;
        state
            .storage
            .put_file(
                key,
                &state.storage.local_dir().join(key),
                content_type_for_key(key),
            )
            .await?;
    }
    if !files.keys.is_empty() {
        tracing::info!(asset_id=%asset, revision_id=%rev, file_count=files.keys.len(), elapsed_ms=upload_started.elapsed().as_millis() as u64, "review OSS derivative uploads finished");
    }
    progress
        .set("running", "saving_review", "正在保存审核结果")
        .await?;
    let mut tx = state.db.begin().await?;
    ensure_latest_tx(&mut tx, asset, rev).await?;
    let status: String =
        sqlx::query_scalar("SELECT review_status FROM kb_asset_revisions WHERE revision_id=?")
            .bind(rev)
            .fetch_one(&mut *tx)
            .await?;
    if status == "approved" {
        return Err(ApiError::bad_request("该版本已完成审核，请刷新"));
    }
    let vectors: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM kb_vector_records WHERE revision_id=?")
            .bind(rev)
            .fetch_one(&mut *tx)
            .await?;
    if vectors > 0 {
        return Err(ApiError::bad_request(
            "该版本已有索引记录，请创建新修订后审核",
        ));
    }
    let mut next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(part_index),-1)+1 FROM kb_content_parts WHERE revision_id=?",
    )
    .bind(rev)
    .fetch_one(&mut *tx)
    .await?;
    for (source_id, source_key, segments) in derived {
        // Keep teacher descriptions as a text part, rather than incorrectly
        // applying a full-video transcript to every 60-second segment.
        let original = rows
            .iter()
            .find(|r| r.get::<String, _>("part_id") == source_id)
            .ok_or_else(ApiError::internal)?;
        if original
            .get::<Option<String>, _>("content_text")
            .as_deref()
            .is_some_and(|t| !t.trim().is_empty())
        {
            let metadata = json!({"source":"teacher_text","sourceStorageKey":source_key});
            let text: Option<String> = original.get("content_text");
            let hash = ingest::hash_content(
                text.as_deref(),
                Some(&source_key),
                original.get("start_ms"),
                original.get("end_ms"),
                &metadata,
            );
            sqlx::query("UPDATE kb_content_parts SET modality='text',media_storage_key=?,metadata_json=?,content_sha256=? WHERE part_id=?")
                .bind(&source_key).bind(metadata.to_string()).bind(hash).bind(&source_id).execute(&mut *tx).await?;
        } else {
            sqlx::query("DELETE FROM kb_content_parts WHERE part_id=?")
                .bind(&source_id)
                .execute(&mut *tx)
                .await?;
        }
        for segment in segments {
            let metadata = json!({"source":"video_segment","videoPrepared":true,"frameStorageKey":segment.frame_storage_key,"sourceStorageKey":source_key,"audioUnderstood":false,"needsDescription":true});
            let hash = ingest::hash_content(
                None,
                Some(&segment.storage_key),
                Some(segment.start_ms),
                Some(segment.end_ms),
                &metadata,
            );
            sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,media_storage_key,start_ms,end_ms,metadata_json,content_sha256,evidence_status) VALUES(?,?,?,'video',?,?,?,?,?,'visual_only')")
                .bind(Uuid::new_v4().to_string()).bind(rev).bind(next).bind(&segment.storage_key).bind(segment.start_ms).bind(segment.end_ms).bind(metadata.to_string()).bind(hash).execute(&mut *tx).await?;
            next += 1;
        }
    }
    // Missing ASR never prevents visual indexing, but audio without reviewed
    // transcript must not count as semantic evidence.
    sqlx::query("UPDATE kb_content_parts SET evidence_status=CASE WHEN content_text IS NOT NULL AND length(trim(content_text))>0 THEN 'approved' WHEN modality IN ('image','video') THEN 'visual_only' ELSE 'unavailable' END WHERE revision_id=?")
        .bind(rev).execute(&mut *tx).await?;
    let usable:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_content_parts WHERE revision_id=? AND evidence_status IN ('approved','visual_only')").bind(rev).fetch_one(&mut *tx).await?;
    if usable == 0 {
        return Err(ApiError::bad_request(
            "没有可检索正文或视觉素材，请先补充教师确认的字幕/说明",
        ));
    }
    sqlx::query("UPDATE kb_asset_revisions SET review_status='approved',status='review',index_status='pending',review_note=? WHERE revision_id=?")
        .bind(format!("Reviewed by {actor}")).bind(rev).execute(&mut *tx).await?;
    sqlx::query("UPDATE kb_asset_revisions SET status='ready',index_status='not_required' WHERE revision_id=? AND asset_id IN (SELECT asset_id FROM kb_assets WHERE evidence_mode='file')")
        .bind(rev).execute(&mut *tx).await?;
    crate::lexical::index_revision(&mut tx, rev).await?;
    audit(
        &mut tx,
        actor,
        "asset.review",
        asset,
        json!({"revisionId":rev,"retrievablePartCount":usable}),
    )
    .await?;
    tx.commit().await?;
    files.retained = true;
    Ok(())
}

#[derive(Default)]
struct DerivativeFiles {
    keys: Vec<String>,
    retained: bool,
    storage: Option<crate::storage::BlobStore>,
}

fn content_type_for_key(key: &str) -> &'static str {
    match key.rsplit_once('.').map(|(_, ext)| ext) {
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        _ => "video/mp4",
    }
}
impl Drop for DerivativeFiles {
    fn drop(&mut self) {
        if !self.retained {
            for key in &self.keys {
                if !key.contains(['/', '\\']) {
                    if let Some(storage) = &self.storage {
                        let _ = std::fs::remove_file(storage.local_dir().join(key));
                    }
                }
            }
            if let Some(storage) = self.storage.clone().filter(|storage| storage.is_remote()) {
                let keys = self.keys.clone();
                tokio::spawn(async move {
                    for key in keys {
                        if storage.delete(&key).await.is_err() {
                            tracing::warn!(storage_key = %key, "failed to clean up uncommitted OSS derivative");
                        }
                    }
                });
            }
        }
    }
}

fn parse_metadata(raw: Option<String>) -> Value {
    raw.and_then(|v| serde_json::from_str::<Value>(&v).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

async fn reindex_asset(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(asset): Path<String>,
) -> Result<(StatusCode, Json<AssetView>), ApiError> {
    let user = require_admin(&state, &headers).await?;
    let rev = latest_revision(&state.db, &asset).await?;
    let current = revision(&state.db, &rev).await?;
    if current.review != "approved" {
        return Err(ApiError::bad_request("请先审核当前资料版本"));
    }
    if ingest::load_asset(&state.db, &asset).await?.evidence_mode == "file" {
        return Err(ApiError::bad_request(
            "此资料为直接证据，不需要向量化，审核后可直接发布",
        ));
    }
    let config = models::load(&state, "embedding").await?;
    require_config(&config)?;
    check_course_space(&state.db, &current.course, &config.fingerprint()).await?;
    let fingerprint = config.fingerprint();
    let job = Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("index:{rev}:{fingerprint}").as_bytes(),
    )
    .to_string();
    let mut tx = state.db.begin().await?;
    ensure_latest_tx(&mut tx, &asset, &rev).await?;
    let running: Option<String> = sqlx::query_scalar("SELECT state FROM kb_jobs WHERE job_id=?")
        .bind(&job)
        .fetch_optional(&mut *tx)
        .await?;
    if !running
        .as_deref()
        .is_some_and(|s| s == "queued" || s == "running")
    {
        sqlx::query("INSERT INTO kb_jobs(job_id,job_type,asset_id,revision_id,state) VALUES(?,'index_revision',?,?,'queued') ON CONFLICT(job_id) DO UPDATE SET state='queued',attempts=0,error_code=NULL,error_message=NULL,available_at=CURRENT_TIMESTAMP,started_at=NULL,finished_at=NULL")
            .bind(&job).bind(&asset).bind(&rev).execute(&mut *tx).await?;
        sqlx::query("UPDATE kb_jobs SET progress_completed=0,progress_total=(SELECT COUNT(*) FROM kb_content_parts WHERE revision_id=? AND evidence_status IN ('approved','visual_only')),progress_stage='queued' WHERE job_id=?")
            .bind(&rev).bind(&job).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO kb_index_requests(job_id,revision_id,config_fingerprint,actor_id) VALUES(?,?,?,?) ON CONFLICT(job_id) DO UPDATE SET actor_id=excluded.actor_id")
            .bind(&job).bind(&rev).bind(&fingerprint).bind(&user.id).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO kb_index_outbox(id,revision_id,state) VALUES(?,?,'pending') ON CONFLICT(id) DO UPDATE SET state='pending',attempts=0,error_code=NULL,updated_at=CURRENT_TIMESTAMP")
            .bind(&job).bind(&rev).execute(&mut *tx).await?;
        sqlx::query("UPDATE kb_asset_revisions SET index_status=CASE WHEN status='published' AND index_status='verified' THEN index_status ELSE 'pending' END,status=CASE WHEN status='published' THEN status ELSE 'indexing' END WHERE revision_id=?")
            .bind(&rev).execute(&mut *tx).await?;
        audit(
            &mut tx,
            &user.id,
            "asset.reindex_queued",
            &asset,
            json!({"jobId":job,"revisionId":rev,"configFingerprint":fingerprint}),
        )
        .await?;
    }
    tx.commit().await?;
    kick_worker(state.clone());
    Ok((
        StatusCode::ACCEPTED,
        Json(ingest::load_asset(&state.db, &asset).await?),
    ))
}

fn require_config(config: &ModelConfig) -> Result<(), ApiError> {
    if !config.enabled
        || config
            .api_key
            .as_deref()
            .is_none_or(|k| k.trim().is_empty())
    {
        return Err(ApiError::unavailable("请先启用并配置 Embedding 模型密钥"));
    }
    if config.dimension == 0 {
        return Err(ApiError::bad_request("向量维度无效"));
    }
    Ok(())
}

/// Retain the course's active vector space until an explicit full-course
/// rebuild can verify every published revision. A single-asset reindex never
/// changes a course's embedding generation.
async fn check_course_space(
    db: &SqlitePool,
    course: &str,
    fingerprint: &str,
) -> Result<(), ApiError> {
    let fingerprints:Vec<String>=sqlx::query_scalar("SELECT DISTINCT e.config_fingerprint FROM kb_vector_collections c JOIN kb_embedding_configs e USING(embedding_config_id) WHERE c.collection_id IN (SELECT collection_id FROM kb_active_indexes WHERE course_id=? UNION SELECT r.collection_id FROM kb_asset_revisions r JOIN kb_assets a USING(asset_id) JOIN kb_course_nodes n ON n.node_id=a.node_id WHERE n.course_id=? AND a.published_revision_id=r.revision_id AND a.deleted_at IS NULL)")
        .bind(course).bind(course).fetch_all(db).await?;
    if fingerprints.iter().any(|existing| existing != fingerprint) {
        return Err(ApiError::bad_request(
            "Embedding 配置与课程当前索引不一致，需要完整课程重建；单资料重建不会切换课程索引",
        ));
    }
    Ok(())
}

fn kick_worker(state: AppState) {
    if state.worker_running.swap(true, Ordering::SeqCst) {
        return;
    }
    tokio::spawn(async move {
        struct RunningGuard(std::sync::Arc<std::sync::atomic::AtomicBool>);
        impl Drop for RunningGuard {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _running = RunningGuard(state.worker_running.clone());
        loop {
            {
                // Never keep the workflow locked while the worker is idle.
                // Editors also use this lock to create immutable revisions.
                let _guard = state.workflow_lock.lock().await;
                if let Err(error) = drain_jobs(&state).await {
                    tracing::warn!(code = error.code, "index worker will retry queued tasks");
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// Invoke once after migrations and state initialization, before serving.
/// Running jobs represent the previous process and are safe to replay because
/// every Qdrant point has a deterministic ID and is verified after upsert.
pub async fn resume_jobs(state: AppState) {
    let reset=async {
        let mut tx=state.db.begin().await?;
        sqlx::query("UPDATE kb_jobs SET state='queued',available_at=CURRENT_TIMESTAMP WHERE (job_type IN ('index_revision','asset_cleanup') AND state='running') OR (job_type='asset_cleanup' AND state='failed')").execute(&mut *tx).await?;
        sqlx::query("UPDATE kb_index_outbox SET state='pending',updated_at=CURRENT_TIMESTAMP WHERE state='running'").execute(&mut *tx).await?;
        sqlx::query("UPDATE kb_asset_revisions SET extraction_json=json_set(extraction_json,'$.reviewProgress.state','failed','$.reviewProgress.stage','interrupted','$.reviewProgress.message','上次审核因服务重启中断，请重新点击审核通过') WHERE json_extract(extraction_json,'$.reviewProgress.state')='running'").execute(&mut *tx).await?;
        tx.commit().await
    }.await;
    if reset.is_err() {
        tracing::error!("unable to restore indexing jobs");
        return;
    }
    kick_worker(state);
}

async fn drain_jobs(state: &AppState) -> Result<(), ApiError> {
    loop {
        let claimed=sqlx::query("UPDATE kb_jobs SET state='running',attempts=attempts+1,started_at=CURRENT_TIMESTAMP WHERE job_id=(SELECT j.job_id FROM kb_jobs j WHERE j.state='queued' AND j.available_at<=CURRENT_TIMESTAMP AND (j.job_type='asset_cleanup' OR (j.job_type='index_revision' AND EXISTS(SELECT 1 FROM kb_index_requests q WHERE q.job_id=j.job_id))) ORDER BY j.created_at,j.job_id LIMIT 1) AND state='queued' RETURNING job_id,job_type,asset_id,revision_id,attempts")
            .fetch_optional(&state.db).await?;
        let Some(job) = claimed else {
            return Ok(());
        };
        let id: String = job.get("job_id");
        let rev: Option<String> = job.get("revision_id");
        let kind: String = job.get("job_type");
        let attempts: i64 = job.get("attempts");
        sqlx::query("UPDATE kb_index_outbox SET state='running',attempts=?,updated_at=CURRENT_TIMESTAMP WHERE id=?").bind(attempts).bind(&id).execute(&state.db).await?;
        let operation = async {
            if kind == "asset_cleanup" {
                cleanup_asset(state, &job.get::<String, _>("asset_id")).await
            } else {
                index_revision(state, &id, rev.as_deref().ok_or_else(ApiError::internal)?).await
            }
        };
        let result = match tokio::time::timeout(Duration::from_secs(1800), operation).await {
            Ok(result) => result,
            Err(_) => Err(ApiError::unavailable("索引任务超时，请重试")),
        };
        match result {
            Ok(()) => {
                let mut tx = state.db.begin().await?;
                let completed = sqlx::query("UPDATE kb_jobs SET state='succeeded',finished_at=CURRENT_TIMESTAMP,error_code=NULL,error_message=NULL WHERE job_id=? AND state='running'").bind(&id).execute(&mut *tx).await?;
                if completed.rows_affected() == 1 {
                    sqlx::query("UPDATE kb_index_outbox SET state='succeeded',error_code=NULL,updated_at=CURRENT_TIMESTAMP WHERE id=?").bind(&id).execute(&mut *tx).await?;
                }
                tx.commit().await?;
            }
            Err(error) => {
                // A failed network deletion must never permanently abandon
                // owned OSS files/vectors. Schedule it without blocking other jobs.
                let retry = kind == "asset_cleanup"
                    || (error.status == StatusCode::SERVICE_UNAVAILABLE && attempts < 3);
                let next = if retry { "queued" } else { "failed" };
                let mut tx = state.db.begin().await?;
                let completed = sqlx::query("UPDATE kb_jobs SET state=?,error_code=?,error_message=?,finished_at=CURRENT_TIMESTAMP WHERE job_id=? AND state='running'")
                    .bind(next).bind(error.code).bind(&error.message).bind(&id).execute(&mut *tx).await?;
                if completed.rows_affected() == 0 {
                    // Withdrawal/deletion wins over an in-flight worker's result.
                    tx.commit().await?;
                    continue;
                }
                sqlx::query("UPDATE kb_index_outbox SET state=?,error_code=?,updated_at=CURRENT_TIMESTAMP WHERE id=?")
                    .bind(if retry {"pending"} else {"failed"}).bind(error.code).bind(&id).execute(&mut *tx).await?;
                sqlx::query("UPDATE kb_asset_revisions SET index_status=CASE WHEN status IN ('published','withdrawn') AND index_status='verified' THEN index_status ELSE 'failed' END,status=CASE WHEN status IN ('published','withdrawn') THEN status ELSE 'failed' END WHERE revision_id=?").bind(&rev).execute(&mut *tx).await?;
                tx.commit().await?;
                if retry {
                    let delay = 1u64 << attempts.clamp(1, 8) as u32;
                    sqlx::query("UPDATE kb_jobs SET available_at=datetime('now',?),finished_at=NULL WHERE job_id=? AND state='queued'")
                        .bind(format!("+{delay} seconds")).bind(&id).execute(&state.db).await?;
                }
            }
        }
    }
}

async fn cleanup_asset(state: &AppState, asset: &str) -> Result<(), ApiError> {
    let deleted: Option<String> =
        sqlx::query_scalar("SELECT deleted_at FROM kb_assets WHERE asset_id=?")
            .bind(asset)
            .fetch_one(&state.db)
            .await?;
    if deleted.is_none() {
        return Err(ApiError::bad_request("资料未删除，不能回收原件"));
    }
    let collections:Vec<String>=sqlx::query_scalar("SELECT DISTINCT c.qdrant_collection FROM kb_vector_collections c JOIN kb_vector_records v USING(collection_id) WHERE v.revision_id IN (SELECT revision_id FROM kb_asset_revisions WHERE asset_id=?)").bind(asset).fetch_all(&state.db).await?;
    for collection in collections {
        let response = state
            .qdrant
            .get(&format!("collections/{collection}"))
            .await
            .map_err(|_| ApiError::unavailable("清理时向量数据库不可用"))?;
        if response.status() == StatusCode::NOT_FOUND {
            continue;
        }
        state.qdrant.delete_asset(&collection, asset).await?;
    }
    // Preserve keys still used by another live asset/revision, including
    // teacher-cloned segments and frame keys inside metadata.
    let rows=sqlx::query("SELECT a.asset_id,a.deleted_at,a.storage_key,p.media_storage_key,p.metadata_json FROM kb_assets a LEFT JOIN kb_asset_revisions r USING(asset_id) LEFT JOIN kb_content_parts p USING(revision_id)").fetch_all(&state.db).await?;
    let mut candidates = HashSet::new();
    let mut protected = HashSet::new();
    for row in rows {
        let row_asset: String = row.get("asset_id");
        let mut keys = vec![row.get::<String, _>("storage_key")];
        if let Some(key) = row.get::<Option<String>, _>("media_storage_key") {
            keys.push(key);
        }
        let metadata = parse_metadata(row.get("metadata_json"));
        for field in ["frameStorageKey", "sourceStorageKey"] {
            if let Some(key) = metadata.get(field).and_then(Value::as_str) {
                keys.push(key.to_owned());
            }
        }
        if row_asset == asset {
            candidates.extend(keys);
        } else if row.get::<Option<String>, _>("deleted_at").is_none() {
            protected.extend(keys);
        }
    }
    for key in candidates.difference(&protected) {
        if key.contains(['/', '\\']) || key.starts_with('.') {
            return Err(ApiError::bad_request("回收文件包含无效存储键"));
        }
        state.storage.delete(key).await?;
    }
    sqlx::query("UPDATE kb_vector_records SET status='deleted' WHERE revision_id IN (SELECT revision_id FROM kb_asset_revisions WHERE asset_id=?)").bind(asset).execute(&state.db).await?;
    sqlx::query("DELETE FROM kb_media_tickets WHERE asset_id=?")
        .bind(asset)
        .execute(&state.db)
        .await?;
    Ok(())
}

async fn index_revision(state: &AppState, job: &str, rev: &str) -> Result<(), ApiError> {
    let current = revision(&state.db, rev).await?;
    if current.review != "approved" || current.status == "withdrawn" {
        return Err(ApiError::bad_request("版本尚未审核或已经撤回"));
    }
    let config = models::load(state, "embedding").await?;
    require_config(&config)?;
    let fingerprint = config.fingerprint();
    let requested: String =
        sqlx::query_scalar("SELECT config_fingerprint FROM kb_index_requests WHERE job_id=?")
            .bind(job)
            .fetch_one(&state.db)
            .await?;
    if fingerprint != requested {
        return Err(ApiError::bad_request(
            "索引排队后模型配置发生变化，请按新配置重新提交",
        ));
    }
    check_course_space(&state.db, &current.course, &fingerprint).await?;
    let (collection_id, collection) = select_collection(state, &current.course, &config).await?;
    let parts=sqlx::query("SELECT * FROM kb_content_parts WHERE revision_id=? AND evidence_status IN ('approved','visual_only') ORDER BY part_index").bind(rev).fetch_all(&state.db).await?;
    if parts.is_empty() {
        return Err(ApiError::bad_request("没有审核后的可检索内容"));
    }
    let original_name: String = sqlx::query_scalar(
        "SELECT original_name FROM kb_assets WHERE asset_id=? AND deleted_at IS NULL",
    )
    .bind(&current.asset)
    .fetch_one(&state.db)
    .await?;
    sqlx::query("UPDATE kb_jobs SET progress_total=?,progress_completed=0,progress_stage='embedding' WHERE job_id=? AND state='running'")
        .bind(parts.len() as i64).bind(job).execute(&state.db).await?;
    let mut ids = Vec::new();
    for part in &parts {
        ensure_job_active(state, job, rev).await?;
        let part_id: String = part.get("part_id");
        let modality: String = part.get("modality");
        let hash: String = part.get("content_sha256");
        // Include the original filename in the embedding input and point
        // identity so a reindex refreshes old vectors when the searchable
        // metadata changes. Existing points remain readable until replaced.
        let embedding_hash = format!("{hash}|{original_name}");
        let point = point_id(rev, &part_id, &fingerprint, &embedding_hash);
        let payload = json!({"source_system":"existing-course-api","course_id":current.course,"node_id":current.node,"asset_id":current.asset,"revision_id":rev,"part_id":part_id,"modality":modality,"source_name":original_name,"config_fingerprint":fingerprint,"content_sha256":hash});
        sqlx::query("INSERT INTO kb_vector_records(vector_record_id,collection_id,revision_id,part_id,qdrant_point_id,modality,dimensions,status) VALUES(?,?,?,?,?,?,?,'pending') ON CONFLICT(collection_id,part_id) DO UPDATE SET qdrant_point_id=excluded.qdrant_point_id,status=CASE WHEN kb_vector_records.status='verified' THEN kb_vector_records.status ELSE 'pending' END")
            .bind(Uuid::new_v5(&Uuid::NAMESPACE_URL,format!("record:{collection_id}:{part_id}").as_bytes()).to_string()).bind(&collection_id).bind(rev).bind(&part_id).bind(&point).bind(&modality).bind(config.dimension as i64).execute(&state.db).await?;
        if !verify_point(state, &collection, &point, &payload, config.dimension).await? {
            let text: Option<String> = part.get("content_text");
            let media_key: Option<String> = part.get("media_storage_key");
            let mut input = Vec::new();
            if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                input.push(json!({"type":"text","text":format!("课程资料文件名：{original_name}\n资料内容：{text}")}));
            }
            if matches!(modality.as_str(), "image" | "video") {
                let key = media_key
                    .as_deref()
                    .ok_or_else(|| ApiError::bad_request("视觉片段缺少媒体文件"))?;
                state.storage.ensure_local(key).await?;
                if !input.iter().any(|item| item["type"] == "text") {
                    input.push(
                        json!({"type":"text","text":format!("课程资料文件名：{original_name}")}),
                    );
                }
                input.push(
                    embedding::media_input(state.storage.local_dir(), key, &modality, config.fps)
                        .await?,
                );
            }
            if input.is_empty() {
                return Err(ApiError::bad_request(
                    "音频须补充教师确认字幕，不能把文件名当作知识向量",
                ));
            }
            let vector = embedding::embed(&config, input).await?;
            ensure_job_active(state, job, rev).await?;
            state
                .qdrant
                .upsert(
                    &collection,
                    json!({"id":point,"vector":vector,"payload":payload}),
                )
                .await?;
            sqlx::query(
                "UPDATE kb_vector_records SET status=CASE WHEN status='verified' THEN status ELSE 'written' END WHERE collection_id=? AND part_id=?",
            )
            .bind(&collection_id)
            .bind(&part_id)
            .execute(&state.db)
            .await?;
            if !verify_point(state, &collection, &point, &payload, config.dimension).await? {
                return Err(ApiError::unavailable("向量写入后内容、维度或指纹核验失败"));
            }
        }
        sqlx::query(
            "UPDATE kb_vector_records SET status='verified' WHERE collection_id=? AND part_id=?",
        )
        .bind(&collection_id)
        .bind(&part_id)
        .execute(&state.db)
        .await?;
        ids.push(point);
        sqlx::query("UPDATE kb_jobs SET progress_completed=? WHERE job_id=? AND state='running'")
            .bind(ids.len() as i64)
            .bind(job)
            .execute(&state.db)
            .await?;
    }
    sqlx::query("UPDATE kb_jobs SET progress_stage='verifying' WHERE job_id=? AND state='running'")
        .bind(job)
        .execute(&state.db)
        .await?;
    if !state.qdrant.verify(&collection, &ids).await? {
        return Err(ApiError::unavailable("向量记录完整性核验失败"));
    }
    if models::load(state, "embedding").await?.fingerprint() != fingerprint {
        return Err(ApiError::bad_request("索引期间模型配置已更改，请重新提交"));
    }
    ensure_job_active(state, job, rev).await?;
    let mut tx = state.db.begin().await?;
    let updated=sqlx::query("UPDATE kb_asset_revisions SET index_status='verified',status=CASE WHEN status='published' THEN status ELSE 'ready' END,embedding_config_id=?,collection_id=?,expected_vector_count=? WHERE revision_id=? AND review_status='approved' AND status!='withdrawn' AND EXISTS(SELECT 1 FROM kb_jobs WHERE job_id=? AND state='running') AND EXISTS(SELECT 1 FROM kb_assets a JOIN kb_course_nodes n ON a.node_id=n.node_id WHERE a.asset_id=kb_asset_revisions.asset_id AND a.deleted_at IS NULL AND n.is_active=1)")
        .bind(&fingerprint).bind(&collection_id).bind(ids.len() as i64).bind(rev).bind(job).execute(&mut *tx).await?;
    if updated.rows_affected() != 1 {
        return Err(ApiError::bad_request("资料已被撤回或删除，索引不能发布"));
    }
    tx.commit().await?;
    Ok(())
}

pub fn point_id(revision: &str, part: &str, fingerprint: &str, content_hash: &str) -> String {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!("kb:{revision}:{part}:{fingerprint}:{content_hash}").as_bytes(),
    )
    .to_string()
}

async fn ensure_job_active(state: &AppState, job: &str, rev: &str) -> Result<(), ApiError> {
    let valid:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_jobs j JOIN kb_asset_revisions r ON r.revision_id=j.revision_id JOIN kb_assets a ON a.asset_id=r.asset_id JOIN kb_course_nodes n ON a.node_id=n.node_id WHERE j.job_id=? AND r.revision_id=? AND j.state='running' AND a.deleted_at IS NULL AND r.review_status='approved' AND r.status!='withdrawn' AND n.is_active=1")
        .bind(job).bind(rev).fetch_one(&state.db).await?;
    if valid != 1 {
        return Err(ApiError::bad_request("任务已取消，资料已撤回或节点失效"));
    }
    Ok(())
}

async fn select_collection(
    state: &AppState,
    course: &str,
    config: &ModelConfig,
) -> Result<(String, String), ApiError> {
    let fingerprint = config.fingerprint();
    let mut tx = state.db.begin().await?;
    sqlx::query("INSERT INTO kb_embedding_configs(embedding_config_id,provider,model,dimensions,instructions_fingerprint,config_fingerprint,is_active,created_by) VALUES(?,'volcengine-ark',?,?,?,?,0,'index-worker') ON CONFLICT(config_fingerprint) DO NOTHING")
        .bind(&fingerprint).bind(&config.model).bind(config.dimension as i64).bind(format!("{:x}",Sha256::digest(config.instructions.as_bytes()))).bind(&fingerprint).execute(&mut *tx).await?;
    let existing=sqlx::query("SELECT collection_id,qdrant_collection FROM kb_vector_collections WHERE course_id=? AND embedding_config_id=? AND status!='retired' ORDER BY generation DESC LIMIT 1")
        .bind(course).bind(&fingerprint).fetch_optional(&mut *tx).await?;
    let (id, name) = if let Some(row) = existing {
        (
            row.get::<String, _>("collection_id"),
            row.get::<String, _>("qdrant_collection"),
        )
    } else {
        let generation:i64=sqlx::query_scalar("SELECT COALESCE(MAX(generation),0)+1 FROM kb_vector_collections WHERE course_id=? AND embedding_config_id=?").bind(course).bind(&fingerprint).fetch_one(&mut *tx).await?;
        let id = Uuid::new_v4().to_string();
        let course_hash = format!("{:x}", Sha256::digest(course.as_bytes()));
        let name = format!(
            "kb_{}_{}_{}_g{generation}",
            state.environment.name(),
            &course_hash[..16],
            &fingerprint[..24]
        );
        sqlx::query("INSERT INTO kb_vector_collections(collection_id,course_id,embedding_config_id,generation,qdrant_collection,status) VALUES(?,?,?,?,?,'building')")
            .bind(&id).bind(course).bind(&fingerprint).bind(generation).bind(&name).execute(&mut *tx).await?;
        (id, name)
    };
    tx.commit().await?;
    state
        .qdrant
        .ensure_collection(&name, config.dimension)
        .await?;
    let response = state
        .qdrant
        .get(&format!("collections/{name}"))
        .await
        .map_err(|_| ApiError::unavailable("向量collection核验失败"))?;
    let body: Value = response
        .json()
        .await
        .map_err(|_| ApiError::unavailable("向量collection响应无效"))?;
    let vectors = body
        .pointer("/result/config/params/vectors")
        .ok_or_else(|| ApiError::unavailable("向量collection缺少维度配置"))?;
    if vectors.get("size").and_then(Value::as_u64) != Some(config.dimension as u64)
        || vectors.get("distance").and_then(Value::as_str) != Some("Cosine")
    {
        return Err(ApiError::bad_request(
            "已有collection维度或距离算法不匹配，须重建索引",
        ));
    }
    Ok((id, name))
}

async fn verify_point(
    state: &AppState,
    collection: &str,
    id: &str,
    payload: &Value,
    dimension: usize,
) -> Result<bool, ApiError> {
    let response = state
        .qdrant
        .get(&format!(
            "collections/{collection}/points/{id}?with_payload=true&with_vector=true"
        ))
        .await
        .map_err(|_| ApiError::unavailable("向量记录核验连接失败"))?;
    if response.status() == StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !response.status().is_success() {
        return Err(ApiError::unavailable("向量记录核验失败"));
    }
    let body: Value = response
        .json()
        .await
        .map_err(|_| ApiError::unavailable("向量记录响应无效"))?;
    let Some(result) = body.get("result") else {
        return Ok(false);
    };
    let valid_vector = result
        .get("vector")
        .and_then(Value::as_array)
        .is_some_and(|v| {
            v.len() == dimension && v.iter().all(|n| n.as_f64().is_some_and(f64::is_finite))
        });
    let valid_payload = payload.as_object().is_some_and(|fields| {
        fields
            .iter()
            .all(|(key, value)| result.get("payload").and_then(|p| p.get(key)) == Some(value))
    });
    Ok(valid_vector && valid_payload && result.get("id").and_then(Value::as_str) == Some(id))
}

#[derive(Deserialize)]
struct StatusInput {
    status: String,
}

async fn update_status(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(asset): Path<String>,
    Json(input): Json<StatusInput>,
) -> Result<Json<AssetView>, ApiError> {
    let user = require_admin(&state, &headers).await?;
    match input.status.as_str() {
        "published" => publish(&state, &asset, &user.id).await?,
        "withdrawn" => withdraw(&state, &asset, &user.id).await?,
        _ => return Err(ApiError::bad_request("不支持的发布状态")),
    }
    Ok(Json(ingest::load_asset(&state.db, &asset).await?))
}

async fn publish(state: &AppState, asset: &str, actor: &str) -> Result<(), ApiError> {
    let rev = latest_revision(&state.db, asset).await?;
    let current = revision(&state.db, &rev).await?;
    if ingest::load_asset(&state.db, asset).await?.evidence_mode == "file" {
        if current.review != "approved" || current.index != "not_required" {
            return Err(ApiError::bad_request("直接证据需要先审核"));
        }
        let mut tx = state.db.begin().await?;
        ensure_latest_tx(&mut tx, asset, &rev).await?;
        sqlx::query("UPDATE kb_asset_revisions SET status='ready' WHERE asset_id=? AND status='published' AND revision_id!=?")
            .bind(asset).bind(&rev).execute(&mut *tx).await?;
        let changed = sqlx::query("UPDATE kb_asset_revisions SET status='published',published_at=CURRENT_TIMESTAMP WHERE revision_id=? AND review_status='approved' AND index_status='not_required'")
            .bind(&rev).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(ApiError::bad_request("审核状态已变化，请刷新"));
        }
        sqlx::query(
            "UPDATE kb_assets SET published_revision_id=? WHERE asset_id=? AND deleted_at IS NULL",
        )
        .bind(&rev)
        .bind(asset)
        .execute(&mut *tx)
        .await?;
        audit(
            &mut tx,
            actor,
            "asset.published",
            asset,
            json!({"revisionId":rev,"evidenceMode":"file"}),
        )
        .await?;
        tx.commit().await?;
        return Ok(());
    }
    if current.review != "approved" || current.index != "verified" {
        return Err(ApiError::bad_request("必须完成审核和向量核验后才能发布"));
    }
    let collection = current
        .collection
        .ok_or_else(|| ApiError::bad_request("版本没有完整索引"))?;
    let row=sqlx::query("SELECT c.qdrant_collection,e.config_fingerprint,e.dimensions FROM kb_vector_collections c JOIN kb_embedding_configs e USING(embedding_config_id) WHERE c.collection_id=?").bind(&collection).fetch_one(&state.db).await?;
    let fingerprint: String = row.get("config_fingerprint");
    if models::load(state, "embedding").await?.fingerprint() != fingerprint {
        return Err(ApiError::bad_request(
            "当前模型与资料索引配置不一致，需要课程重建",
        ));
    }
    check_course_space(&state.db, &current.course, &fingerprint).await?;
    let records=sqlx::query("SELECT v.qdrant_point_id,v.status,p.part_id,p.content_sha256,p.modality FROM kb_vector_records v JOIN kb_content_parts p USING(part_id) WHERE v.revision_id=? AND v.collection_id=?").bind(&rev).bind(&collection).fetch_all(&state.db).await?;
    let expected: i64 = sqlx::query_scalar(
        "SELECT expected_vector_count FROM kb_asset_revisions WHERE revision_id=?",
    )
    .bind(&rev)
    .fetch_one(&state.db)
    .await?;
    if expected == 0 || records.len() as i64 != expected {
        return Err(ApiError::bad_request("索引记录不完整，请重新索引"));
    }
    let name: String = row.get("qdrant_collection");
    for record in &records {
        if record.get::<String, _>("status") != "verified" {
            return Err(ApiError::bad_request("索引尚未全部核验"));
        }
        let payload = json!({"course_id":current.course,"node_id":current.node,"asset_id":asset,"revision_id":rev,"part_id":record.get::<String,_>("part_id"),"modality":record.get::<String,_>("modality"),"config_fingerprint":fingerprint,"content_sha256":record.get::<String,_>("content_sha256")});
        if !verify_point(
            state,
            &name,
            &record.get::<String, _>("qdrant_point_id"),
            &payload,
            row.get::<i64, _>("dimensions") as usize,
        )
        .await?
        {
            return Err(ApiError::unavailable("发布前向量核验失败，请重建索引"));
        }
    }
    let mut tx = state.db.begin().await?;
    ensure_latest_tx(&mut tx, asset, &rev).await?;
    let current_config: Option<String> =
        sqlx::query_scalar("SELECT config_json FROM kb_runtime_configs WHERE kind='embedding'")
            .fetch_optional(&mut *tx)
            .await?;
    if let Some(raw) = current_config {
        let config: ModelConfig = serde_json::from_str(&raw).map_err(|_| ApiError::internal())?;
        if config.fingerprint() != fingerprint {
            return Err(ApiError::bad_request("发布时模型配置已变化，请重新索引"));
        }
    }
    let active: Option<String> =
        sqlx::query_scalar("SELECT collection_id FROM kb_active_indexes WHERE course_id=?")
            .bind(&current.course)
            .fetch_optional(&mut *tx)
            .await?;
    if active.as_deref().is_some_and(|id| id != collection) {
        return Err(ApiError::bad_request(
            "课程已有不同活动索引，需要完整课程重建",
        ));
    }
    let foreign:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_assets a JOIN kb_course_nodes n ON a.node_id=n.node_id JOIN kb_asset_revisions r ON r.revision_id=a.published_revision_id WHERE n.course_id=? AND a.deleted_at IS NULL AND a.evidence_mode='content' AND (r.collection_id IS NULL OR r.collection_id!=?)").bind(&current.course).bind(&collection).fetch_one(&mut *tx).await?;
    if foreign > 0 {
        return Err(ApiError::bad_request(
            "课程已有其他索引代际的已发布资料，需要成套重建后切换",
        ));
    }
    // One transaction switches the publication pointer, activates the first
    // collection, and updates FTS visibility. Vector writes alone publish none.
    sqlx::query("INSERT INTO kb_active_indexes(course_id,collection_id) VALUES(?,?) ON CONFLICT(course_id) DO NOTHING").bind(&current.course).bind(&collection).execute(&mut *tx).await?;
    sqlx::query("UPDATE kb_vector_collections SET status='active' WHERE collection_id=?")
        .bind(&collection)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE kb_asset_revisions SET status='ready' WHERE asset_id=? AND status='published' AND revision_id!=?").bind(asset).bind(&rev).execute(&mut *tx).await?;
    let changed=sqlx::query("UPDATE kb_asset_revisions SET status='published',published_at=CURRENT_TIMESTAMP WHERE revision_id=? AND review_status='approved' AND index_status='verified'").bind(&rev).execute(&mut *tx).await?;
    if changed.rows_affected() != 1 {
        return Err(ApiError::bad_request("审核或索引状态已经变化，请刷新"));
    }
    sqlx::query(
        "UPDATE kb_assets SET published_revision_id=? WHERE asset_id=? AND deleted_at IS NULL",
    )
    .bind(&rev)
    .bind(asset)
    .execute(&mut *tx)
    .await?;
    // Migration triggers maintain FTS. Readers still JOIN this newly committed
    // publication pointer and current node authorization before returning text.
    audit(
        &mut tx,
        actor,
        "asset.published",
        asset,
        json!({"revisionId":rev,"collectionId":collection}),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn withdraw(state: &AppState, asset: &str, actor: &str) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kb_assets WHERE asset_id=? AND deleted_at IS NULL",
    )
    .bind(asset)
    .fetch_one(&mut *tx)
    .await?;
    if exists != 1 {
        return Err(ApiError::not_found("资料不存在"));
    }
    sqlx::query("UPDATE kb_asset_revisions SET status='withdrawn' WHERE revision_id=(SELECT published_revision_id FROM kb_assets WHERE asset_id=?)").bind(asset).execute(&mut *tx).await?;
    sqlx::query("UPDATE kb_assets SET published_revision_id=NULL WHERE asset_id=?")
        .bind(asset)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE kb_jobs SET state='cancelled',finished_at=CURRENT_TIMESTAMP WHERE asset_id=? AND job_type='index_revision' AND state IN ('queued','running')").bind(asset).execute(&mut *tx).await?;
    sqlx::query("UPDATE kb_asset_revisions SET status=CASE WHEN index_status='verified' THEN 'ready' ELSE 'review' END WHERE asset_id=? AND status='indexing'").bind(asset).execute(&mut *tx).await?;
    audit(&mut tx, actor, "asset.withdrawn", asset, json!({})).await?;
    tx.commit().await?;
    Ok(())
}

async fn ensure_latest_tx(
    tx: &mut Transaction<'_, Sqlite>,
    asset: &str,
    rev: &str,
) -> Result<(), ApiError> {
    let latest:Option<String>=sqlx::query_scalar("SELECT r.revision_id FROM kb_asset_revisions r JOIN kb_assets a USING(asset_id) JOIN kb_course_nodes n ON n.node_id=a.node_id WHERE a.asset_id=? AND a.deleted_at IS NULL AND n.level=2 AND n.is_active=1 ORDER BY r.revision_number DESC LIMIT 1").bind(asset).fetch_optional(&mut **tx).await?;
    if latest.as_deref() != Some(rev) {
        return Err(ApiError::bad_request(
            "资料已有新版本或节点失效，请刷新后重试",
        ));
    }
    Ok(())
}

async fn audit(
    tx: &mut Transaction<'_, Sqlite>,
    actor: &str,
    action: &str,
    asset: &str,
    detail: Value,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO kb_audit_events(audit_id,actor_id,action,resource_type,resource_id,detail_json) VALUES(?,?,?,'asset',?,?)").bind(Uuid::new_v4().to_string()).bind(actor).bind(action).bind(asset).bind(detail.to_string()).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[ignore = "requires KB_TEST_VIDEO_PATH and local ffmpeg/ffprobe; uses only an isolated processing directory"]
    async fn supplied_video_can_be_probed_saved_and_reviewed() {
        let source = std::env::var_os("KB_TEST_VIDEO_PATH").expect("set KB_TEST_VIDEO_PATH");
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data/verification")
            .join(format!("video-range-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let key = format!("{}.mp4", Uuid::new_v4());
        tokio::fs::copy(source, root.join(&key)).await.unwrap();
        let (mut state, _) = crate::db::test_published_state().await;
        state.storage = crate::storage::BlobStore::new(crate::config::StorageConfig::Local {
            local_dir: root.clone(),
        });
        sqlx::query("UPDATE kb_assets SET asset_type='video',mime_type='video/mp4',storage_key=? WHERE asset_id='a'")
            .bind(&key).execute(&state.db).await.unwrap();
        let duration = media::probe_duration(&root.join(&key), "video")
            .await
            .unwrap();
        assert_eq!(duration, 112_384);
        // This also exercises legacy-upload metadata enrichment and duration
        // validation before creating an immutable revision from the real file.
        ingest::save_parts_checked(
            &state,
            "a",
            "admin",
            serde_json::from_value(json!({
                "revisionId":"r", "parts":[{
                    "modality":"video", "text":"", "startSeconds":0, "endSeconds":112,
                }],
            }))
            .unwrap(),
        )
        .await
        .unwrap();
        let rev = latest_revision(&state.db, "a").await.unwrap();
        let progress = ReviewProgress {
            db: &state.db,
            revision: &rev,
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        let started = std::time::Instant::now();
        review_revision(&state, "a", &rev, "admin", &progress)
            .await
            .unwrap();
        let rows = sqlx::query("SELECT start_ms,end_ms,media_storage_key,metadata_json FROM kb_content_parts WHERE revision_id=? AND modality='video' ORDER BY start_ms")
            .bind(&rev).fetch_all(&state.db).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(
            (
                rows[0].get::<i64, _>("start_ms"),
                rows[0].get::<i64, _>("end_ms")
            ),
            (0, 60_000)
        );
        assert_eq!(
            (
                rows[1].get::<i64, _>("start_ms"),
                rows[1].get::<i64, _>("end_ms")
            ),
            (60_000, 112_000)
        );
        let asset = ingest::load_asset(&state.db, "a").await.unwrap();
        assert_eq!(asset.review_status, "approved");
        assert!(asset.published);
        let extraction = asset.extraction.unwrap();
        assert_eq!(extraction["reviewProgress"]["stage"], "saving_review");
        eprintln!("supplied video: source duration {duration} ms; {} prepared clips; review without OSS network took {:.3} s", rows.len(), started.elapsed().as_secs_f64());
        let mut entries = tokio::fs::read_dir(&root).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            tokio::fs::remove_file(entry.path()).await.unwrap();
        }
        tokio::fs::remove_dir(root).await.unwrap();
    }

    #[tokio::test]
    async fn idle_index_worker_releases_the_editing_lock() {
        let state = crate::db::test_state().await;
        kick_worker(state.clone());
        // Let the worker enter its idle sleep after inspecting the empty queue.
        tokio::time::sleep(Duration::from_millis(50)).await;
        let guard = tokio::time::timeout(Duration::from_millis(250), state.workflow_lock.lock())
            .await
            .expect("an idle index worker must not block fragment editing");
        drop(guard);
    }

    #[tokio::test]
    async fn review_progress_preserves_parser_metadata_and_recovers_after_restart() {
        let (state, _) = crate::db::test_published_state().await;
        sqlx::query("UPDATE kb_asset_revisions SET extraction_json=? WHERE revision_id='r'")
            .bind(
                json!({"sourceDurationMs":112384,"sourceName":"video.mp4","message":"需要校对"})
                    .to_string(),
            )
            .execute(&state.db)
            .await
            .unwrap();
        let progress = ReviewProgress {
            db: &state.db,
            revision: "r",
            started_at: chrono::Utc::now().to_rfc3339(),
        };
        progress
            .set("running", "preparing_video", "正在裁剪视频")
            .await
            .unwrap();
        let extraction = ingest::load_asset(&state.db, "a")
            .await
            .unwrap()
            .extraction
            .unwrap();
        assert_eq!(extraction["reviewProgress"]["state"], "running");
        assert_eq!(extraction["sourceDurationMs"], 112384);
        resume_jobs(state.clone()).await;
        let extraction = ingest::load_asset(&state.db, "a")
            .await
            .unwrap()
            .extraction
            .unwrap();
        assert_eq!(extraction["reviewProgress"]["state"], "failed");
        assert_eq!(extraction["reviewProgress"]["stage"], "interrupted");
        assert_eq!(extraction["sourceName"], "video.mp4");
        assert_eq!(extraction["sourceDurationMs"], 112384);
        assert_eq!(extraction["message"], "需要校对");
    }

    async fn published_fixture() -> (AppState, tokio::task::JoinHandle<()>) {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        for migration in [
            include_str!("../migrations/0001_initial.sql"),
            include_str!("../migrations/0003_knowledge_workflow.sql"),
            include_str!("../migrations/0004_index_requests.sql"),
            include_str!("../migrations/0008_index_progress.sql"),
        ] {
            sqlx::raw_sql(migration).execute(&db).await.unwrap();
        }
        sqlx::query(
            "ALTER TABLE kb_assets ADD COLUMN evidence_mode TEXT NOT NULL DEFAULT 'content'",
        )
        .execute(&db)
        .await
        .unwrap();
        let model = ModelConfig {
            kind: "embedding".into(),
            provider: "ark".into(),
            thinking: "disabled".into(),
            reasoning_effort: "high".into(),
            answer_verification_enabled: true,
            answer_provenance_enabled: true,
            answer_provenance_mode: "inline".into(),
            base_url: "http://unused.invalid".into(),
            model: "fixture".into(),
            dimension: 2,
            protocol: "responses".into(),
            vision_enabled: false,
            enabled: false,
            instructions: String::new(),
            fps: 1.0,
            api_key: None,
            configured: false,
        };
        let fingerprint = model.fingerprint();
        sqlx::query("INSERT INTO kb_runtime_configs(kind,config_json,updated_by) VALUES('embedding',?,'admin')").bind(serde_json::to_string(&model).unwrap()).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('leaf','course',2,'Leaf')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by) VALUES('asset','leaf','text','source','safe.txt',4,'admin')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_embedding_configs(embedding_config_id,provider,model,dimensions,config_fingerprint,created_by) VALUES('config','ark','fixture',2,?,'admin')").bind(&fingerprint).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_vector_collections(collection_id,course_id,embedding_config_id,generation,qdrant_collection,status) VALUES('collection','course','config',1,'fixture_collection','active')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,review_status,index_status,collection_id,expected_vector_count,created_by) VALUES('revision','asset',1,'hash','v1','published','approved','verified','collection',1,'admin')").execute(&db).await.unwrap();
        sqlx::query("UPDATE kb_assets SET published_revision_id='revision' WHERE asset_id='asset'")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,content_text,content_sha256,evidence_status) VALUES('part','revision',0,'text','reviewed source','content','approved')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_vector_records(vector_record_id,collection_id,revision_id,part_id,qdrant_point_id,modality,dimensions,status) VALUES('record','collection','revision','part','point','text',2,'verified')").execute(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO kb_active_indexes(course_id,collection_id) VALUES('course','collection')",
        )
        .execute(&db)
        .await
        .unwrap();
        let payload = json!({"course_id":"course","node_id":"leaf","asset_id":"asset","revision_id":"revision","part_id":"part","modality":"text","config_fingerprint":fingerprint,"content_sha256":"content"});
        let response = json!({"result":{"id":"point","vector":[1.0,0.0],"payload":payload}});
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/collections/fixture_collection/points/point",
            axum::routing::get(move || {
                let response = response.clone();
                async move { Json(response) }
            }),
        );
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = crate::config::AppConfig::test_config();
        config.storage = crate::config::StorageConfig::Local {
            local_dir: ingest::uploads_dir(),
        };
        config.qdrant.http_url = format!("http://{address}").parse().unwrap();
        let state = AppState::new(
            config.clone(),
            db,
            crate::qdrant::QdrantAdapter::new(config.qdrant.clone()).unwrap(),
            crate::auth::AuthUpstream::new(config.auth.clone()).unwrap(),
        );
        (state, task)
    }

    #[tokio::test]
    async fn withdrawn_verified_revision_can_publish_again_without_editing() {
        let (state, mock) = published_fixture().await;
        withdraw(&state, "asset", "admin").await.unwrap();
        let pointer: Option<String> = sqlx::query_scalar(
            "SELECT published_revision_id FROM kb_assets WHERE asset_id='asset'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert!(pointer.is_none());
        assert_eq!(
            revision(&state.db, "revision").await.unwrap().status,
            "withdrawn"
        );
        publish(&state, "asset", "admin").await.unwrap();
        let pointer: String = sqlx::query_scalar(
            "SELECT published_revision_id FROM kb_assets WHERE asset_id='asset'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(pointer, "revision");
        mock.abort();
    }

    #[tokio::test]
    async fn failed_reindex_keeps_verified_publication_available() {
        let (state, mock) = published_fixture().await;
        sqlx::query("INSERT INTO kb_jobs(job_id,job_type,asset_id,revision_id,state,attempts) VALUES('job','index_revision','asset','revision','queued',2)").execute(&state.db).await.unwrap();
        sqlx::query("INSERT INTO kb_index_requests(job_id,revision_id,config_fingerprint,actor_id) VALUES('job','revision','unused','admin')").execute(&state.db).await.unwrap();
        sqlx::query(
            "INSERT INTO kb_index_outbox(id,revision_id,state) VALUES('job','revision','pending')",
        )
        .execute(&state.db)
        .await
        .unwrap();
        drain_jobs(&state).await.unwrap();
        let current = revision(&state.db, "revision").await.unwrap();
        assert_eq!(current.status, "published");
        assert_eq!(current.index, "verified");
        assert_eq!(current.published.as_deref(), Some("revision"));
        let record: String = sqlx::query_scalar(
            "SELECT status FROM kb_vector_records WHERE vector_record_id='record'",
        )
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(record, "verified");
        let job: String = sqlx::query_scalar("SELECT state FROM kb_jobs WHERE job_id='job'")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(job, "failed");
        publish(&state, "asset", "admin").await.unwrap();
        mock.abort();
    }
    #[test]
    fn point_ids_are_replayable_and_content_specific() {
        assert_eq!(point_id("r", "p", "f", "h"), point_id("r", "p", "f", "h"));
        assert_ne!(
            point_id("r", "p", "f", "h"),
            point_id("r", "p", "f", "corrected")
        );
        assert_ne!(
            point_id("r", "p", "f", "h"),
            point_id("r", "p", "new-model", "h")
        );
        assert!(Uuid::parse_str(&point_id("r", "p", "f", "h")).is_ok());
    }
    #[tokio::test]
    async fn model_switch_requires_whole_course_rebuild() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0003_knowledge_workflow.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0004_index_requests.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_embedding_configs(embedding_config_id,provider,model,dimensions,config_fingerprint,created_by) VALUES('old','ark','vision',1024,'old-fingerprint','admin')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_vector_collections(collection_id,course_id,embedding_config_id,generation,qdrant_collection,status) VALUES('collection','course','old',1,'safe_collection','active')").execute(&db).await.unwrap();
        sqlx::query(
            "INSERT INTO kb_active_indexes(course_id,collection_id) VALUES('course','collection')",
        )
        .execute(&db)
        .await
        .unwrap();
        assert!(check_course_space(&db, "course", "old-fingerprint")
            .await
            .is_ok());
        assert!(check_course_space(&db, "course", "new-fingerprint")
            .await
            .is_err());
        let active: String = sqlx::query_scalar(
            "SELECT collection_id FROM kb_active_indexes WHERE course_id='course'",
        )
        .fetch_one(&db)
        .await
        .unwrap();
        assert_eq!(active, "collection");
    }

    #[tokio::test]
    async fn fts_follows_insert_correction_and_delete() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0003_knowledge_workflow.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0004_index_requests.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('leaf','course',2,'Leaf')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by) VALUES('a','leaf','text','a.txt','a.txt',1,'admin')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by) VALUES('r','a',1,'hash','v1','review','admin')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,content_text,content_sha256) VALUES('p','r',0,'text','originalword','h')").execute(&db).await.unwrap();
        let count:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_content_parts_fts WHERE kb_content_parts_fts MATCH 'originalword'").fetch_one(&db).await.unwrap();
        assert_eq!(count, 1);
        sqlx::query("UPDATE kb_content_parts SET content_text='correctedword' WHERE part_id='p'")
            .execute(&db)
            .await
            .unwrap();
        let old:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_content_parts_fts WHERE kb_content_parts_fts MATCH 'originalword'").fetch_one(&db).await.unwrap();
        assert_eq!(old, 0);
        let new:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_content_parts_fts WHERE kb_content_parts_fts MATCH 'correctedword'").fetch_one(&db).await.unwrap();
        assert_eq!(new, 1);
        sqlx::query("DELETE FROM kb_content_parts WHERE part_id='p'")
            .execute(&db)
            .await
            .unwrap();
        let deleted:i64=sqlx::query_scalar("SELECT COUNT(*) FROM kb_content_parts_fts WHERE kb_content_parts_fts MATCH 'correctedword'").fetch_one(&db).await.unwrap();
        assert_eq!(deleted, 0);
    }

    #[tokio::test]
    async fn cleanup_preserves_keys_referenced_by_another_asset() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::raw_sql(include_str!("../migrations/0001_initial.sql"))
            .execute(&db)
            .await
            .unwrap();
        sqlx::raw_sql(include_str!("../migrations/0003_knowledge_workflow.sql"))
            .execute(&db)
            .await
            .unwrap();
        let directory = ingest::uploads_dir();
        tokio::fs::create_dir_all(&directory).await.unwrap();
        let shared = format!("{}.txt", Uuid::new_v4());
        let frame = format!("{}.jpg", Uuid::new_v4());
        let other = format!("{}.txt", Uuid::new_v4());
        tokio::fs::write(directory.join(&shared), b"shared content")
            .await
            .unwrap();
        tokio::fs::write(directory.join(&frame), b"derivative")
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('leaf','course',2,'Leaf')").execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by,deleted_at) VALUES('deleted','leaf','text','source',?,1,'admin',CURRENT_TIMESTAMP)").bind(&shared).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by) VALUES('live','leaf','text','source2',?,1,'admin')").bind(&other).execute(&db).await.unwrap();
        for (rev, asset) in [("r1", "deleted"), ("r2", "live")] {
            sqlx::query("INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by) VALUES(?,?,1,'h','v1','review','admin')").bind(rev).bind(asset).execute(&db).await.unwrap();
        }
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,media_storage_key,metadata_json,content_sha256) VALUES('p1','r1',0,'image',?,?,'h')").bind(&shared).bind(json!({"frameStorageKey":frame}).to_string()).execute(&db).await.unwrap();
        sqlx::query("INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,media_storage_key,content_sha256) VALUES('p2','r2',0,'image',?,'h')").bind(&shared).execute(&db).await.unwrap();
        let mut config = crate::config::AppConfig::test_config();
        config.storage = crate::config::StorageConfig::Local {
            local_dir: ingest::uploads_dir(),
        };
        let state = AppState::new(
            config.clone(),
            db,
            crate::qdrant::QdrantAdapter::new(config.qdrant.clone()).unwrap(),
            crate::auth::AuthUpstream::new(config.auth.clone()).unwrap(),
        );
        cleanup_asset(&state, "deleted").await.unwrap();
        assert!(directory.join(&shared).exists());
        assert!(!directory.join(&frame).exists());
        cleanup_asset(&state, "deleted").await.unwrap();
        tokio::fs::remove_file(directory.join(&shared))
            .await
            .unwrap();
    }
}
