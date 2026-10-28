use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::http::{header, HeaderMap, StatusCode};
use reqwest::{Client, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{config::AuthConfig, error::ApiError, state::AppState};

#[derive(Clone)]
pub struct AuthUpstream {
    client: Client,
    config: AuthConfig,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UserRole {
    Admin,
    Student,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuthenticatedUser {
    pub id: String,
    pub role: UserRole,
    #[serde(default)]
    pub profile: Option<crate::student_profiles::StudentProfile>,
}

#[derive(Debug, Error)]
pub enum AuthError {
    #[error("authorization token is missing")]
    MissingToken,
    #[error("authentication upstream is not configured")]
    NotConfigured,
    #[error("authentication upstream rejected the token")]
    Unauthorized,
    #[error("authentication upstream request failed")]
    Upstream,
    #[error("authentication upstream returned an invalid identity")]
    InvalidIdentity,
}

#[async_trait]
pub trait IdentityProvider: Send + Sync {
    async fn authenticate(&self, bearer_token: &str) -> Result<AuthenticatedUser, AuthError>;
}

impl AuthUpstream {
    pub fn new(config: AuthConfig) -> Result<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(3))
            .timeout(std::time::Duration::from_secs(15))
            .build()
            .context("build authentication upstream client")?;
        Ok(Self { client, config })
    }
    pub fn is_configured(&self) -> bool {
        self.config.base_url.is_some()
    }

    pub fn endpoint(&self, path: &str) -> Result<Url, ApiError> {
        let base = self
            .config
            .base_url
            .as_ref()
            .ok_or_else(|| ApiError::unavailable("尚未配置课程身份服务"))?;
        let mut base = base.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        let endpoint = base
            .join(path.trim_start_matches('/'))
            .map_err(|_| ApiError::upstream())?;
        // The Bearer token belongs only to the configured backend. Never follow
        // an accidentally absolute or cross-origin path supplied by an adapter.
        if endpoint.origin() != base.origin() {
            return Err(ApiError::upstream());
        }
        Ok(endpoint)
    }

    pub async fn get<T: DeserializeOwned>(&self, token: &str, path: &str) -> Result<T, ApiError> {
        let url = self.endpoint(path)?;
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| ApiError::upstream())?;
        match response.status() {
            StatusCode::UNAUTHORIZED => return Err(ApiError::unauthorized()),
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => {
                return Err(ApiError::forbidden("无权访问该课程范围"))
            }
            status if !status.is_success() => return Err(ApiError::upstream()),
            _ => {}
        }
        let raw: Value = response.json().await.map_err(|_| ApiError::upstream())?;
        let value = unwrap_envelope(raw)?;
        serde_json::from_value(value).map_err(|_| ApiError::upstream())
    }
}

#[async_trait]
impl IdentityProvider for AuthUpstream {
    async fn authenticate(&self, token: &str) -> Result<AuthenticatedUser, AuthError> {
        if token.trim().is_empty() {
            return Err(AuthError::MissingToken);
        }
        if !self.is_configured() {
            return Err(AuthError::NotConfigured);
        }
        let raw: Value = self
            .get(token, &self.config.me_path)
            .await
            .map_err(|error| {
                if error.status == StatusCode::UNAUTHORIZED || error.status == StatusCode::FORBIDDEN
                {
                    AuthError::Unauthorized
                } else {
                    AuthError::Upstream
                }
            })?;
        parse_identity(raw)
    }
}

pub async fn require_user(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, ApiError> {
    let token = bearer_token(headers).ok_or_else(ApiError::unauthorized)?;
    state
        .auth
        .authenticate(token)
        .await
        .map_err(|error| match error {
            AuthError::MissingToken | AuthError::Unauthorized | AuthError::InvalidIdentity => {
                ApiError::unauthorized()
            }
            AuthError::NotConfigured => ApiError::unavailable("尚未配置课程身份服务"),
            AuthError::Upstream => ApiError::upstream(),
        })
}

pub async fn require_admin(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<AuthenticatedUser, ApiError> {
    let user = require_user(state, headers).await?;
    if user.role != UserRole::Admin {
        return Err(ApiError::forbidden("需要管理员权限"));
    }
    Ok(user)
}

pub fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("bearer") && !token.trim().is_empty()).then_some(token.trim())
}

pub fn unwrap_envelope(raw: Value) -> Result<Value, ApiError> {
    if let Some(code) = raw.get("code") {
        if code.as_i64() != Some(0) {
            return Err(if matches!(code.as_i64(), Some(401 | 403)) {
                ApiError::unauthorized()
            } else {
                ApiError::upstream()
            });
        }
        return raw.get("data").cloned().ok_or_else(ApiError::upstream);
    }
    Ok(raw)
}

fn parse_identity(mut raw: Value) -> Result<AuthenticatedUser, AuthError> {
    if raw.get("id").is_none() {
        if let Some(user) = raw.get("user") {
            raw = user.clone();
        }
    }
    let id = match raw
        .get("id")
        .or_else(|| raw.get("userId"))
        .or_else(|| raw.get("user_id"))
    {
        Some(Value::String(id)) if !id.trim().is_empty() => id.clone(),
        Some(Value::Number(id)) if id.is_i64() || id.is_u64() => id.to_string(),
        _ => return Err(AuthError::InvalidIdentity),
    };
    if raw.get("active") == Some(&Value::Bool(false)) {
        return Err(AuthError::Unauthorized);
    }
    let role = match raw
        .get("role")
        .and_then(Value::as_str)
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("ADMIN") => UserRole::Admin,
        Some("STUDENT") => UserRole::Student,
        _ => return Err(AuthError::InvalidIdentity),
    };
    let profile = (role == UserRole::Student).then(|| crate::student_profiles::parse(&raw));
    Ok(AuthenticatedUser { id, role, profile })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn handles_numeric_id_and_api_envelope() {
        let raw = unwrap_envelope(json!({"code":0,"message":"ok","data":{"id":12,"role":"ADMIN"}}))
            .unwrap();
        let user = parse_identity(raw).unwrap();
        assert_eq!(user.id, "12");
        assert_eq!(user.role, UserRole::Admin);
    }
    #[test]
    fn rejects_failed_envelope_and_unknown_role() {
        assert!(unwrap_envelope(json!({"code":500,"data":{"id":"1","role":"ADMIN"}})).is_err());
        assert!(parse_identity(json!({"id":"1","role":"unknown"})).is_err());
        assert!(parse_identity(json!({"id":"1","role":"STUDENT","active":false})).is_err());
        assert_eq!(
            parse_identity(json!({"id":"s1","role":"student"}))
                .unwrap()
                .id,
            "s1"
        );
    }
    #[test]
    fn preserves_api_prefix_without_trailing_slash() {
        let upstream = AuthUpstream::new(AuthConfig {
            base_url: Some(Url::parse("https://example.com/api").unwrap()),
            me_path: "/auth/me".into(),
        })
        .unwrap();
        assert_eq!(
            upstream.endpoint("/auth/me").unwrap().as_str(),
            "https://example.com/api/auth/me"
        );
        assert!(upstream.endpoint("https://other.example/auth/me").is_err());
    }
}
