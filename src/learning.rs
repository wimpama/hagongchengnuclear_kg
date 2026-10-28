//! Administrative learning corpus. Student dialogue is observational data,
//! never a published scientific source. All operations use the tenant's DB.
use crate::{embedding, error::ApiError, models, state::AppState};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};
use std::{collections::HashSet, time::Duration};

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScopeInput {
    pub scope_type: String,
    #[serde(default)]
    pub scope_id: String,
}

#[derive(Clone)]
pub struct LearningScope {
    pub input: ScopeInput,
    pub title: String,
    pub courses: Vec<String>,
    pub nodes: Vec<String>,
}

impl LearningScope {
    // The filter is applied again in SQLite after vector retrieval. Qdrant
    // payloads never authorize a student identity or a teaching scope.
    pub fn filter_sql(&self, alias: &str) -> String {
        let courses = json!(self.courses).to_string().replace('\'', "''");
        let nodes = json!(self.nodes).to_string().replace('\'', "''");
        format!(
            "{alias}.course_id IN (SELECT value FROM json_each('{courses}')){}",
            if self.input.scope_type == "node" {
                format!(" AND {alias}.scope_type IN ('level1','level2') AND {alias}.scope_id IN (SELECT value FROM json_each('{nodes}'))")
            } else {
                String::new()
            }
        )
    }
    pub fn vector_filter(&self) -> Value {
        let mut must = vec![json!({"key":"course_id","match":{"any":self.courses}})];
        if self.input.scope_type == "node" {
            must.push(json!({"key":"node_id","match":{"any":self.nodes}}));
        }
        json!({"must":must})
    }
}

pub async fn resolve(db: &SqlitePool, input: ScopeInput) -> Result<LearningScope, ApiError> {
    if input.scope_id.len() > 160 {
        return Err(ApiError::bad_request("范围 ID 无效"));
    }
    let mut nodes = Vec::new();
    let (title, courses) = match input.scope_type.as_str() {
        "all" => (
            "全部课程".to_owned(),
            sqlx::query_scalar("SELECT course_id FROM kb_courses WHERE is_active=1")
                .fetch_all(db)
                .await?,
        ),
        "course" => {
            let title: String = sqlx::query_scalar(
                "SELECT title FROM kb_courses WHERE course_id=? AND is_active=1",
            )
            .bind(&input.scope_id)
            .fetch_optional(db)
            .await?
            .ok_or_else(|| ApiError::not_found("课程未同步或已失效"))?;
            (title, vec![input.scope_id.clone()])
        }
        "node" => {
            let row = sqlx::query("SELECT n.title,n.course_id,n.level FROM kb_course_nodes n JOIN kb_courses c USING(course_id) WHERE n.node_id=? AND n.is_active=1 AND c.is_active=1")
                .bind(&input.scope_id).fetch_optional(db).await?.ok_or_else(|| ApiError::not_found("节点未同步或已失效"))?;
            nodes.push(input.scope_id.clone());
            if row.get::<i64, _>("level") == 1 {
                nodes.extend(sqlx::query_scalar::<_,String>("SELECT node_id FROM kb_course_nodes WHERE parent_node_id=? AND is_active=1")
                    .bind(&input.scope_id).fetch_all(db).await?);
            }
            (row.get("title"), vec![row.get("course_id")])
        }
        _ => return Err(ApiError::bad_request("不支持的教学分析范围")),
    };
    Ok(LearningScope {
        input,
        title,
        courses,
        nodes,
    })
}

pub fn start(state: AppState) {
    tokio::spawn(async move {
        loop {
            let succeeded = match synchronize(&state).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(message=%error.message, "student learning index update failed");
                    let _ =
                        sqlx::query("UPDATE kb_learning_state SET last_error=? WHERE singleton=1")
                            .bind(error.message)
                            .execute(&state.db)
                            .await;
                    false
                }
            };
            let more = if succeeded {
                if let Ok(config) = models::load(&state, "embedding").await {
                    config.enabled && config.api_key.is_some() && sqlx::query_scalar::<_,i64>(&format!("SELECT EXISTS(SELECT 1 FROM kb_learning_turns t WHERE {ALLOWED_VECTOR} AND {})", pending_vector_sql()))
                        .bind(config.fingerprint()).fetch_one(&state.db).await.unwrap_or(0) != 0
                } else {
                    false
                }
            } else {
                false
            };
            tokio::time::sleep(Duration::from_secs(if more { 2 } else { 60 })).await;
        }
    });
}

fn decode(raw: String) -> String {
    serde_json::from_str(&raw).unwrap_or(raw)
}

pub async fn confirm_role(
    state: &AppState,
    user: &crate::auth::AuthenticatedUser,
) -> Result<(), ApiError> {
    if user.role == crate::auth::UserRole::Student {
        if let Some(profile) = &user.profile {
            crate::student_profiles::store(&state.db, &user.id, profile).await?;
        }
    }
    let role = if user.role == crate::auth::UserRole::Student {
        "student"
    } else {
        "admin"
    };
    sqlx::query(
        "UPDATE kb_conversations SET creator_role=? WHERE user_id=? AND creator_role='unknown'",
    )
    .bind(role)
    .bind(&user.id)
    .execute(&state.db)
    .await?;
    Ok(())
}

