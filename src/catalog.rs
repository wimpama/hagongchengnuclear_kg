use std::collections::{HashMap, HashSet};

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{Row, SqlitePool};

use crate::{
    auth::{bearer_token, require_admin, require_user},
    error::ApiError,
    state::AppState,
};

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Scope {
    pub course_id: String,
    pub node_ids: Vec<String>,
    pub title: String,
    /// 0 = entire course, 1 = parent node, 2 = leaf node.
    pub level: u8,
}

#[derive(Debug, Clone)]
struct Course {
    id: String,
    title: String,
    is_default: bool,
}

#[derive(Debug, Clone)]
struct Node {
    id: String,
    course_id: String,
    parent_id: Option<String>,
    title: String,
    level: u8,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncResult {
    pub course_count: usize,
    pub node_count: usize,
}

/// Reads every page and complete tree before starting the write transaction.
/// Any upstream failure leaves the previously published mirror unchanged.
pub async fn sync_catalog(state: &AppState, token: &str) -> Result<SyncResult, ApiError> {
    let mut courses = Vec::new();
    let mut page = 1usize;
    let mut expected_total = None;
    loop {
        let raw: Value = state
            .auth
            .get(token, &format!("/admin/courses?page={page}&pageSize=100"))
            .await?;
        let list = raw
            .get("list")
            .and_then(Value::as_array)
            .ok_or_else(ApiError::upstream)?;
        let total = raw
            .get("total")
            .and_then(Value::as_u64)
            .ok_or_else(ApiError::upstream)? as usize;
        if expected_total.is_some_and(|previous| previous != total) {
            // A changing total means the pages do not describe one complete
            // catalog; retry instead of deactivating potentially missing rows.
            return Err(ApiError::unavailable("课程目录正在变化，请重试同步"));
        }
        expected_total = Some(total);
        if total > 10_000 {
            return Err(ApiError::unavailable("课程目录超过单次同步限制"));
        }
        if list.is_empty() && courses.len() < total {
            return Err(ApiError::upstream());
        }
        for entry in list {
            courses.push(parse_course(entry)?);
        }
        if courses.len() > total {
            return Err(ApiError::upstream());
        }
        if courses.len() >= total {
            break;
        }
        page += 1;
        if page > 100 {
            return Err(ApiError::upstream());
        }
    }
    let mut course_ids = HashSet::new();
    let mut all_node_ids = HashSet::new();
    let mut nodes = Vec::new();
    for course in &courses {
        if !course_ids.insert(course.id.clone()) {
            return Err(ApiError::upstream());
        }
        let roots: Vec<Value> = state
            .auth
            .get(token, &format!("/admin/courses/{}/nodes", course.id))
            .await?;
        let mut hydrated = Vec::new();
        for root in roots {
            let id = read_id(root.get("id"))?;
            let detail: Value = state.auth.get(token, &format!("/admin/nodes/{id}")).await?;
            // Match the frontend's authoritative detail call. Never assume a
            // missing children field in a summary is an empty subtree.
            if read_id(detail.get("id"))? != id {
                return Err(ApiError::upstream());
            }
            hydrated.push(detail);
        }
        let flattened = validate_tree(&course.id, &hydrated)?;
        for node in flattened {
            if !all_node_ids.insert(node.id.clone()) {
                return Err(ApiError::upstream());
            }
            nodes.push(node);
        }
    }
    let mut transaction = state.db.begin().await?;
    sqlx::query("UPDATE kb_courses SET is_active = 0")
        .execute(&mut *transaction)
        .await?;
    sqlx::query("UPDATE kb_course_nodes SET is_active = 0")
        .execute(&mut *transaction)
        .await?;
    for course in &courses {
        sqlx::query("INSERT INTO kb_courses(course_id,title,is_default,is_active,synced_at) VALUES(?,?,?,1,CURRENT_TIMESTAMP) ON CONFLICT(course_id) DO UPDATE SET title=excluded.title,is_default=excluded.is_default,is_active=1,synced_at=CURRENT_TIMESTAMP")
            .bind(&course.id).bind(&course.title).bind(course.is_default).execute(&mut *transaction).await?;
    }
    // Parents are inserted before children so foreign-key validation stays on.
    nodes.sort_by_key(|node| node.level);
    for node in &nodes {
        sqlx::query("INSERT INTO kb_course_nodes(node_id,course_id,parent_node_id,level,title,is_active,synced_at) VALUES(?,?,?,?,?,1,CURRENT_TIMESTAMP) ON CONFLICT(node_id) DO UPDATE SET course_id=excluded.course_id,parent_node_id=excluded.parent_node_id,level=excluded.level,title=excluded.title,is_active=1,synced_at=CURRENT_TIMESTAMP")
            .bind(&node.id).bind(&node.course_id).bind(&node.parent_id).bind(node.level as i64).bind(&node.title).execute(&mut *transaction).await?;
    }
    sqlx::query("INSERT INTO kb_catalog_sync_state(singleton,generation,course_count,node_count) VALUES(1,1,?,?) ON CONFLICT(singleton) DO UPDATE SET generation=generation+1,synced_at=CURRENT_TIMESTAMP,course_count=excluded.course_count,node_count=excluded.node_count")
        .bind(courses.len() as i64).bind(nodes.len() as i64).execute(&mut *transaction).await?;
    transaction.commit().await?;
    Ok(SyncResult {
        course_count: courses.len(),
        node_count: nodes.len(),
    })
}

pub async fn resolve_scope(
    state: &AppState,
    token: &str,
    scope_type: &str,
    scope_id: Option<&str>,
) -> Result<Scope, ApiError> {
    if !matches!(scope_type, "course" | "node") {
        return Err(ApiError::bad_request("不支持的知识范围"));
    }
    let scope_id = scope_id.filter(|id| !id.is_empty());
    if let Some(id) = scope_id {
        validate_id(id)?;
    }

    // User-visible catalog is fetched with this user's token on every request.
    // An admin mirror never grants a student access by itself.
    let visible: Vec<Value> = state.auth.get(token, "/courses").await?;
    let courses = visible
        .iter()
        .map(parse_course)
        .collect::<Result<Vec<_>, _>>()?;
    let requested_node = if scope_type == "node" {
        let id = scope_id.ok_or_else(|| ApiError::bad_request("缺少节点 ID"))?;
        let row = sqlx::query("SELECT n.course_id,n.level,n.parent_node_id,n.title FROM kb_course_nodes n JOIN kb_courses c ON c.course_id=n.course_id WHERE n.node_id=? AND n.is_active=1 AND c.is_active=1")
            .bind(id).fetch_optional(&state.db).await?.ok_or_else(|| ApiError::not_found("知识节点未同步或已失效"))?;
        Some((
            row.get::<String, _>("course_id"),
            row.get::<i64, _>("level") as u8,
            row.get::<Option<String>, _>("parent_node_id"),
            row.get::<String, _>("title"),
        ))
    } else {
        None
    };

    let course_id = if let Some(node) = &requested_node {
        node.0.clone()
    } else if let Some(id) = scope_id {
        id.to_owned()
    } else {
        let default: Option<Value> = state.auth.get(token, "/courses/default").await?;
        match default {
            Some(value) => read_id(value.get("id"))?,
            None => fallback_course_id(&courses)?,
        }
    };
    let course = courses
        .iter()
        .find(|course| course.id == course_id)
        .ok_or_else(|| ApiError::forbidden("无权访问该课程"))?;
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM kb_courses WHERE course_id=? AND is_active=1")
            .bind(&course_id)
            .fetch_one(&state.db)
            .await?;
    if active == 0 {
        return Err(ApiError::unavailable("请管理员先同步课程目录"));
    }

    let graph: Value = state
        .auth
        .get(token, &format!("/courses/{course_id}/graph"))
        .await?;
    if read_id(graph.get("course").and_then(|course| course.get("id")))? != course_id {
        return Err(ApiError::upstream());
    }
    let roots = graph
        .get("nodes")
        .and_then(Value::as_array)
        .ok_or_else(ApiError::upstream)?;
    let mut visible_tree = Vec::new();
    for root in roots {
        if root.get("level").and_then(Value::as_u64) != Some(1) {
            return Err(ApiError::upstream());
        }
        if root.get("parentId").is_some_and(|parent| !parent.is_null()) {
            return Err(ApiError::upstream());
        }
        validate_course_reference(&course_id, root)?;
        let id = read_id(root.get("id"))?;
        let detail: Value = state.auth.get(token, &format!("/nodes/{id}")).await?;
        if read_id(detail.get("id"))? != id {
            return Err(ApiError::upstream());
        }
        visible_tree.push(detail);
    }
    let visible_nodes = validate_tree(&course_id, &visible_tree)?;
    let mirror = sqlx::query("SELECT node_id,parent_node_id,level FROM kb_course_nodes WHERE course_id=? AND is_active=1").bind(&course_id).fetch_all(&state.db).await?;
    let mirror: HashMap<String, (Option<String>, u8)> = mirror
        .into_iter()
        .map(|row| {
            (
                row.get("node_id"),
                (row.get("parent_node_id"), row.get::<i64, _>("level") as u8),
            )
        })
        .collect();
    let (level, title) = requested_node
        .as_ref()
        .map(|node| (node.1, node.3.clone()))
        .unwrap_or((0, course.title.clone()));
    if let Some((_, node_level, parent, _)) = &requested_node {
        let node = visible_nodes
            .iter()
            .find(|node| Some(node.id.as_str()) == scope_id)
            .ok_or_else(|| ApiError::forbidden("该节点不在当前可访问课程中"))?;
        if node.level != *node_level || node.parent_id != *parent {
            return Err(ApiError::unavailable("课程结构已改变，请管理员重新同步"));
        }
    }
    let node_ids = visible_nodes
        .iter()
        .filter(|node| {
            node.level == 2
                && mirror
                    .get(&node.id)
                    .is_some_and(|(parent, level)| *level == 2 && *parent == node.parent_id)
                && match level {
                    0 => true,
                    1 => node.parent_id.as_deref() == scope_id,
                    2 => Some(node.id.as_str()) == scope_id,
                    _ => false,
                }
        })
        .map(|node| node.id.clone())
        .collect();
    Ok(Scope {
        course_id,
        node_ids,
        title,
        level,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextQuery {
    scope_type: String,
    scope_id: Option<String>,
}

pub async fn context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ContextQuery>,
) -> Result<Json<Value>, ApiError> {
    require_user(&state, &headers).await?;
    let token = bearer_token(&headers).ok_or_else(ApiError::unauthorized)?;
    let scope = resolve_scope(&state, token, &query.scope_type, query.scope_id.as_deref()).await?;
    let published_asset_count = published_asset_count(&state.db, &scope).await?;
    let available = published_asset_count > 0 && state.qdrant.health().await.ok;
    let message = if published_asset_count == 0 {
        "这个范围暂无可用于问答的已发布知识资料"
    } else if !available {
        "课程知识库暂不可用，请稍后重试"
    } else {
        "可以开始课程知识库对话"
    };
    Ok(Json(
        json!({"scopeType":query.scope_type,"scopeId":query.scope_id.filter(|id|!id.is_empty()).unwrap_or_else(||scope.course_id.clone()),"title":scope.title,"courseId":scope.course_id,"level":scope.level,"nodeCount":scope.node_ids.len(),"publishedAssetCount":published_asset_count,"ready":available,"available":available,"message":message}),
    ))
}

pub(crate) async fn published_asset_count(
    pool: &SqlitePool,
    scope: &Scope,
) -> Result<i64, ApiError> {
    if scope.node_ids.is_empty() {
        return Ok(0);
    }
    let nodes = serde_json::to_string(&scope.node_ids).map_err(|_| ApiError::internal())?;
    let count=sqlx::query_scalar::<_,i64>(
        "SELECT COUNT(DISTINCT a.asset_id) FROM kb_assets a
         JOIN kb_course_nodes n ON n.node_id=a.node_id AND n.is_active=1 AND n.level=2
         JOIN kb_asset_revisions r ON r.revision_id=a.published_revision_id AND r.asset_id=a.asset_id
         JOIN kb_active_indexes ai ON ai.course_id=n.course_id
         JOIN kb_vector_collections vc ON vc.collection_id=ai.collection_id AND vc.course_id=n.course_id AND vc.status='active'
         JOIN kb_embedding_configs ec ON ec.embedding_config_id=vc.embedding_config_id
         WHERE n.course_id=? AND a.node_id IN (SELECT value FROM json_each(?))
           AND a.deleted_at IS NULL AND r.status='published'
           AND r.review_status='approved' AND r.index_status='verified'
           AND r.collection_id=vc.collection_id AND r.embedding_config_id=vc.embedding_config_id
           AND r.expected_vector_count>0
           AND r.expected_vector_count=(SELECT COUNT(*) FROM kb_content_parts p WHERE p.revision_id=r.revision_id AND p.evidence_status IN ('approved','visual_only'))
           AND NOT EXISTS(SELECT 1 FROM kb_content_parts p WHERE p.revision_id=r.revision_id AND p.evidence_status NOT IN ('approved','visual_only','unavailable'))
           AND r.expected_vector_count=(SELECT COUNT(*) FROM kb_vector_records vr
               JOIN kb_content_parts p ON p.part_id=vr.part_id AND p.revision_id=r.revision_id
               WHERE vr.revision_id=r.revision_id AND vr.collection_id=vc.collection_id
                 AND vr.status='verified' AND vr.dimensions=ec.dimensions)"
    ).bind(&scope.course_id).bind(&nodes).fetch_one(pool).await?;
    let files: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM kb_assets a JOIN kb_course_nodes n ON n.node_id=a.node_id JOIN kb_asset_revisions r ON r.revision_id=a.published_revision_id WHERE n.course_id=? AND n.node_id IN (SELECT value FROM json_each(?)) AND n.is_active=1 AND a.deleted_at IS NULL AND a.evidence_mode='file' AND r.status='published' AND r.review_status='approved' AND r.index_status='not_required'")
        .bind(&scope.course_id).bind(&nodes).fetch_one(pool).await?;
    Ok(count + files)
}

pub async fn sync(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<SyncResult>, ApiError> {
    require_admin(&state, &headers).await?;
    let token = bearer_token(&headers).ok_or_else(ApiError::unauthorized)?;
    Ok(Json(sync_catalog(&state, token).await?))
}

fn parse_course(value: &Value) -> Result<Course, ApiError> {
    Ok(Course {
        id: read_id(value.get("id"))?,
        title: value
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(ApiError::upstream)?
            .to_owned(),
        is_default: value
            .get("isDefault")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

fn fallback_course_id(courses: &[Course]) -> Result<String, ApiError> {
    courses
        .iter()
        .find(|course| course.is_default)
        .or_else(|| courses.first())
        .map(|course| course.id.clone())
        .ok_or_else(|| ApiError::not_found("没有可访问课程"))
}

fn validate_tree(course_id: &str, roots: &[Value]) -> Result<Vec<Node>, ApiError> {
    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    for root in roots {
        if root.get("level").and_then(Value::as_u64) != Some(1) {
            return Err(ApiError::upstream());
        }
        if root.get("parentId").is_some_and(|parent| !parent.is_null()) {
            return Err(ApiError::upstream());
        }
        validate_course_reference(course_id, root)?;
        let root_id = read_id(root.get("id"))?;
        if !seen.insert(root_id.clone()) {
            return Err(ApiError::upstream());
        }
        nodes.push(Node {
            id: root_id.clone(),
            course_id: course_id.into(),
            parent_id: None,
            title: root
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(ApiError::upstream)?
                .into(),
            level: 1,
        });
        let children = root
            .get("children")
            .and_then(Value::as_array)
            .ok_or_else(ApiError::upstream)?;
        for child in children {
            if child.get("level").and_then(Value::as_u64) != Some(2)
                || child
                    .get("children")
                    .and_then(Value::as_array)
                    .is_some_and(|children| !children.is_empty())
            {
                return Err(ApiError::upstream());
            }
            let id = read_id(child.get("id"))?;
            validate_course_reference(course_id, child)?;
            if !seen.insert(id.clone()) {
                return Err(ApiError::upstream());
            }
            if let Some(parent) = child.get("parentId").filter(|value| !value.is_null()) {
                if read_id(Some(parent))? != root_id {
                    return Err(ApiError::upstream());
                }
            }
            nodes.push(Node {
                id,
                course_id: course_id.into(),
                parent_id: Some(root_id.clone()),
                title: child
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(ApiError::upstream)?
                    .into(),
                level: 2,
            });
        }
    }
    Ok(nodes)
}

fn validate_course_reference(course_id: &str, node: &Value) -> Result<(), ApiError> {
    if let Some(value) = node.get("courseId").filter(|value| !value.is_null()) {
        if read_id(Some(value))? != course_id {
            return Err(ApiError::upstream());
        }
    }
    Ok(())
}

pub(crate) fn read_id(value: Option<&Value>) -> Result<String, ApiError> {
    let id = match value {
        Some(Value::String(id)) => id.clone(),
        Some(Value::Number(id)) if id.is_i64() || id.is_u64() => id.to_string(),
        _ => return Err(ApiError::upstream()),
    };
    validate_id(&id)?;
    Ok(id)
}
pub(crate) fn validate_id(id: &str) -> Result<(), ApiError> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        return Err(ApiError::bad_request("无效 ID"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_real_parent_relationship() {
        let nodes=validate_tree("course1",&[json!({"id":1,"name":"root","level":1,"children":[{"id":2,"name":"leaf","level":2}]})]).unwrap();
        assert_eq!(nodes[1].parent_id.as_deref(), Some("1"));
        assert_eq!(nodes[1].course_id, "course1");
    }
    #[test]
    fn rejects_duplicate_ids_and_invalid_depth() {
        assert!(validate_tree(
            "c",
            &[json!({"id":1,"name":"a","level":1,"children":[{"id":1,"name":"b","level":2}]})]
        )
        .is_err());
        assert!(validate_tree(
            "c",
            &[json!({"id":1,"name":"a","level":1,"children":[{"id":2,"name":"b","level":3}]})]
        )
        .is_err());
        assert!(validate_tree("c", &[json!({"id":1,"name":"a","level":1})]).is_err());
    }
    #[test]
    fn rejects_forged_parent_and_path_ids() {
        assert!(validate_tree("c",&[json!({"id":1,"name":"a","level":1,"children":[{"id":2,"parentId":99,"name":"b","level":2}]})]).is_err());
        assert!(validate_id("../admin").is_err());
    }

    #[test]
    fn null_default_uses_visible_catalog_without_treating_a_node_as_course() {
        // The live API currently returns data:null from /courses/default.
        let courses = vec![
            parse_course(&json!({"id":11,"name":"course A","isDefault":false})).unwrap(),
            parse_course(&json!({"id":12,"name":"course B","isDefault":false})).unwrap(),
        ];
        assert_eq!(fallback_course_id(&courses).unwrap(), "11");
        assert!(fallback_course_id(&[]).is_err());
    }

    #[test]
    fn live_summary_shape_does_not_require_parent_ids_on_children() {
        let roots = [json!({"id":101,"level":1,"name":"root","parentId":null,
            "children":[{"id":102,"code":"1.1","name":"leaf","level":2,"orderIndex":0,"status":"NOT_STARTED"}]})];
        assert_eq!(
            validate_tree("10", &roots).unwrap()[1].parent_id.as_deref(),
            Some("101")
        );
        assert!(validate_tree(
            "10",
            &[json!({"id":101,"name":"bad root","level":1,"parentId":99,"children":[]})]
        )
        .is_err());
    }

    #[tokio::test]
    async fn context_excludes_withdrawn_unverified_and_wrong_collection_assets() {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::migrate!("./migrations").run(&pool).await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO kb_courses(course_id,title) VALUES('c','course');
             INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('root','c',1,'root');
             INSERT INTO kb_course_nodes(node_id,course_id,parent_node_id,level,title) VALUES('leaf','c','root',2,'leaf');
             INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by) VALUES('a','leaf','text','a.txt','a.txt',1,'admin');
             INSERT INTO kb_embedding_configs(embedding_config_id,provider,model,dimensions,config_fingerprint,created_by) VALUES('ec','ark','embedding',1024,'f1','admin');
             INSERT INTO kb_vector_collections(collection_id,course_id,embedding_config_id,generation,qdrant_collection,status) VALUES('vc','c','ec',1,'collection1','active');
             INSERT INTO kb_active_indexes(course_id,collection_id) VALUES('c','vc');
             INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by,review_status,index_status,embedding_config_id,collection_id,expected_vector_count) VALUES('r','a',1,'hash','v1','published','admin','approved','verified','ec','vc',1);
             UPDATE kb_assets SET published_revision_id='r' WHERE asset_id='a';
             INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,content_text,content_sha256,evidence_status) VALUES('p','r',0,'text','evidence','hash','approved');
             INSERT INTO kb_vector_records(vector_record_id,collection_id,revision_id,part_id,qdrant_point_id,modality,dimensions,status) VALUES('vr','vc','r','p','point','text',1024,'verified');"
        ).execute(&pool).await.unwrap();
        let scope = Scope {
            course_id: "c".into(),
            node_ids: vec!["leaf".into()],
            title: "course".into(),
            level: 0,
        };
        assert_eq!(published_asset_count(&pool, &scope).await.unwrap(), 1);
        sqlx::query("UPDATE kb_assets SET published_revision_id=NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(published_asset_count(&pool, &scope).await.unwrap(), 0);
        sqlx::query("UPDATE kb_assets SET published_revision_id='r'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE kb_vector_records SET status='written'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(published_asset_count(&pool, &scope).await.unwrap(), 0);
        sqlx::query("UPDATE kb_vector_records SET status='verified'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE kb_asset_revisions SET collection_id=NULL")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(published_asset_count(&pool, &scope).await.unwrap(), 0);
        sqlx::query("UPDATE kb_asset_revisions SET collection_id='vc'")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE kb_content_parts SET evidence_status='pending'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(published_asset_count(&pool, &scope).await.unwrap(), 0);
        sqlx::query("UPDATE kb_content_parts SET evidence_status='visual_only'")
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(published_asset_count(&pool, &scope).await.unwrap(), 1);
        let other = Scope {
            node_ids: vec!["other_leaf".into()],
            ..scope
        };
        assert_eq!(published_asset_count(&pool, &other).await.unwrap(), 0);
    }

    /// Explicitly opt-in: performs login and read-only upstream requests, and
    /// stores the mirror only in an ephemeral in-memory SQLite database.
    #[tokio::test]
    #[ignore = "requires authorized test credentials in environment"]
    async fn live_auth_catalog_scopes() -> anyhow::Result<()> {
        use crate::{
            auth::{AuthUpstream, UserRole},
            config::AppConfig,
            qdrant::QdrantAdapter,
        };
        use axum::http::{header, HeaderValue, StatusCode};

        async fn login(
            client: &reqwest::Client,
            base: &str,
            account_var: &str,
            password_var: &str,
        ) -> anyhow::Result<String> {
            let account = std::env::var(account_var)?;
            let password = std::env::var(password_var)?;
            let response = client
                .post(format!("{base}/auth/login"))
                .json(&json!({"account":account,"password":password}))
                .send()
                .await?;
            anyhow::ensure!(response.status().is_success(), "test login rejected");
            let raw = crate::auth::unwrap_envelope(response.json().await?)
                .map_err(|_| anyhow::anyhow!("test login envelope rejected"))?;
            raw.get("token")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| anyhow::anyhow!("test login returned no token"))
        }

        fn headers(token: &str) -> HeaderMap {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {token}")).unwrap(),
            );
            headers
        }

        let base = std::env::var("KB_TEST_UPSTREAM_BASE_URL")
            .unwrap_or_else(|_| "https://futuredev.artz.zone/api".into());
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        let admin_token = login(
            &client,
            &base,
            "KB_TEST_ADMIN_ACCOUNT",
            "KB_TEST_ADMIN_PASSWORD",
        )
        .await?;
        let student_token = login(
            &client,
            &base,
            "KB_TEST_STUDENT_ACCOUNT",
            "KB_TEST_STUDENT_PASSWORD",
        )
        .await?;
        let mut config = AppConfig::from_env()?;
        config.auth.base_url = Some(reqwest::Url::parse(&base)?);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await?;
        sqlx::migrate!("./migrations").run(&pool).await?;
        let auth = AuthUpstream::new(config.auth.clone())?;
        let qdrant = QdrantAdapter::new(config.qdrant.clone())?;
        let state = AppState::new(config, pool, qdrant, auth);
        assert_eq!(
            require_admin(&state, &headers(&admin_token))
                .await
                .unwrap()
                .role,
            UserRole::Admin
        );
        assert_eq!(
            require_user(&state, &headers(&student_token))
                .await
                .unwrap()
                .role,
            UserRole::Student
        );
        assert_eq!(
            require_admin(&state, &headers(&student_token))
                .await
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        let sync = sync_catalog(&state, &admin_token).await.unwrap();
        assert!(sync.course_count > 0 && sync.node_count > 0);
        let course = resolve_scope(&state, &student_token, "course", None)
            .await
            .unwrap();
        assert_eq!(course.level, 0);
        let root_id:String=sqlx::query_scalar("SELECT node_id FROM kb_course_nodes WHERE course_id=? AND level=1 AND is_active=1 ORDER BY node_id LIMIT 1")
            .bind(&course.course_id).fetch_one(&state.db).await?;
        let root = resolve_scope(&state, &student_token, "node", Some(&root_id))
            .await
            .unwrap();
        assert_eq!(root.level, 1);
        assert!(!root.node_ids.is_empty());
        assert!(root.node_ids.iter().all(|id| course.node_ids.contains(id)));
        let leaf_id = &root.node_ids[0];
        let leaf = resolve_scope(&state, &student_token, "node", Some(leaf_id))
            .await
            .unwrap();
        assert_eq!(leaf.level, 2);
        assert_eq!(leaf.node_ids, vec![leaf_id.clone()]);
        assert_eq!(
            resolve_scope(&state, &student_token, "course", Some("__missing_course"))
                .await
                .unwrap_err()
                .status,
            StatusCode::FORBIDDEN
        );
        Ok(())
    }
}
