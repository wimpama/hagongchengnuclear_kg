//! Existing courseware is resolved by ID through the authenticated course API.
//! Signed URLs stay server-side and are never accepted from an import request.
use axum::{
    extract::{Path, State},
    http::HeaderMap,
    routing::get,
    Json, Router,
};
use reqwest::Url;
use serde::Serialize;
use serde_json::Value;
use sqlx::Row;

use crate::{
    auth::{bearer_token, require_admin},
    catalog::{read_id, validate_id},
    config::Environment,
    error::ApiError,
    ingest::{file_type, require_active_leaf},
    state::AppState,
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/kb-api/v1/admin/nodes/{node_id}/knowledge/context",
            get(node_context),
        )
        .route(
            "/kb-api/v1/admin/nodes/{node_id}/knowledge/coursewares",
            get(list_coursewares),
        )
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeContext {
    node_id: String,
    node_title: String,
    course_id: String,
    course_title: String,
    parent_node_id: String,
    parent_node_title: String,
    parent_node_code: String,
}

async fn node_context(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
) -> Result<Json<NodeContext>, ApiError> {
    require_admin(&state, &headers).await?;
    validate_id(&node_id)?;
    require_active_leaf(&state.db, &node_id).await?;
    let token = bearer_token(&headers).ok_or_else(ApiError::unauthorized)?;
    let row = sqlx::query("SELECT n.course_id,n.parent_node_id,c.title AS course_title FROM kb_course_nodes n JOIN kb_courses c ON c.course_id=n.course_id AND c.is_active=1 WHERE n.node_id=? AND n.is_active=1")
        .bind(&node_id).fetch_optional(&state.db).await?
        .ok_or_else(|| ApiError::bad_request("请先同步课程结构"))?;
    let parent_id: String = row
        .try_get("parent_node_id")
        .map_err(|_| ApiError::upstream())?;
    let node: Value = state
        .auth
        .get(token, &format!("/admin/nodes/{node_id}"))
        .await?;
    let parent: Value = state
        .auth
        .get(token, &format!("/admin/nodes/{parent_id}"))
        .await?;
    if read_id(node.get("id"))? != node_id
        || read_id(node.get("parentId"))? != parent_id
        || node.get("level").and_then(Value::as_u64) != Some(2)
        || read_id(parent.get("id"))? != parent_id
        || parent.get("level").and_then(Value::as_u64) != Some(1)
    {
        return Err(ApiError::bad_request("节点层级已变化，请重新同步课程结构"));
    }
    Ok(Json(NodeContext {
        node_id,
        node_title: node
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        course_id: row.get("course_id"),
        course_title: row.get("course_title"),
        parent_node_id: parent_id,
        parent_node_title: parent
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        parent_node_code: parent
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CoursewareView {
    pub id: String,
    pub name: String,
    pub content_type: String,
    pub kind: Option<String>,
    pub size_bytes: Option<u64>,
    pub created_at: Option<String>,
    pub importable: bool,
    pub unavailable_reason: Option<String>,
}

pub struct ImportSource {
    pub id: String,
    pub name: String,
    pub url: Url,
    pub extension: String,
    pub kind: &'static str,
    pub mime: &'static str,
}

async fn node_coursewares(
    state: &AppState,
    token: &str,
    node_id: &str,
) -> Result<Vec<Value>, ApiError> {
    validate_id(node_id)?;
    require_active_leaf(&state.db, node_id).await?;
    let node: Value = state
        .auth
        .get(token, &format!("/admin/nodes/{node_id}"))
        .await?;
    if read_id(node.get("id"))? != node_id || node.get("level").and_then(Value::as_u64) != Some(2) {
        return Err(ApiError::bad_request(
            "只能从当前二级节点导入课件，请同步课程结构后重试",
        ));
    }
    state
        .auth
        .get(token, &format!("/admin/nodes/{node_id}/coursewares"))
        .await
}

async fn list_coursewares(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(node_id): Path<String>,
) -> Result<Json<Vec<CoursewareView>>, ApiError> {
    require_admin(&state, &headers).await?;
    let token = bearer_token(&headers).ok_or_else(ApiError::unauthorized)?;
    let items = node_coursewares(&state, token, &node_id).await?;
    let mut result = Vec::with_capacity(items.len());
    for item in items {
        let source = import_source(&item, state.environment);
        result.push(CoursewareView {
            id: read_id(item.get("id"))?,
            name: item
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("未命名课件")
                .into(),
            content_type: item
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .into(),
            kind: source.as_ref().ok().map(|source| source.kind.into()),
            size_bytes: item.get("fileSize").and_then(Value::as_u64),
            created_at: item
                .get("createdAt")
                .and_then(Value::as_str)
                .map(str::to_owned),
            importable: source.is_ok(),
            unavailable_reason: source.err().map(|error| error.message),
        });
    }
    Ok(Json(result))
}

pub async fn resolve_import(
    state: &AppState,
    headers: &HeaderMap,
    node_id: &str,
    courseware_id: &str,
) -> Result<ImportSource, ApiError> {
    validate_id(courseware_id)?;
    let token = bearer_token(headers).ok_or_else(ApiError::unauthorized)?;
    let items = node_coursewares(state, token, node_id).await?;
    let item = items
        .iter()
        .find(|item| read_id(item.get("id")).is_ok_and(|id| id == courseware_id))
        .ok_or_else(|| ApiError::not_found("该课件不属于当前二级节点，或已被删除"))?;
    import_source(item, state.environment)
}

fn import_source(item: &Value, environment: Environment) -> Result<ImportSource, ApiError> {
    if item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| {
            matches!(
                status.to_ascii_uppercase().as_str(),
                "UPLOADING" | "FAILED" | "DELETED" | "PROCESSING"
            )
        })
    {
        return Err(ApiError::bad_request("课件尚未上传完成或不可用"));
    }
    let url = source_url(
        item.get("fileUrl")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        environment,
    )?;
    let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
    let extension = url
        .path()
        .rsplit('.')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let (kind, mime) = file_type(&extension).ok_or_else(|| ApiError::bad_request(
        "此课件格式暂不支持，请将 DOC/PPT/XLS 转为 DOCX/PPTX/CSV，流媒体转为 MP4，GIF 转为 PNG 后上传"
    ))?;
    let maximum = match kind {
        "image" => 20,
        "text" => 10,
        _ => 100,
    } * 1024
        * 1024;
    if item
        .get("fileSize")
        .and_then(Value::as_u64)
        .is_some_and(|size| size == 0 || size > maximum)
    {
        return Err(ApiError::bad_request(format!(
            "文件为空或超过 {}MB 导入限制",
            maximum / 1024 / 1024
        )));
    }
    Ok(ImportSource {
        id: read_id(item.get("id"))?,
        name: name.into(),
        url,
        extension,
        kind,
        mime,
    })
}

fn source_url(value: &str, environment: Environment) -> Result<Url, ApiError> {
    let (setting, defaults) = match environment {
        Environment::Development => (
            "KB_DEV_COURSEWARE_HOSTS",
            "futuredev-cdn.artz.zone,futuredev.oss-cn-beijing.aliyuncs.com",
        ),
        Environment::Production => (
            "KB_PROD_COURSEWARE_HOSTS",
            "future-cdn.artz.zone,futureprod.oss-cn-beijing.aliyuncs.com",
        ),
    };
    let hosts = std::env::var(setting).unwrap_or_else(|_| defaults.into());
    checked_source_url(value, &hosts)
}

fn checked_source_url(value: &str, hosts: &str) -> Result<Url, ApiError> {
    let error = || ApiError::bad_request("课件链接无效或来源域名未配置，请检查后端课件来源配置");
    let url = Url::parse(value).map_err(|_| error())?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(443)
        || url.fragment().is_some()
        || !url.path().starts_with("/courseware/")
        || !hosts
            .split(',')
            .any(|host| Some(host.trim()) == url.host_str())
    {
        return Err(error());
    }
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn source_urls_reject_other_tenants_private_hosts_and_redirect_targets() {
        let hosts = "futuredev-cdn.artz.zone,futuredev.oss-cn-beijing.aliyuncs.com";
        assert!(checked_source_url(
            "https://futuredev-cdn.artz.zone/courseware/pdf/%E4%B9%A0.pdf?Expires=1&Signature=test",
            hosts
        )
        .is_ok());
        for value in [
            "https://future-cdn.artz.zone/courseware/a.pdf",
            "https://futuredev-cdn.artz.zone.evil.test/courseware/a.pdf",
            "https://futuredev-cdn.artz.zone@127.0.0.1/courseware/a.pdf",
            "https://127.0.0.1/courseware/a.pdf",
            "http://futuredev-cdn.artz.zone/courseware/a.pdf",
            "https://futuredev-cdn.artz.zone:8443/courseware/a.pdf",
            "https://futuredev-cdn.artz.zone/api/redirect",
            "https://futuredev-cdn.artz.zone/courseware/../secret",
        ] {
            assert!(
                checked_source_url(value, hosts).is_err(),
                "accepted {value}"
            );
        }
    }

    #[test]
    fn unsupported_incomplete_and_oversized_coursewares_cannot_be_imported() {
        let mut item = json!({"id":24,"name":"习题册.pdf","fileUrl":"https://futuredev-cdn.artz.zone/courseware/pdf/test.pdf","fileSize":1024});
        let source = import_source(&item, Environment::Development).unwrap();
        assert_eq!(source.kind, "document");
        assert_eq!(source.mime, "application/pdf");
        item["status"] = json!("UPLOADING");
        assert!(import_source(&item, Environment::Development).is_err());
        item["status"] = Value::Null;
        item["fileSize"] = json!(101 * 1024 * 1024);
        assert!(import_source(&item, Environment::Development).is_err());
        item["fileSize"] = json!(1024);
        item["fileUrl"] = json!("https://futuredev-cdn.artz.zone/courseware/ppt/test.ppt");
        assert!(import_source(&item, Environment::Development).is_err());
    }
}