// A reset applies to the exact scope and any ancestor scope that includes it.
const ALLOWED_DATA: &str = "NOT EXISTS(SELECT 1 FROM kb_learning_resets z WHERE seq.seq<=z.data_cutoff AND (z.scope_type='all' OR (z.scope_type='course' AND z.scope_id=c.course_id) OR (z.scope_type='node' AND (z.scope_id=c.scope_id OR z.scope_id=(SELECT parent_node_id FROM kb_course_nodes WHERE node_id=c.scope_id)))))";
const ALLOWED_VECTOR: &str = "NOT EXISTS(SELECT 1 FROM kb_learning_resets z WHERE t.source_rowid<=z.vector_cutoff AND (z.scope_type='all' OR (z.scope_type='course' AND z.scope_id=t.course_id) OR (z.scope_type='node' AND (z.scope_id=t.scope_id OR z.scope_id=(SELECT parent_node_id FROM kb_course_nodes WHERE node_id=t.scope_id)))))";

// Attachment contents are immutable. The ordered set of live IDs changes on
// expiry/deletion, causing the same vector point to be replaced without them.
const MEDIA_KEY_SQL: &str = "COALESCE((SELECT group_concat(value,',') FROM json_each(t.attachments_json) WHERE EXISTS(SELECT 1 FROM kb_question_attachments qa WHERE qa.attachment_id=value AND qa.conversation_id=t.conversation_id AND qa.user_id=t.user_id AND qa.expires_at>CURRENT_TIMESTAMP)),'')";
fn pending_vector_sql() -> String {
    format!("NOT EXISTS(SELECT 1 FROM kb_learning_vectors v WHERE v.turn_id=t.turn_id AND v.fingerprint=? AND v.media_key={MEDIA_KEY_SQL})")
}

