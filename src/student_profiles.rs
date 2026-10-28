//! Basic student information comes only from the authorized account service.
use crate::{error::ApiError, state::AppState};
use futures_util::{stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use std::{
    collections::{HashMap, HashSet},
    time::Duration,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudentProfile {
    pub student_no: Option<String>,
    pub name: Option<String>,
    pub class_name: Option<String>,
    pub college: Option<String>,
    pub major: Option<String>,
    pub grade: Option<String>,
}

fn field(raw: &Value, key: &str) -> Option<String> {
    raw.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
        .map(|value| value.chars().take(160).collect())
}

pub fn parse(raw: &Value) -> StudentProfile {
    StudentProfile {
        student_no: field(raw, "studentNo"),
        name: field(raw, "realName"),
        class_name: field(raw, "className"),
        college: field(raw, "college"),
        major: field(raw, "major"),
        grade: field(raw, "grade"),
    }
}

pub async fn store(
    db: &SqlitePool,
    user_id: &str,
    profile: &StudentProfile,
) -> Result<(), ApiError> {
    sqlx::query("INSERT INTO kb_student_profiles(user_id,student_no,name,class_name,college,major,grade) VALUES(?,?,?,?,?,?,?) ON CONFLICT(user_id) DO UPDATE SET student_no=COALESCE(excluded.student_no,student_no),name=COALESCE(excluded.name,name),class_name=COALESCE(excluded.class_name,class_name),college=COALESCE(excluded.college,college),major=COALESCE(excluded.major,major),grade=COALESCE(excluded.grade,grade)")
        .bind(user_id).bind(&profile.student_no).bind(&profile.name).bind(&profile.class_name)
        .bind(&profile.college).bind(&profile.major).bind(&profile.grade).execute(db).await?;
    Ok(())
}

/// Called only after teacher authorization and scope checks. Cached profiles
/// also cover older conversations without requiring the student to log in again.
pub async fn resolve(
    state: &AppState,
    token: &str,
    user_ids: &[String],
) -> Result<HashMap<String, StudentProfile>, ApiError> {
    let ids: HashSet<_> = user_ids.iter().cloned().collect();
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let rows = sqlx::query("SELECT *,COALESCE(checked_at>datetime('now','-30 minutes'),0) AS fresh FROM kb_student_profiles WHERE user_id IN (SELECT value FROM json_each(?))")
        .bind(serde_json::json!(ids).to_string()).fetch_all(&state.db).await?;
    let mut profiles = HashMap::new();
    let mut fresh = HashSet::new();
    for row in rows {
        let id: String = row.get("user_id");
        if row.get::<bool, _>("fresh") {
            fresh.insert(id.clone());
        }
        profiles.insert(
            id,
            StudentProfile {
                student_no: row.get("student_no"),
                name: row.get("name"),
                class_name: row.get("class_name"),
                college: row.get("college"),
                major: row.get("major"),
                grade: row.get("grade"),
            },
        );
    }
    let missing: Vec<_> = ids.into_iter().filter(|id| !fresh.contains(id)).collect();
    let fetched = stream::iter(missing.into_iter().map(|id| async move {
        // Account IDs never become arbitrary paths or query strings.
        let result = if !id.is_empty()
            && id.len() <= 128
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            tokio::time::timeout(
                Duration::from_secs(4),
                state
                    .auth
                    .get::<Value>(token, &format!("/admin/users/{id}")),
            )
            .await
            .ok()
            .and_then(Result::ok)
        } else {
            None
        };
        let profile = result
            .filter(|raw| {
                let actual_id = raw.get("id").map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| value.to_string())
                });
                actual_id.as_deref() == Some(id.as_str())
                    && raw
                        .get("role")
                        .and_then(Value::as_str)
                        .is_some_and(|r| r.eq_ignore_ascii_case("student"))
            })
            .map(|raw| parse(&raw))
            .unwrap_or_default();
        (id, profile)
    }))
    .buffer_unordered(6)
    .collect::<Vec<_>>()
    .await;
    for (id, profile) in fetched {
        // Cache a failed lookup briefly as well; keep any previously verified
        // fields instead of replacing them with guessed or empty information.
        store(&state.db, &id, &profile).await?;
        sqlx::query("UPDATE kb_student_profiles SET checked_at=CURRENT_TIMESTAMP WHERE user_id=?")
            .bind(&id)
            .execute(&state.db)
            .await?;
        let old = profiles.entry(id).or_insert_with(StudentProfile::default);
        if profile.student_no.is_some() {
            old.student_no = profile.student_no;
        }
        if profile.name.is_some() {
            old.name = profile.name;
        }
        if profile.class_name.is_some() {
            old.class_name = profile.class_name;
        }
        if profile.college.is_some() {
            old.college = profile.college;
        }
        if profile.major.is_some() {
            old.major = profile.major;
        }
        if profile.grade.is_some() {
            old.grade = profile.grade;
        }
    }
    Ok(profiles)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        extract::Path,
        http::{header, HeaderMap, StatusCode},
        routing::get,
        Json, Router,
    };
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    #[test]
    fn only_authoritative_basic_fields_are_used() {
        let profile = parse(
            &serde_json::json!({"id":6,"username":"not-a-name","name":"unverified","realName":" 张同学 ","studentNo":" 20260001 ","college":"核科学学院","className":"一班","grade":"2026","major":"核工程"}),
        );
        assert_eq!(profile.name.as_deref(), Some("张同学"));
        assert_eq!(profile.student_no.as_deref(), Some("20260001"));
        assert_eq!(
            parse(&serde_json::json!({"id":6,"username":"someone"})),
            StudentProfile::default()
        );
    }

    #[tokio::test]
    async fn partial_login_is_enriched_once_and_lookups_cannot_invent_or_replace_students() {
        let mut state = crate::db::test_state().await;
        // A partial auth response must not mark the admin lookup fresh.
        store(
            &state.db,
            "s1",
            &parse(&serde_json::json!({"realName":"张同学"})),
        )
        .await
        .unwrap();
        store(
            &state.db,
            "s3",
            &parse(&serde_json::json!({"studentNo":"20260003"})),
        )
        .await
        .unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let app = Router::new().route("/admin/users/{id}", get(move |Path(id): Path<String>, headers: HeaderMap| {
            seen.fetch_add(1, Ordering::SeqCst);
            async move {
                assert_eq!(headers[header::AUTHORIZATION], "Bearer teacher-only");
                match id.as_str() {
                    "s1" => (StatusCode::OK, Json(serde_json::json!({"id":"s1","role":"STUDENT","realName":"张同学","studentNo":"20260001","className":"核工一班","college":"核科学学院","major":"核工程","grade":"2026"}))),
                    "s2" => (StatusCode::OK, Json(serde_json::json!({"id":"other-student","role":"STUDENT","studentNo":"must-not-leak"}))),
                    _ => (StatusCode::FORBIDDEN, Json(serde_json::json!({}))),
                }
            }
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        state.auth = crate::auth::AuthUpstream::new(crate::config::AuthConfig {
            base_url: Some(format!("http://{address}").parse().unwrap()),
            me_path: "/auth/me".into(),
        })
        .unwrap();
        let ids = vec![
            "s1".into(),
            "s1".into(),
            "s2".into(),
            "s3".into(),
            "../admin".into(),
        ];
        let profiles = resolve(&state, "teacher-only", &ids).await.unwrap();
        assert_eq!(requests.load(Ordering::SeqCst), 3);
        assert_eq!(profiles["s1"].student_no.as_deref(), Some("20260001"));
        assert_eq!(profiles["s1"].college.as_deref(), Some("核科学学院"));
        assert_eq!(profiles["s2"], StudentProfile::default());
        assert_eq!(profiles["s3"].student_no.as_deref(), Some("20260003"));
        assert_eq!(profiles["../admin"], StudentProfile::default());
        store(&state.db, "s1", &StudentProfile::default())
            .await
            .unwrap();
        let again = resolve(&state, "teacher-only", &ids).await.unwrap();
        assert_eq!(again, profiles);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "profile refresh must be cached"
        );
        let other_tenant = crate::db::test_state().await;
        assert_eq!(
            sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM kb_student_profiles")
                .fetch_one(&other_tenant.db)
                .await
                .unwrap(),
            0
        );
        server.abort();
    }
}
