//! One HTTP service dispatches to isolated development/production application states.
use crate::{config::Environment, error::ApiError};
use axum::{
    extract::Request,
    http::{header, HeaderMap, Method},
    response::{IntoResponse, Response},
    Router,
};
use tower::ServiceExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

fn host_environment(host: &str) -> Option<Environment> {
    match host {
        "future.artz.zone" => Some(Environment::Production),
        "futuredev.artz.zone" | "localhost" | "127.0.0.1" | "[::1]" | "::1" => {
            Some(Environment::Development)
        }
        _ => None,
    }
}

fn source_environment(headers: &HeaderMap) -> Result<Environment, ApiError> {
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<axum::http::uri::Authority>().ok())
        .ok_or_else(|| ApiError::bad_request("缺少有效站点来源"))?;
    let host_env =
        host_environment(host.host()).ok_or_else(|| ApiError::forbidden("未配置的知识库站点"))?;
    if let Some(origin) = headers.get(header::ORIGIN) {
        let origin = origin
            .to_str()
            .ok()
            .and_then(|value| reqwest::Url::parse(value).ok())
            .filter(|url| {
                matches!(url.scheme(), "https" | "http")
                    && url.username().is_empty()
                    && url.password().is_none()
            })
            .ok_or_else(|| ApiError::forbidden("无效站点来源"))?;
        let origin_host = origin
            .host_str()
            .ok_or_else(|| ApiError::forbidden("无效站点来源"))?;
        let origin_env = host_environment(origin_host)
            .ok_or_else(|| ApiError::forbidden("未配置的知识库站点"))?;
        if origin_env != host_env {
            return Err(ApiError::forbidden("请求站点与来源不一致"));
        }
    }
    // Never trust a browser-provided bucket, environment flag or forwarded-host.
    // Public reverse proxies must preserve Host. Each state authenticates against
    // its own existing business API and reads only its own database/collections.
    Ok(host_env)
}