pub async fn capture(db: &SqlitePool) -> Result<(), ApiError> {
    // No source cursor is advanced before every selected pair is committed.
    // Failed/in-flight questions have no assistant and aren't counted.
    let rows = sqlx::query(&format!("SELECT q.message_id,seq.seq AS source_rowid,q.content_json AS question,q.attachments_json,a.content_json AS answer,a.citations_json,q.created_at AS turn_at,c.* FROM kb_messages q JOIN kb_learning_source_seq seq ON seq.turn_id=q.message_id JOIN kb_conversations c USING(conversation_id) JOIN kb_messages a ON a.conversation_id=q.conversation_id AND a.client_message_id=q.client_message_id AND a.role='assistant' WHERE q.role='user' AND c.deleted_at IS NULL AND c.creator_role='student' AND {ALLOWED_DATA} AND NOT EXISTS(SELECT 1 FROM kb_learning_turns t WHERE t.turn_id=q.message_id) ORDER BY seq.seq LIMIT 1000"))
        .fetch_all(db).await?;
    let mut tx = db.begin().await?;
    sqlx::query("DELETE FROM kb_learning_turns WHERE NOT EXISTS(SELECT 1 FROM kb_conversations c WHERE c.conversation_id=kb_learning_turns.conversation_id AND c.deleted_at IS NULL AND c.creator_role='student')")
        .execute(&mut *tx).await?;
    // Backfill already synchronized discussions on upgrade, without reviving
    // records deliberately cleared by a teacher.
    sqlx::query("UPDATE kb_learning_turns SET attachments_json=(SELECT q.attachments_json FROM kb_messages q WHERE q.message_id=kb_learning_turns.turn_id) WHERE EXISTS(SELECT 1 FROM kb_messages q WHERE q.message_id=kb_learning_turns.turn_id AND q.attachments_json!=kb_learning_turns.attachments_json)")
        .execute(&mut *tx).await?;
    for row in rows {
        let question = decode(row.get("question"));
        let answer = decode(row.get("answer"));
        let citations: String = row
            .get::<Option<String>, _>("citations_json")
            .unwrap_or_else(|| "[]".into());
        let attachments: String = row.get("attachments_json");
        let bytes = question.len() + answer.len() + citations.len() + attachments.len();
        sqlx::query("INSERT OR IGNORE INTO kb_learning_turns(turn_id,source_rowid,conversation_id,user_id,course_id,scope_type,scope_id,question,answer,citations_json,created_at,byte_size,attachments_json) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(row.get::<String,_>("message_id")).bind(row.get::<i64,_>("source_rowid")).bind(row.get::<String,_>("conversation_id"))
            .bind(row.get::<String,_>("user_id")).bind(row.get::<String,_>("course_id")).bind(row.get::<String,_>("scope_type"))
            .bind(row.get::<String,_>("scope_id")).bind(question).bind(answer).bind(citations).bind(row.get::<String,_>("turn_at"))
            .bind(bytes as i64).bind(attachments).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE kb_learning_state SET synced_at=CURRENT_TIMESTAMP WHERE singleton=1")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn synchronize(state: &AppState) -> Result<(), ApiError> {
    let Ok(_guard) = state.learning_lock.try_lock() else {
        return Ok(());
    };
    capture(&state.db).await?;
    // Student-deleted histories must disappear even from retained vectors.
    let orphaned = sqlx::query("SELECT v.turn_id,v.fingerprint,l.collection_name FROM kb_learning_vectors v JOIN kb_learning_collections l USING(fingerprint) WHERE NOT EXISTS(SELECT 1 FROM kb_conversations c WHERE c.conversation_id=v.conversation_id AND c.deleted_at IS NULL AND c.creator_role='student') LIMIT 200")
        .fetch_all(&state.db).await?;
    for row in orphaned {
        let id: String = row.get("turn_id");
        let collection: String = row.get("collection_name");
        state
            .qdrant
            .delete_points(&collection, std::slice::from_ref(&id))
            .await?;
        sqlx::query("DELETE FROM kb_learning_vectors WHERE turn_id=? AND fingerprint=?")
            .bind(id)
            .bind(row.get::<String, _>("fingerprint"))
            .execute(&state.db)
            .await?;
    }
    let config = models::load(state, "embedding").await?;
    if !config.enabled || config.api_key.is_none() {
        return Ok(());
    }
    let fingerprint = config.fingerprint();
    let pending = sqlx::query(&format!("SELECT t.*,{MEDIA_KEY_SQL} AS media_key FROM kb_learning_turns t WHERE {ALLOWED_VECTOR} AND {} ORDER BY t.source_rowid LIMIT 16", pending_vector_sql()))
        .bind(&fingerprint).fetch_all(&state.db).await?;
    if pending.is_empty() {
        return Ok(());
    }
    let collection = format!(
        "learning_{}_{}",
        state.environment.name(),
        &fingerprint[..16]
    );
    state
        .qdrant
        .ensure_collection(&collection, config.dimension)
        .await?;
    sqlx::query("INSERT OR IGNORE INTO kb_learning_collections(fingerprint,collection_name,dimensions) VALUES(?,?,?)")
        .bind(&fingerprint).bind(&collection).bind(config.dimension as i64).execute(&state.db).await?;
    for row in pending {
        let id: String = row.get("turn_id");
        let question: String = row.get("question");
        let answer: String = row.get("answer");
        let media_key: String = row.get("media_key");
        let ids: Vec<String> =
            serde_json::from_str(&row.get::<String, _>("attachments_json")).unwrap_or_default();
        // Keep the question primary; old assistant text supplies discussion
        // context, not an authoritative scientific definition.
        let input: String = format!("学生提问：{question}\n当时的助教回复（历史记录）：{answer}")
            .chars()
            .take(6000)
            .collect();
        let inputs = learning_inputs(
            state,
            &row.get::<String, _>("conversation_id"),
            &row.get::<String, _>("user_id"),
            &ids,
            &input,
        )
        .await?;
        let vector = embedding::embed(&config, inputs).await?;
        let still_current: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM kb_learning_turns t JOIN kb_conversations c USING(conversation_id) WHERE t.turn_id=? AND c.deleted_at IS NULL AND c.creator_role='student' AND {ALLOWED_VECTOR} AND {MEDIA_KEY_SQL}=?"))
            .bind(&id).bind(&media_key).fetch_one(&state.db).await?;
        if still_current == 0 {
            continue;
        }
        state.qdrant.upsert(&collection, json!({"id":id,"vector":vector,"payload":{
            "course_id":row.get::<String,_>("course_id"),"node_id":row.get::<String,_>("scope_id"),"turn_id":id,"corpus":"student_learning"}})).await?;
        sqlx::query("INSERT OR REPLACE INTO kb_learning_vectors(turn_id,fingerprint,source_rowid,conversation_id,course_id,scope_type,scope_id,vector_bytes,media_key) VALUES(?,?,?,?,?,?,?,?,?)")
            .bind(&id).bind(&fingerprint).bind(row.get::<i64,_>("source_rowid")).bind(row.get::<String,_>("conversation_id"))
            .bind(row.get::<String,_>("course_id")).bind(row.get::<String,_>("scope_type")).bind(row.get::<String,_>("scope_id"))
            .bind((config.dimension * 4) as i64).bind(media_key).execute(&state.db).await?;
        sqlx::query(
            "UPDATE kb_learning_turns SET indexed_fingerprint=?,vector_bytes=? WHERE turn_id=?",
        )
        .bind(&fingerprint)
        .bind((config.dimension * 4) as i64)
        .bind(id)
        .execute(&state.db)
        .await?;
    }
    sqlx::query("UPDATE kb_learning_state SET last_error=NULL WHERE singleton=1")
        .execute(&state.db)
        .await?;
    Ok(())
}

pub async fn status(state: &AppState, scope: &LearningScope) -> Result<Value, ApiError> {
    let filter = scope.filter_sql("t");
    let row = sqlx::query(&format!("SELECT COUNT(*) AS turns,COUNT(DISTINCT user_id) AS students,COUNT(DISTINCT conversation_id) AS conversations,COALESCE(SUM(byte_size),0) AS bytes,MAX(created_at) AS latest FROM kb_learning_turns t WHERE {filter}"))
        .fetch_one(&state.db).await?;
    let vectors = sqlx::query(&format!("SELECT COUNT(*) AS count,COALESCE(SUM(vector_bytes),0) AS bytes FROM kb_learning_vectors t WHERE {filter}"))
        .fetch_one(&state.db).await?;
    let sync = sqlx::query("SELECT synced_at,last_error FROM kb_learning_state WHERE singleton=1")
        .fetch_one(&state.db)
        .await?;
    let fingerprint = models::load(state, "embedding").await?.fingerprint();
    let pending: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM kb_learning_turns t WHERE {filter} AND {ALLOWED_VECTOR} AND {}",
        pending_vector_sql()
    ))
    .bind(fingerprint)
    .fetch_one(&state.db)
    .await?;
    let unclassified: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM kb_conversations t WHERE {filter} AND t.deleted_at IS NULL AND t.creator_role='unknown'"))
        .fetch_one(&state.db).await?;
    Ok(
        json!({"scopeTitle":scope.title,"turnCount":row.get::<i64,_>("turns"),"studentCount":row.get::<i64,_>("students"),"conversationCount":row.get::<i64,_>("conversations"),
        "dataBytes":row.get::<i64,_>("bytes"),"vectorCount":vectors.get::<i64,_>("count"),"vectorBytes":vectors.get::<i64,_>("bytes"),
        "latestRecordAt":row.get::<Option<String>,_>("latest"),"syncedAt":sync.get::<Option<String>,_>("synced_at"),"lastError":sync.get::<Option<String>,_>("last_error"),"pendingTurns":pending,
        "unclassifiedConversationCount":unclassified,
        "sizeNote":"会话体量为问答正文与引用的 UTF-8 字节；向量体量按维度×4字节统计，不含数据库索引与元数据开销。"}),
    )
}