pub fn router(development: Router, production: Router) -> Router {
    // Direct browser requests use bearer tokens, including cross-origin local
    // development. Only echo origins accepted by the same site/environment
    // checks as actual requests; preflights must not reach authenticated routes.
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|_, request| {
            source_environment(&request.headers).is_ok()
        }))
        .allow_methods([
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            header::ACCEPT,
            header::RANGE,
        ])
        .expose_headers([
            header::ACCEPT_RANGES,
            header::CONTENT_RANGE,
            header::CONTENT_LENGTH,
        ])
        .max_age(std::time::Duration::from_secs(600));
    Router::new()
        .fallback(move |request: Request| {
            let development = development.clone();
            let production = production.clone();
            async move {
                let app = match source_environment(request.headers()) {
                    Ok(Environment::Development) => development,
                    Ok(Environment::Production) => production,
                    Err(error) => return error.into_response(),
                };
                let response: Response = match app.oneshot(request).await {
                    Ok(response) => response,
                    Err(error) => match error {},
                };
                response
            }
        })
        .layer(cors)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn direct_browser_preflight_allows_tokens_only_for_matching_sites() {
        let route = || Router::new().fallback(|| async { axum::http::StatusCode::UNAUTHORIZED });
        let app = router(route(), route());
        for (host, origin, allowed) in [
            ("127.0.0.1:8080", "http://localhost:5173", true),
            ("127.0.0.1:8080", "http://127.0.0.1:5173", true),
            ("futuredev.artz.zone", "http://localhost:5173", true),
            ("futuredev.artz.zone", "http://127.0.0.1:5173", true),
            ("futuredev.artz.zone", "https://futuredev.artz.zone", true),
            ("future.artz.zone", "https://future.artz.zone", true),
            ("future.artz.zone", "http://localhost:5173", false),
            ("127.0.0.1:8080", "https://future.artz.zone", false),
            ("127.0.0.1:8080", "https://evil.test", false),
            ("127.0.0.1:8080", "null", false),
            (
                "future.artz.zone.evil.test",
                "https://future.artz.zone",
                false,
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::OPTIONS)
                        .uri("/test")
                        .header(header::HOST, host)
                        .header(header::ORIGIN, origin)
                        .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                        .header(
                            header::ACCESS_CONTROL_REQUEST_HEADERS,
                            "authorization,content-type",
                        )
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            assert_eq!(
                response
                    .headers()
                    .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                    .map(|v| v.to_str().unwrap()),
                allowed.then_some(origin)
            );
            assert!(response
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
                .is_none());
            if allowed {
                let headers = response.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS]
                    .to_str()
                    .unwrap();
                assert!(headers.contains("authorization") && headers.contains("content-type"));
                let methods = response.headers()[header::ACCESS_CONTROL_ALLOW_METHODS]
                    .to_str()
                    .unwrap();
                for method in ["GET", "POST", "PUT", "PATCH", "DELETE"] {
                    assert!(methods.contains(method));
                }
                assert!(response.headers()[header::VARY]
                    .to_str()
                    .unwrap()
                    .contains("origin"));
            }
        }
    }

    #[tokio::test]
    async fn direct_browser_responses_keep_errors_streams_and_range_media_readable() {
        use axum::{
            http::StatusCode,
            response::sse::{Event, Sse},
        };
        let tenant = || {
            Router::new()
            .route("/protected", axum::routing::get(|| async { StatusCode::UNAUTHORIZED }))
            .route("/events", axum::routing::get(|| async {
                Sse::new(async_stream::stream! {
                    yield Ok::<_, std::convert::Infallible>(Event::default().event("delta").data("hello"));
                    // CORS must not wait for the stream to finish before its first event.
                    std::future::pending::<()>().await;
                })
            }))
            .route("/media", axum::routing::get(|| async {
                (StatusCode::PARTIAL_CONTENT, [
                    (header::CONTENT_RANGE, "bytes 0-4/10"),
                    (header::ACCEPT_RANGES, "bytes"),
                ], "hello")
            }))
        };
        let app = router(tenant(), tenant());
        for (path, status) in [
            ("/protected", StatusCode::UNAUTHORIZED),
            ("/events", StatusCode::OK),
            ("/media", StatusCode::PARTIAL_CONTENT),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::HOST, "127.0.0.1:8080")
                        .header(header::ORIGIN, "http://localhost:5173")
                        .header(header::RANGE, "bytes=0-4")
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
                "http://localhost:5173"
            );
            if path == "/media" {
                assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-4/10");
                assert!(response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS]
                    .to_str()
                    .unwrap()
                    .contains("content-range"));
            }
            if path == "/events" {
                let mut stream = response.into_body().into_data_stream();
                let first = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    std::future::poll_fn(|cx| {
                        futures_core::Stream::poll_next(std::pin::Pin::new(&mut stream), cx)
                    }),
                )
                .await
                .unwrap()
                .unwrap()
                .unwrap();
                assert!(std::str::from_utf8(&first).unwrap().contains("hello"));
            }
        }
        let rejected = app
            .oneshot(
                Request::builder()
                    .uri("/protected")
                    .header(header::HOST, "future.artz.zone")
                    .header(header::ORIGIN, "http://localhost:5173")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
        assert!(rejected
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none());
    }

    #[test]
    fn exact_hosts_choose_bucket_environment_and_conflicts_are_rejected() {
        for (host, expected) in [
            ("127.0.0.1:8080", Environment::Development),
            ("localhost:5173", Environment::Development),
            ("futuredev.artz.zone", Environment::Development),
            ("future.artz.zone", Environment::Production),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::HOST, host.parse().unwrap());
            assert_eq!(source_environment(&headers).unwrap(), expected);
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "future.artz.zone".parse().unwrap());
        headers.insert(
            header::ORIGIN,
            "https://futuredev.artz.zone".parse().unwrap(),
        );
        assert!(source_environment(&headers).is_err());
        headers.remove(header::ORIGIN);
        headers.insert(header::HOST, "future.artz.zone.evil.test".parse().unwrap());
        assert!(source_environment(&headers).is_err());
    }
    #[tokio::test]
    async fn same_url_dispatches_to_separate_application_states() {
        let app = router(
            Router::new().route("/test", axum::routing::get(|| async { "dev" })),
            Router::new().route("/test", axum::routing::get(|| async { "prod" })),
        );
        for (host, expected) in [("localhost", "dev"), ("future.artz.zone", "prod")] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/test")
                        .header(header::HOST, host)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 20)
                    .await
                    .unwrap(),
                expected
            );
        }
    }

    #[tokio::test]
    async fn local_and_futuredev_share_sqlite_records_while_production_is_isolated() {
        use axum::{extract::State, Json};
        use serde_json::{json, Value};
        use sqlx::SqlitePool;

        // Use the real schema and identical user/resource IDs in two temporary
        // SQLite files. No existing database, OSS resource or network is used.
        async fn seed(pool: &SqlitePool, marker: &str) {
            sqlx::migrate!("./migrations").run(pool).await.unwrap();
            sqlx::raw_sql(
                "INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('parent','course',1,'parent');
                 INSERT INTO kb_course_nodes(node_id,course_id,parent_node_id,level,title) VALUES('node','course','parent',2,'node');
                 INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,created_by)
                 VALUES('asset','node','text','initial','knowledge/doc/test.txt',1,'teacher');
                 INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id,title)
                 VALUES('conversation','student','course','level2','node','initial');
                 INSERT INTO kb_messages(message_id,conversation_id,role,content_json)
                 VALUES('message','conversation','user','initial');
                 INSERT INTO kb_question_attachments(attachment_id,conversation_id,user_id,original_name,mime_type,storage_key,byte_size)
                 VALUES('attachment','conversation','student','initial','text/plain','knowledge/user_upload/test.txt',1);",
            )
            .execute(pool)
            .await
            .unwrap();
            update_records(State(pool.clone()), Json(marker.to_owned())).await;
        }
        async fn update_records(State(pool): State<SqlitePool>, Json(marker): Json<String>) {
            let mut tx = pool.begin().await.unwrap();
            for statement in [
                "UPDATE kb_assets SET original_name=? WHERE asset_id='asset'",
                "UPDATE kb_conversations SET title=? WHERE conversation_id='conversation'",
                "UPDATE kb_messages SET content_json=? WHERE message_id='message'",
                "UPDATE kb_question_attachments SET original_name=? WHERE attachment_id='attachment'",
            ] {
                sqlx::query(statement).bind(&marker).execute(&mut *tx).await.unwrap();
            }
            tx.commit().await.unwrap();
        }
        async fn read_records(State(pool): State<SqlitePool>) -> Json<Value> {
            let mut records = Vec::new();
            for statement in [
                "SELECT original_name FROM kb_assets WHERE asset_id='asset'",
                "SELECT title FROM kb_conversations WHERE conversation_id='conversation'",
                "SELECT content_json FROM kb_messages WHERE message_id='message'",
                "SELECT original_name FROM kb_question_attachments WHERE attachment_id='attachment'",
            ] {
                records.push(sqlx::query_scalar::<_, String>(statement).fetch_one(&pool).await.unwrap());
            }
            Json(json!(records))
        }
        async fn read_at(app: &Router, host: &str, origin: &str) -> Value {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/records")
                        .header(header::HOST, host)
                        .header(header::ORIGIN, origin)
                        .body(axum::body::Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
            )
            .unwrap()
        }

        let directory =
            std::env::temp_dir().join(format!("kb-tenant-test-{}", uuid::Uuid::new_v4()));
        let db_url = |name: &str| {
            format!(
                "sqlite://{}/{}.sqlite?mode=rwc",
                directory.to_string_lossy().replace('\\', "/"),
                name
            )
        };
        let dev = crate::db::connect(&db_url("dev")).await.unwrap();
        let prod = crate::db::connect(&db_url("prod")).await.unwrap();
        seed(&dev, "dev").await;
        seed(&prod, "prod").await;
        let tenant = |pool| {
            Router::new()
                .route(
                    "/records",
                    axum::routing::get(read_records).post(update_records),
                )
                .with_state(pool)
        };
        let app = router(tenant(dev.clone()), tenant(prod.clone()));
        for (host, origin) in [
            ("localhost:8080", "http://localhost:5173"),
            ("127.0.0.1:8080", "http://127.0.0.1:5173"),
            ("futuredev.artz.zone", "https://futuredev.artz.zone"),
            ("futuredev.artz.zone", "http://localhost:5173"),
        ] {
            assert_eq!(read_at(&app, host, origin).await, json!(vec!["dev"; 4]));
        }
        assert_eq!(
            read_at(&app, "future.artz.zone", "https://future.artz.zone").await,
            json!(vec!["prod"; 4])
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/records")
                    .header(header::HOST, "localhost:8080")
                    .header(header::ORIGIN, "http://localhost:5173")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(axum::body::Body::from("\"updated-locally\""))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            read_at(&app, "futuredev.artz.zone", "https://futuredev.artz.zone").await,
            json!(vec!["updated-locally"; 4])
        );
        assert_eq!(
            read_at(&app, "future.artz.zone", "https://future.artz.zone").await,
            json!(vec!["prod"; 4])
        );
        drop(app);
        dev.close().await;
        prod.close().await;
        std::fs::remove_dir_all(directory).unwrap();
    }
}