pub async fn clear(state: &AppState, scope: &LearningScope, kind: &str) -> Result<(), ApiError> {
    if !matches!(kind, "data" | "vectors") {
        return Err(ApiError::bad_request("清理类型无效"));
    }
    let _guard = state.learning_lock.lock().await;
    let filter = scope.filter_sql("t");
    let cutoff: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq),0) FROM kb_learning_source_seq")
        .fetch_one(&state.db)
        .await?;
    // Vector cleanup is awaited and retryable. Rows aren't marked deleted
    // unless Qdrant accepted deletion, so failed clears never lie about size.
    if kind == "vectors" {
        let collections =
            sqlx::query("SELECT fingerprint,collection_name FROM kb_learning_collections")
                .fetch_all(&state.db)
                .await?;
        for collection in collections {
            let fingerprint: String = collection.get("fingerprint");
            let ids: Vec<String> = sqlx::query_scalar(&format!(
                "SELECT turn_id FROM kb_learning_vectors t WHERE {filter} AND fingerprint=?"
            ))
            .bind(&fingerprint)
            .fetch_all(&state.db)
            .await?;
            for chunk in ids.chunks(100) {
                state
                    .qdrant
                    .delete_points(&collection.get::<String, _>("collection_name"), chunk)
                    .await?;
            }
        }
    }
    let mut tx = state.db.begin().await?;
    let column = if kind == "data" {
        "data_cutoff"
    } else {
        "vector_cutoff"
    };
    sqlx::query(&format!("INSERT INTO kb_learning_resets(scope_type,scope_id,{column}) VALUES(?,?,?) ON CONFLICT(scope_type,scope_id) DO UPDATE SET {column}=MAX({column},excluded.{column})"))
        .bind(&scope.input.scope_type).bind(&scope.input.scope_id).bind(cutoff).execute(&mut *tx).await?;
    if kind == "data" {
        sqlx::query(&format!(
            "DELETE FROM kb_learning_turns AS t WHERE {filter}"
        ))
        .execute(&mut *tx)
        .await?;
    } else {
        sqlx::query(&format!(
            "DELETE FROM kb_learning_vectors AS t WHERE {filter}"
        ))
        .execute(&mut *tx)
        .await?;
        sqlx::query(&format!("UPDATE kb_learning_turns AS t SET indexed_fingerprint=NULL,vector_bytes=0 WHERE {filter}")).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub node_title: String,
    pub question: String,
    pub answer: String,
    pub created_at: String,
    pub citations: Value,
    #[serde(default)]
    pub attachments: Vec<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub student: Option<crate::student_profiles::StudentProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub record_number: Option<usize>,
}
fn turn(row: sqlx::sqlite::SqliteRow) -> Turn {
    Turn {
        id: row.get("turn_id"),
        user_id: row.get("user_id"),
        conversation_id: row.get("conversation_id"),
        node_title: row
            .get::<Option<String>, _>("node_title")
            .unwrap_or_default(),
        question: row.get("question"),
        answer: row.get("answer"),
        created_at: row.get("created_at"),
        citations: serde_json::from_str(&row.get::<String, _>("citations_json"))
            .unwrap_or(json!([])),
        attachments: serde_json::from_str::<Vec<String>>(&row.get::<String, _>("attachments_json"))
            .unwrap_or_default()
            .into_iter()
            .map(|id| json!({"id":id}))
            .collect(),
        student: None,
        record_number: None,
    }
}
const TURN_SELECT: &str = "SELECT t.*,COALESCE(n.title,c.title) AS node_title FROM kb_learning_turns t JOIN kb_conversations source ON source.conversation_id=t.conversation_id AND source.deleted_at IS NULL LEFT JOIN kb_course_nodes n ON n.node_id=t.scope_id AND t.scope_type!='course' LEFT JOIN kb_courses c ON c.course_id=t.course_id";

pub async fn hydrate_attachments(state: &AppState, item: &mut Turn) -> Result<(), ApiError> {
    let ids = item
        .attachments
        .iter()
        .filter_map(|a| a["id"].as_str().map(str::to_owned))
        .collect::<Vec<_>>();
    item.attachments =
        crate::question_attachments::views(state, &item.conversation_id, &item.user_id, &ids)
            .await?;
    Ok(())
}

pub async fn get_turn(state: &AppState, scope: &LearningScope, id: &str) -> Result<Turn, ApiError> {
    let row = sqlx::query(&format!(
        "{TURN_SELECT} WHERE {} AND t.turn_id=? AND source.creator_role='student'",
        scope.filter_sql("t")
    ))
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or_else(|| ApiError::not_found("问答记录已清理或不在当前范围"))?;
    let mut item = turn(row);
    hydrate_attachments(state, &mut item).await?;
    Ok(item)
}

async fn learning_inputs(
    state: &AppState,
    conversation: &str,
    user: &str,
    ids: &[String],
    text: &str,
) -> Result<Vec<Value>, ApiError> {
    let mut inputs = vec![json!({"type":"text","text":text})];
    for attachment in crate::question_attachments::views(state, conversation, user, ids).await? {
        if attachment["available"] != true {
            continue;
        }
        let id = attachment["id"].as_str().unwrap_or_default();
        if attachment["mimeType"]
            .as_str()
            .is_some_and(|mime| mime.starts_with("image/"))
        {
            let key: Option<String> = sqlx::query_scalar("SELECT storage_key FROM kb_question_attachments WHERE attachment_id=? AND conversation_id=? AND user_id=? AND expires_at>CURRENT_TIMESTAMP")
                .bind(id).bind(conversation).bind(user).fetch_optional(&state.db).await?;
            if let Some(key) = key {
                state.storage.ensure_local(&key).await?;
                inputs.push(
                    embedding::media_input(state.storage.local_dir(), &key, "image", 1.).await?,
                );
            }
        } else if let Some(text) = attachment["textPreview"].as_str().filter(|s| !s.is_empty()) {
            inputs.push(json!({"type":"text","text":format!("学生提问文档（非教师发布知识）：{}\n{}",attachment["name"].as_str().unwrap_or_default(),crate::question_attachments::bounded_text(text, 6000))}));
        }
    }
    Ok(inputs)
}

pub async fn teacher_images(
    state: &AppState,
    sources: &mut [Turn],
    vision: bool,
) -> Result<Vec<(String, String)>, ApiError> {
    let mut images = Vec::new();
    for (index, source) in sources.iter_mut().enumerate() {
        for attachment in &mut source.attachments {
            if attachment["available"] != true
                || !attachment["mimeType"]
                    .as_str()
                    .is_some_and(|mime| mime.starts_with("image/"))
            {
                continue;
            }
            attachment["imageProvidedToModel"] = json!(false);
            if !vision || images.len() >= 6 {
                continue;
            }
            let Some(id) = attachment["id"].as_str() else {
                continue;
            };
            let material = crate::question_attachments::material(
                state,
                &source.conversation_id,
                &[id.to_owned()],
                true,
            )
            .await;
            if let Ok(material) = material {
                for (_, url) in material.images {
                    images.push((
                        format!(
                            "question_attachment:record{}:{id}",
                            source.record_number.unwrap_or(index + 1)
                        ),
                        url,
                    ));
                }
                attachment["imageProvidedToModel"] = json!(true);
            }
        }
    }
    Ok(images)
}

fn question_key(question: &str) -> String {
    // Preserve mathematical operators; only normalize presentation differences.
    question
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(['?', '？', '！', '.', '。', ';', '；'])
        .trim()
        .to_lowercase()
}

struct QuestionGroup {
    question: String,
    variants: Vec<String>,
    count: i64,
    users: HashSet<String>,
}

async fn question_groups(db: &SqlitePool, filter: &str) -> Result<Value, ApiError> {
    let rows = sqlx::query(&format!("SELECT question,COUNT(*) AS count,json_group_array(DISTINCT user_id) AS users FROM kb_learning_turns t WHERE {filter} GROUP BY question ORDER BY count DESC,MAX(source_rowid) DESC LIMIT 300"))
        .fetch_all(db).await?;
    let mut groups = Vec::<QuestionGroup>::new();
    let mut positions = std::collections::HashMap::<String, usize>::new();
    for row in rows {
        let question: String = row.get("question");
        let key = question_key(&question);
        let index = *positions.entry(key).or_insert_with(|| {
            groups.push(QuestionGroup {
                question: question.clone(),
                variants: Vec::new(),
                count: 0,
                users: HashSet::new(),
            });
            groups.len() - 1
        });
        let group = &mut groups[index];
        group.count += row.get::<i64, _>("count");
        if group.variants.len() < 3 {
            group.variants.push(question);
        }
        let users: Vec<String> =
            serde_json::from_str(&row.get::<String, _>("users")).unwrap_or_default();
        group.users.extend(users);
    }
    groups.sort_by_key(|group| std::cmp::Reverse(group.count));
    let mut output = Vec::new();
    let mut bytes = 0;
    let mut covered = 0;
    for group in groups {
        let data = json!({"question":group.question,"variants":group.variants,"count":group.count,"studentCount":group.users.len(),"userIds":group.users});
        let size = data.to_string().len();
        if bytes + size > 24_000 {
            continue;
        }
        bytes += size;
        covered += group.count;
        output.push(data);
    }
    Ok(
        json!({"groups":output,"coveredQuestionCount":covered,"normalization":"仅合并空白及句尾标点差异；语义相似主题可继续归纳，不能据此声称学生不会。学生人数按userIds去重。"}),
    )
}

pub async fn retrieve(
    state: &AppState,
    scope: &LearningScope,
    query: &str,
) -> Result<(Value, Vec<Turn>), ApiError> {
    let filter = scope.filter_sql("t");
    let stats = status(state, scope).await?;
    let groups = sqlx::query(&format!("SELECT t.scope_type,t.scope_id,COALESCE(n.title,c.title) AS title,COUNT(*) AS count,COUNT(DISTINCT t.user_id) AS students FROM kb_learning_turns t LEFT JOIN kb_course_nodes n ON n.node_id=t.scope_id AND t.scope_type!='course' LEFT JOIN kb_courses c ON c.course_id=t.course_id WHERE {filter} GROUP BY t.scope_type,t.scope_id ORDER BY count DESC LIMIT 30"))
        .fetch_all(&state.db).await?;
    let groups: Vec<Value> = groups.iter().map(|r| json!({"scopeType":r.get::<String,_>("scope_type"),"scopeId":r.get::<String,_>("scope_id"),"title":r.get::<Option<String>,_>("title"),"questionCount":r.get::<i64,_>("count"),"studentCount":r.get::<i64,_>("students")})).collect();
    let questions = question_groups(&state.db, &filter).await?;
    let mut windows = Vec::new();
    for (label, from, to) in [
        ("最近7天", "-7 days", "0 days"),
        ("此前7天", "-14 days", "-7 days"),
    ] {
        let row = sqlx::query(&format!("SELECT COUNT(*) AS count,COUNT(DISTINCT user_id) AS students,COUNT(DISTINCT conversation_id) AS conversations FROM kb_learning_turns t WHERE {filter} AND datetime(created_at)>=datetime('now',?) AND datetime(created_at)<datetime('now',?)"))
            .bind(from).bind(to).fetch_one(&state.db).await?;
        windows.push(json!({"label":label,"questionCount":row.get::<i64,_>("count"),"studentCount":row.get::<i64,_>("students"),"conversationCount":row.get::<i64,_>("conversations")}));
    }
    let days = sqlx::query(&format!("SELECT date(created_at) AS day,COUNT(*) AS count,COUNT(DISTINCT user_id) AS students FROM kb_learning_turns t WHERE {filter} AND datetime(created_at)>=datetime('now','-14 days') GROUP BY date(created_at) ORDER BY day"))
        .fetch_all(&state.db).await?;
    let daily: Vec<_> = days.iter().map(|row| json!({"dayUtc":row.get::<String,_>("day"),"questionCount":row.get::<i64,_>("count"),"studentCount":row.get::<i64,_>("students")})).collect();
    let mut ids = Vec::new();
    let mut vector_warning = None;
    let config = models::load(state, "embedding").await?;
    let collection: Option<String> = sqlx::query_scalar(
        "SELECT collection_name FROM kb_learning_collections WHERE fingerprint=?",
    )
    .bind(config.fingerprint())
    .fetch_optional(&state.db)
    .await?;
    if let Some(collection) = collection {
        let result = async {
            let vector =
                embedding::embed(&config, vec![json!({"type":"text","text":query})]).await?;
            state
                .qdrant
                .learning_query(&collection, vector, scope.vector_filter())
                .await
        }
        .await;
        match result {
            Ok(hits) => {
                for hit in hits {
                    if let Some(id) = hit.pointer("/payload/turn_id").and_then(Value::as_str) {
                        ids.push(id.to_owned());
                    }
                }
            }
            Err(error) => vector_warning = Some(error.message),
        }
    }
    // Recent examples and literal matches remain usable while the worker
    // catches up or when an administrator cleared the vector corpus.
    let rows = sqlx::query(&format!(
        "{TURN_SELECT} WHERE {filter} ORDER BY t.source_rowid DESC LIMIT 160"
    ))
    .fetch_all(&state.db)
    .await?;
    let query_terms = crate::lexical::tokens(query);
    let mut samples: Vec<_> = rows.into_iter().map(turn).collect();
    samples.sort_by_cached_key(|t| {
        std::cmp::Reverse(
            crate::lexical::tokens(&t.question)
                .intersection(&query_terms)
                .count(),
        )
    });
    let vector_matched = ids.len();
    for sample in &samples {
        if !ids.contains(&sample.id) {
            ids.push(sample.id.clone());
        }
    }
    let mut result = Vec::new();
    let mut bytes = 0;
    for id in ids.into_iter().take(100) {
        let row = sqlx::query(&format!("{TURN_SELECT} WHERE {filter} AND t.turn_id=?"))
            .bind(id)
            .fetch_optional(&state.db)
            .await?;
        if let Some(row) = row {
            let mut item = turn(row);
            hydrate_attachments(state, &mut item).await?;
            let item_size = item.question.len()
                + item.answer.len()
                + item.citations.to_string().len()
                + json!(item.attachments).to_string().len();
            if bytes + item_size > 50_000 {
                continue;
            }
            bytes += item_size;
            result.push(item);
            if result.len() == 40 {
                break;
            }
        }
    }
    let sample_users: HashSet<_> = result.iter().map(|t| t.user_id.clone()).collect();
    Ok((
        json!({"totals":stats,"byConversationScope":groups,"questionGroups":questions,
        "referenceTimeUtc":chrono::Utc::now().to_rfc3339(),"timeWindows":windows,"dailyActivity":daily,
        "sampleQuestionCount":result.len(),"sampleStudentCount":sample_users.len(),"sampleUserIds":sample_users,
        "vectorCandidates":vector_matched,"vectorWarning":vector_warning,
        "countBoundary":"总数、时间窗口与按对话范围分组为当前已同步数据的精确统计；问题组仅覆盖coveredQuestionCount轮，语义主题合并须说明覆盖范围并按userIds去重。学生基本信息与具体对话内容仅覆盖samples，未返回记录不能猜测。最近无关问题只能按实际可见样本作答，不以未命中检索证明全量没有。提问次数不能证明学生未掌握。课程范围会话不自动归属其子章节。"}),
        result,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    async fn fixture() -> AppState {
        let state = crate::db::test_state().await;
        sqlx::raw_sql("INSERT INTO kb_courses(course_id,title) VALUES('c1','课程1'),('c2','课程2');
            INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('root','c1',1,'第一章'),('root2','c2',1,'外部章节');
            INSERT INTO kb_course_nodes(node_id,course_id,parent_node_id,level,title) VALUES('leaf','c1','root',2,'核反应截面'),('sibling','c1','root',2,'能量'),('outside','c2','root2',2,'外部');
            INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id,creator_role) VALUES
              ('a','student1','c1','level2','leaf','student'),('b','student2','c1','level2','sibling','student'),
              ('c','student3','c2','level2','outside','student'),('d','admin','c1','level2','leaf','admin');").execute(&state.db).await.unwrap();
        for c in ["a", "b", "c", "d"] {
            add_turn(&state.db, c, &format!("{c}-q")).await;
        }
        sqlx::query("INSERT INTO kb_messages(message_id,conversation_id,role,content_json,client_message_id) VALUES('inflight','a','user','\"尚未回答\"','pending')").execute(&state.db).await.unwrap();
        capture(&state.db).await.unwrap();
        state
    }
    async fn add_turn(db: &SqlitePool, conversation: &str, id: &str) {
        for (role, text) in [
            ("user", "核反应截面随能量怎样变化？"),
            ("assistant", "请对照课程资料分析。"),
        ] {
            sqlx::query("INSERT INTO kb_messages(message_id,conversation_id,role,content_json,client_message_id) VALUES(?,?,?,?,?)")
                .bind(format!("{id}-{role}")).bind(conversation).bind(role).bind(json!(text).to_string()).bind(id).execute(db).await.unwrap();
        }
    }
    async fn scope(db: &SqlitePool, kind: &str, id: &str) -> LearningScope {
        resolve(
            db,
            ScopeInput {
                scope_type: kind.into(),
                scope_id: id.into(),
            },
        )
        .await
        .unwrap()
    }
    #[tokio::test]
    async fn old_turns_backfill_live_attachments_and_reindex_without_promoting_them_to_course_knowledge(
    ) {
        let state = fixture().await;
        let key = format!("question-{}.png", uuid::Uuid::new_v4());
        state
            .storage
            .put_bytes(&key, b"student-image", "image/png")
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_question_attachments(attachment_id,conversation_id,user_id,original_name,mime_type,storage_key,byte_size,extracted_text) VALUES('image','a','student1','题图.png','image/png',?,13,''),('doc','a','student1','题目.md','text/markdown','question-test.md',4,'题目公式：x^2')")
            .bind(&key).execute(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_messages SET attachments_json='[\"image\",\"doc\"]' WHERE message_id='a-q-user'").execute(&state.db).await.unwrap();
        capture(&state.db).await.unwrap();
        let inputs = learning_inputs(
            &state,
            "a",
            "student1",
            &["image".into(), "doc".into()],
            "学生问题",
        )
        .await
        .unwrap();
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[1]["type"], "image_url");
        assert!(inputs[2]["text"].as_str().unwrap().contains("题目公式"));
        let config = models::load(&state, "embedding").await.unwrap();
        sqlx::query("INSERT INTO kb_learning_vectors(turn_id,fingerprint,source_rowid,conversation_id,course_id,scope_type,scope_id,vector_bytes) SELECT turn_id,?,source_rowid,conversation_id,course_id,scope_type,scope_id,4096 FROM kb_learning_turns WHERE turn_id='a-q-user'")
            .bind(config.fingerprint()).execute(&state.db).await.unwrap();
        let leaf = scope(&state.db, "node", "leaf").await;
        assert_eq!(status(&state, &leaf).await.unwrap()["pendingTurns"], 1);
        let current_key: String = sqlx::query_scalar(&format!(
            "SELECT {MEDIA_KEY_SQL} FROM kb_learning_turns t WHERE turn_id='a-q-user'"
        ))
        .fetch_one(&state.db)
        .await
        .unwrap();
        assert_eq!(current_key, "image,doc");
        sqlx::query("UPDATE kb_learning_vectors SET media_key=?")
            .bind(&current_key)
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(status(&state, &leaf).await.unwrap()["pendingTurns"], 0);
        sqlx::query("UPDATE kb_question_attachments SET expires_at=datetime('now','-1 second')")
            .execute(&state.db)
            .await
            .unwrap();
        assert_eq!(
            status(&state, &leaf).await.unwrap()["pendingTurns"],
            1,
            "expired image must be removed from the multimodal vector"
        );
        let inputs = learning_inputs(
            &state,
            "a",
            "student1",
            &["image".into(), "doc".into()],
            "学生问题",
        )
        .await
        .unwrap();
        assert_eq!(inputs.len(), 1);
        let course_parts: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_content_parts")
            .fetch_one(&state.db)
            .await
            .unwrap();
        assert_eq!(course_parts, 0);
        state.storage.delete(&key).await.unwrap();
    }

    #[tokio::test]
    async fn legacy_roles_are_confirmed_from_authenticated_identity_before_capture() {
        let state = fixture().await;
        sqlx::query("INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id) VALUES('legacy-student','old-student','c1','level2','leaf'),('legacy-admin','old-admin','c1','level2','leaf')")
            .execute(&state.db).await.unwrap();
        add_turn(&state.db, "legacy-student", "legacy-student-q").await;
        add_turn(&state.db, "legacy-admin", "legacy-admin-q").await;
        capture(&state.db).await.unwrap();
        let current = scope(&state.db, "node", "leaf").await;
        assert_eq!(status(&state, &current).await.unwrap()["turnCount"], 1);
        assert_eq!(
            status(&state, &current).await.unwrap()["unclassifiedConversationCount"],
            2
        );
        confirm_role(
            &state,
            &crate::auth::AuthenticatedUser {
                id: "old-student".into(),
                role: crate::auth::UserRole::Student,
                profile: None,
            },
        )
        .await
        .unwrap();
        confirm_role(
            &state,
            &crate::auth::AuthenticatedUser {
                id: "old-admin".into(),
                role: crate::auth::UserRole::Admin,
                profile: None,
            },
        )
        .await
        .unwrap();
        capture(&state.db).await.unwrap();
        assert_eq!(status(&state, &current).await.unwrap()["turnCount"], 2);
        assert_eq!(
            status(&state, &current).await.unwrap()["unclassifiedConversationCount"],
            0
        );
    }

    #[tokio::test]
    async fn student_counts_are_scoped_exact_and_idempotent() {
        let state = fixture().await;
        capture(&state.db).await.unwrap();
        for (kind, id, expected) in [
            ("all", "", 3),
            ("course", "c1", 2),
            ("node", "root", 2),
            ("node", "leaf", 1),
            ("node", "outside", 1),
        ] {
            let current = scope(&state.db, kind, id).await;
            let data = status(&state, &current).await.unwrap();
            assert_eq!(data["turnCount"], expected);
            assert_eq!(data["studentCount"], expected);
        }
        let (stats, samples) = retrieve(
            &state,
            &scope(&state.db, "node", "leaf").await,
            "谁问过核反应截面",
        )
        .await
        .unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].user_id, "student1");
        assert_eq!(stats["totals"]["turnCount"], 1);
        assert_eq!(stats["byConversationScope"][0]["questionCount"], 1);
    }

    #[tokio::test]
    async fn topic_inputs_merge_punctuation_preserve_math_and_deduplicate_students() {
        let state = fixture().await;
        add_turn(&state.db, "a", "follow-up").await;
        sqlx::query("UPDATE kb_messages SET content_json=? WHERE message_id='follow-up-user'")
            .bind(json!("核反应截面随能量怎样变化").to_string())
            .execute(&state.db)
            .await
            .unwrap();
        capture(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_learning_turns SET created_at=datetime('now','-3 days') WHERE conversation_id='a'").execute(&state.db).await.unwrap();
        sqlx::query("UPDATE kb_learning_turns SET created_at=datetime('now','-10 days') WHERE conversation_id='b'").execute(&state.db).await.unwrap();
        let current = scope(&state.db, "course", "c1").await;
        let (stats, _) = retrieve(&state, &current, "这个章节学生关注什么？")
            .await
            .unwrap();
        let groups = stats["questionGroups"]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["count"], 3);
        assert_eq!(groups[0]["studentCount"], 2);
        assert_eq!(groups[0]["userIds"].as_array().unwrap().len(), 2);
        assert_eq!(stats["questionGroups"]["coveredQuestionCount"], 3);
        assert_eq!(stats["timeWindows"][0]["questionCount"], 2);
        assert_eq!(stats["timeWindows"][1]["questionCount"], 1);
        assert_ne!(question_key("x+y 是什么？"), question_key("x-y 是什么？"));
        assert_ne!(question_key("n!"), question_key("n"));
    }
    #[tokio::test]
    async fn clear_data_keeps_vectors_separate_and_does_not_reimport_old_records() {
        let state = fixture().await;
        sqlx::query("INSERT INTO kb_learning_vectors(turn_id,fingerprint,source_rowid,conversation_id,course_id,scope_type,scope_id,vector_bytes) SELECT turn_id,'f',source_rowid,conversation_id,course_id,scope_type,scope_id,4096 FROM kb_learning_turns").execute(&state.db).await.unwrap();
        let leaf = scope(&state.db, "node", "leaf").await;
        clear(&state, &leaf, "data").await.unwrap();
        capture(&state.db).await.unwrap();
        let data = status(&state, &leaf).await.unwrap();
        assert_eq!(data["turnCount"], 0);
        assert_eq!(data["vectorCount"], 1);
        // New message sequence stays above the cutoff, even if old message
        // rowids were deleted and SQLite now reuses them.
        sqlx::query("DELETE FROM kb_messages WHERE message_id='inflight'")
            .execute(&state.db)
            .await
            .unwrap();
        add_turn(&state.db, "a", "new-q").await;
        capture(&state.db).await.unwrap();
        let data = status(&state, &leaf).await.unwrap();
        assert_eq!(data["turnCount"], 1);
        assert_eq!(
            status(&state, &scope(&state.db, "node", "sibling").await)
                .await
                .unwrap()["turnCount"],
            1
        );
    }
    #[tokio::test]
    async fn clear_vectors_awaits_database_deletion_and_keeps_transcripts() {
        let mut state = fixture().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/collections/test_learning/points/delete",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                assert_eq!(body["points"], json!(["a-q-user"]));
                axum::Json(json!({"status":"ok"}))
            }),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut config = crate::config::AppConfig::test_config().qdrant;
        config.http_url = format!("http://{address}").parse().unwrap();
        state.qdrant = crate::qdrant::QdrantAdapter::new(config).unwrap();
        sqlx::query("INSERT INTO kb_learning_collections VALUES('f','test_learning',1024)")
            .execute(&state.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_learning_vectors(turn_id,fingerprint,source_rowid,conversation_id,course_id,scope_type,scope_id,vector_bytes) SELECT turn_id,'f',source_rowid,conversation_id,course_id,scope_type,scope_id,4096 FROM kb_learning_turns WHERE conversation_id='a'").execute(&state.db).await.unwrap();
        let leaf = scope(&state.db, "node", "leaf").await;
        clear(&state, &leaf, "vectors").await.unwrap();
        let data = status(&state, &leaf).await.unwrap();
        assert_eq!(data["turnCount"], 1);
        assert_eq!(data["vectorCount"], 0);
        assert_eq!(data["pendingTurns"], 0);
        capture(&state.db).await.unwrap();
        assert_eq!(status(&state, &leaf).await.unwrap()["turnCount"], 1);
        server.abort();
    }
}
