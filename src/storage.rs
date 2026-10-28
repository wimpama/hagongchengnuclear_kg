//! Private object storage for course materials and student question files.
//!
//! Local disk remains a working cache because document extraction,
//! ffmpeg and multimodal request construction need a filesystem path. When
//! OSS is the durable source and the service signs
//! all requests server-side; the browser never receives OSS credentials or a
//! public object URL.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::Utc;
use hmac::{Hmac, Mac};
use reqwest::{header, Client, Method, StatusCode, Url};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;

use crate::{config::StorageConfig, error::ApiError};

type HmacSha256 = Hmac<Sha256>;

#[derive(Clone)]
pub struct BlobStore {
    config: StorageConfig,
    client: Client,
}

impl BlobStore {
    pub fn new(config: StorageConfig) -> Self {
        Self {
            config,
            client: Client::builder()
                .connect_timeout(Duration::from_secs(15))
                .read_timeout(Duration::from_secs(60))
                .timeout(Duration::from_secs(900))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("build OSS HTTP client"),
        }
    }

    pub fn local_dir(&self) -> &Path {
        match &self.config {
            #[cfg(test)]
            StorageConfig::Local { local_dir } => local_dir,
            StorageConfig::Oss { local_dir, .. } => local_dir,
        }
    }

    pub fn is_remote(&self) -> bool {
        matches!(self.config, StorageConfig::Oss { .. })
    }

    /// Microsoft Office Viewer fetches the source itself, so it cannot use a
    /// localhost URL or our bearer token. Authorize the published workbook
    /// first, then grant GET on exactly one object for ten minutes.
    pub fn office_read_url(&self, key: &str) -> Result<String, ApiError> {
        self.office_read_url_at(key, Utc::now())
    }

    fn office_read_url_at(
        &self,
        key: &str,
        now: chrono::DateTime<Utc>,
    ) -> Result<String, ApiError> {
        validate_key(key)?;
        if key.starts_with("question-") || !key.ends_with(".xlsx") && !key.ends_with(".xls") {
            return Err(ApiError::bad_request("仅支持课程 Excel 文件在线预览"));
        }
        let (endpoint, region, bucket, access_key_id, access_key_secret) = match &self.config {
            StorageConfig::Oss {
                endpoint,
                region,
                bucket,
                access_key_id,
                access_key_secret,
                ..
            } => (endpoint, region, bucket, access_key_id, access_key_secret),
            #[cfg(test)]
            StorageConfig::Local { .. } => {
                return Err(ApiError::unavailable("Office 在线预览需要 OSS 原文件"))
            }
        };
        let object_key = self.remote_key(key);
        let mut url = object_url(endpoint, bucket, &object_key);
        if url.scheme() != "https" {
            return Err(ApiError::unavailable("Office 在线预览需要 HTTPS OSS 端点"));
        }
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let date = now.format("%Y%m%d").to_string();
        let scope = format!("{date}/{region}/oss/aliyun_v4_request");
        let host = url.host_str().ok_or_else(ApiError::internal)?;
        let host = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host.to_owned(),
        };
        let parameters = std::collections::BTreeMap::from([
            ("response-content-disposition", "inline".to_owned()),
            ("x-oss-additional-headers", "host".to_owned()),
            ("x-oss-credential", format!("{access_key_id}/{scope}")),
            ("x-oss-date", timestamp.clone()),
            ("x-oss-expires", "600".to_owned()),
            ("x-oss-signature-version", "OSS4-HMAC-SHA256".to_owned()),
        ]);
        let query = parameters
            .iter()
            .map(|(name, value)| format!("{name}={}", oss_query_encode(value)))
            .collect::<Vec<_>>()
            .join("&");
        let canonical =
            format!("GET\n/{bucket}/{object_key}\n{query}\nhost:{host}\n\nhost\nUNSIGNED-PAYLOAD");
        let string_to_sign = format!(
            "OSS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            hex_sha256(canonical.as_bytes())
        );
        let date_key = hmac_sha256(format!("aliyun_v4{access_key_secret}").as_bytes(), &date);
        let region_key = hmac_sha256(&date_key, region);
        let service_key = hmac_sha256(&region_key, "oss");
        let signing_key = hmac_sha256(&service_key, "aliyun_v4_request");
        let signature = hex_bytes(&hmac_sha256(&signing_key, &string_to_sign));
        url.set_query(Some(&format!("{query}&x-oss-signature={signature}")));
        Ok(url.to_string())
    }

    /// Serve authorized media straight from OSS, including video byte ranges.
    /// No object URL or credential is exposed, and no preview is persisted locally.
    pub async fn media_response(
        &self,
        key: &str,
        mime: &str,
        method: Method,
        headers: axum::http::HeaderMap,
    ) -> Result<axum::response::Response, ApiError> {
        validate_key(key)?;
        #[cfg(test)]
        if !self.is_remote() {
            let file = tokio::fs::File::open(self.local_path(key))
                .await
                .map_err(|_| ApiError::not_found("媒体文件不存在"))?;
            return crate::tickets::file_response(file, mime, method, headers).await;
        }
        let mut request = self.signed_request(method.clone(), key, "");
        if let Some(range) = headers.get(header::RANGE) {
            let value = range
                .to_str()
                .map_err(|_| ApiError::bad_request("无效媒体范围"))?;
            if value.len() > 100 || !value.starts_with("bytes=") || value.contains(',') {
                return Err(ApiError::bad_request("仅支持单段媒体范围"));
            }
            request = request.header(header::RANGE, value);
        }
        let upstream = request
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OSS 媒体读取失败，请重试"))?;
        let status = upstream.status();
        if status == StatusCode::NOT_FOUND {
            return Err(ApiError::not_found("媒体文件不存在"));
        }
        if !status.is_success() && status != StatusCode::RANGE_NOT_SATISFIABLE {
            return Err(response_error(upstream, "预览").await);
        }
        let mut response = axum::response::Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CONTENT_DISPOSITION, "inline")
            .header(header::ACCEPT_RANGES, "bytes");
        for name in [header::CONTENT_LENGTH, header::CONTENT_RANGE] {
            if let Some(value) = upstream.headers().get(&name) {
                response = response.header(name, value);
            }
        }
        let body = if method == Method::HEAD {
            axum::body::Body::empty()
        } else {
            axum::body::Body::from_stream(upstream.bytes_stream())
        };
        let mut response = response.body(body).map_err(|_| ApiError::internal())?;
        crate::tickets::add_security_headers(&mut response);
        Ok(response)
    }

    /// Upload an already-written local file to the configured durable store.
    pub async fn put_file(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
    ) -> Result<(), ApiError> {
        self.put_file_with_policy(key, path, content_type, false)
            .await
    }

    async fn put_file_with_policy(
        &self,
        key: &str,
        path: &Path,
        content_type: &str,
        forbid_overwrite: bool,
    ) -> Result<(), ApiError> {
        validate_key(key)?;
        if !self.is_remote() {
            return Ok(());
        }
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| ApiError::internal())?;
        let length = file
            .metadata()
            .await
            .map_err(|_| ApiError::internal())?
            .len();
        let response = self
            .signed_request_at(Method::PUT, key, content_type, Utc::now(), forbid_overwrite)
            .header(header::CONTENT_LENGTH, length)
            .body(file)
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OSS 上传资料失败，请检查网络或稍后重试"))?;
        check_response(response, "上传").await
    }

    /// Persist bytes to the local working cache and, in OSS mode, to OSS.
    pub async fn put_bytes(
        &self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), ApiError> {
        validate_key(key)?;
        tokio::fs::create_dir_all(self.local_dir())
            .await
            .map_err(|_| ApiError::internal())?;
        tokio::fs::write(self.local_path(key), bytes)
            .await
            .map_err(|_| ApiError::internal())?;
        if self.is_remote() {
            self.put_bytes_remote(key, bytes, content_type).await?;
        }
        Ok(())
    }

    /// Make a remote object available as a local cache file and return its
    /// path. Existing local files are reused, which keeps indexing fast.
    pub async fn ensure_local(&self, key: &str) -> Result<PathBuf, ApiError> {
        validate_key(key)?;
        let path = self.local_path(key);
        if let Ok(metadata) = tokio::fs::metadata(&path).await {
            if metadata.is_file() && metadata.len() > 0 {
                // Refresh the processing cache's idle time before an extractor
                // or model request uses it. Originals always remain in OSS.
                if let Ok(file) = tokio::fs::File::open(&path).await {
                    let _ = file.into_std().await.set_times(
                        std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()),
                    );
                }
                return Ok(path);
            }
        }
        if !self.is_remote() {
            return Err(ApiError::not_found("资料原件不存在"));
        }

        tokio::fs::create_dir_all(self.local_dir())
            .await
            .map_err(|_| ApiError::internal())?;
        let response = self
            .signed_request(Method::GET, key, "")
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OSS 读取资料失败，请检查网络或稍后重试"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err(ApiError::not_found("OSS 中不存在该资料"));
        }
        if !response.status().is_success() {
            return Err(response_error(response, "读取").await);
        }

        let temporary = self
            .local_dir()
            .join(format!(".{}.download", Uuid::new_v4()));
        let result = async {
            let mut file = tokio::fs::File::create(&temporary)
                .await
                .map_err(|_| ApiError::internal())?;
            let mut response = response;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| ApiError::unavailable("OSS 下载资料中断，请重试"))?
            {
                file.write_all(&chunk)
                    .await
                    .map_err(|_| ApiError::internal())?;
            }
            file.flush().await.map_err(|_| ApiError::internal())?;
            drop(file);
            if tokio::fs::rename(&temporary, &path).await.is_err() {
                // Windows cannot replace a file that another concurrent
                // download has already installed. Both downloads are complete.
                let installed = tokio::fs::metadata(&path)
                    .await
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() > 0);
                if !installed {
                    return Err(ApiError::internal());
                }
                let _ = tokio::fs::remove_file(&temporary).await;
            }
            Ok::<(), ApiError>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result.map(|_| path)
    }

    /// Remove a local cache and its remote object. OSS 404 is treated as
    /// already deleted so cleanup jobs are idempotent.
    pub async fn delete(&self, key: &str) -> Result<(), ApiError> {
        validate_key(key)?;
        if self.is_remote() {
            let response = self
                .signed_request(Method::DELETE, key, "")
                .send()
                .await
                .map_err(|_| ApiError::unavailable("OSS 删除资料失败，可重试清理任务"))?;
            if response.status() != StatusCode::NOT_FOUND {
                check_response(response, "删除").await?;
            }
        }
        match tokio::fs::remove_file(self.local_path(key)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(ApiError::unavailable("本地缓存回收失败，可重试清理任务")),
        }
        Ok(())
    }

    /// Copy a legacy referenced file without overwriting an unrelated object.
    /// Verify the bytes at the remote source, not the existing local cache.
    pub async fn migrate_file(&self, key: &str, content_type: &str) -> Result<bool, ApiError> {
        validate_key(key)?;
        if !self.is_remote() {
            return Err(ApiError::bad_request("迁移需要启用 OSS"));
        }
        let mut file = tokio::fs::File::open(self.local_path(key))
            .await
            .map_err(|_| ApiError::not_found("迁移所需的本地原件缺失"))?;
        let mut digest = Sha256::new();
        let mut buffer = vec![0; 64 * 1024];
        loop {
            let count = file
                .read(&mut buffer)
                .await
                .map_err(|_| ApiError::internal())?;
            if count == 0 {
                break;
            }
            digest.update(&buffer[..count]);
        }
        let expected = hex_bytes(&digest.finalize());
        if let Some(actual) = self.remote_sha256(key).await? {
            if actual != expected {
                return Err(ApiError::bad_request(
                    "OSS 已有同名但内容不同的对象，迁移已停止，未覆盖",
                ));
            }
            return Ok(false);
        }
        self.put_file_with_policy(key, &self.local_path(key), content_type, true)
            .await?;
        if self.remote_sha256(key).await?.as_deref() != Some(expected.as_str()) {
            return Err(ApiError::unavailable(
                "OSS 迁移后的内容校验失败，请保留本地原件并重试",
            ));
        }
        Ok(true)
    }

    async fn remote_sha256(&self, key: &str) -> Result<Option<String>, ApiError> {
        let mut response = self
            .signed_request(Method::GET, key, "")
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OSS 迁移校验读取失败"))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(response_error(response, "校验").await);
        }
        let mut digest = Sha256::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| ApiError::unavailable("OSS 迁移校验中断"))?
        {
            digest.update(&chunk);
        }
        Ok(Some(hex_bytes(&digest.finalize())))
    }

    async fn put_bytes_remote(
        &self,
        key: &str,
        bytes: &[u8],
        content_type: &str,
    ) -> Result<(), ApiError> {
        let response = self
            .signed_request(Method::PUT, key, content_type)
            .body(bytes.to_vec())
            .send()
            .await
            .map_err(|_| ApiError::unavailable("OSS 上传资料失败，请检查网络或稍后重试"))?;
        check_response(response, "上传").await
    }

    fn local_path(&self, key: &str) -> PathBuf {
        self.local_dir().join(key)
    }

    fn signed_request(
        &self,
        method: Method,
        key: &str,
        content_type: &str,
    ) -> reqwest::RequestBuilder {
        self.signed_request_at(method, key, content_type, Utc::now(), false)
    }

    fn signed_request_at(
        &self,
        method: Method,
        key: &str,
        content_type: &str,
        now: chrono::DateTime<Utc>,
        forbid_overwrite: bool,
    ) -> reqwest::RequestBuilder {
        let (endpoint, region, bucket, access_key_id, access_key_secret) = match &self.config {
            StorageConfig::Oss {
                endpoint,
                region,
                bucket,
                access_key_id,
                access_key_secret,
                ..
            } => (endpoint, region, bucket, access_key_id, access_key_secret),
            #[cfg(test)]
            StorageConfig::Local { .. } => unreachable!("local unit test fixture cannot sign OSS"),
        };

        let object_key = self.remote_key(key);
        let url = object_url(endpoint, bucket, &object_key);
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let sign_date = now.format("%Y%m%d").to_string();
        // OSS V4 includes the bucket here even with virtual-hosted URLs.
        // Keys/prefixes are restricted to URL-safe ASCII below/in config.rs.
        let canonical_uri = format!("/{bucket}/{object_key}");
        let host = url
            .host_str()
            .expect("OSS endpoint must contain a host")
            .to_owned();
        let host_value = match url.port() {
            Some(port) => format!("{host}:{port}"),
            None => host,
        };
        let mut signed_headers = vec![
            ("host", host_value.as_str()),
            ("x-oss-content-sha256", "UNSIGNED-PAYLOAD"),
            ("x-oss-date", timestamp.as_str()),
        ];
        if !content_type.is_empty() {
            signed_headers.push(("content-type", content_type.trim()));
        }
        if forbid_overwrite {
            signed_headers.push(("x-oss-forbid-overwrite", "true"));
        }
        signed_headers.sort_by(|left, right| left.0.cmp(right.0));
        let canonical_headers = signed_headers
            .iter()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<String>();
        let canonical_request = format!(
            "{}\n{}\n\n{}\nhost\nUNSIGNED-PAYLOAD",
            method.as_str(),
            canonical_uri,
            canonical_headers
        );
        let scope = format!("{sign_date}/{region}/oss/aliyun_v4_request");
        let string_to_sign = format!(
            "OSS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            hex_sha256(canonical_request.as_bytes())
        );
        let date_key = hmac_sha256(
            format!("aliyun_v4{access_key_secret}").as_bytes(),
            &sign_date,
        );
        let region_key = hmac_sha256(&date_key, region);
        let service_key = hmac_sha256(&region_key, "oss");
        let signing_key = hmac_sha256(&service_key, "aliyun_v4_request");
        let signature = hex_bytes(&hmac_sha256(&signing_key, &string_to_sign));
        let authorization = format!(
            "OSS4-HMAC-SHA256 Credential={access_key_id}/{scope},AdditionalHeaders=host,Signature={signature}"
        );
        let mut request = self
            .client
            .request(method, url)
            .header(header::HOST, host_value)
            .header("x-oss-date", timestamp)
            .header("x-oss-content-sha256", "UNSIGNED-PAYLOAD")
            .header(header::AUTHORIZATION, authorization);
        if forbid_overwrite {
            request = request.header("x-oss-forbid-overwrite", "true");
        }
        if content_type.is_empty() {
            request
        } else {
            request.header(header::CONTENT_TYPE, content_type)
        }
    }

    fn remote_key(&self, key: &str) -> String {
        match &self.config {
            #[cfg(test)]
            StorageConfig::Local { .. } => key.to_owned(),
            StorageConfig::Oss { prefix, .. } => format!(
                "{prefix}{}/{key}",
                if key.starts_with("question-") {
                    "user_upload"
                } else {
                    "doc"
                }
            ),
        }
    }
}

/// Disposable processing files are removed after 24 hours without use.
/// This only inspects direct regular files inside the configured cache; it
/// neither follows symlinks nor traverses legacy originals/other directories.
pub fn start_cache_cleanup(store: BlobStore) {
    tokio::spawn(async move {
        loop {
            let _ = clean_idle_cache(&store, Duration::from_secs(24 * 3600)).await;
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
}

async fn clean_idle_cache(store: &BlobStore, age: Duration) -> std::io::Result<()> {
    let mut entries = tokio::fs::read_dir(store.local_dir()).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if validate_key(name).is_err() && !(name.starts_with('.') && name.ends_with(".download")) {
            continue;
        }
        if entry
            .metadata()
            .await?
            .modified()?
            .elapsed()
            .is_ok_and(|idle| idle > age)
        {
            let _ = tokio::fs::remove_file(entry.path()).await;
        }
    }
    Ok(())
}

/// Persist intent before deleting a database reference. Failed OSS deletes
/// remain here across restarts and are retried by the background reaper.
pub async fn drain_cleanup(state: &crate::state::AppState) -> Result<usize, ApiError> {
    let keys: Vec<String> = sqlx::query_scalar(
        "SELECT storage_key FROM kb_blob_cleanup ORDER BY attempts,created_at LIMIT 100",
    )
    .fetch_all(&state.db)
    .await?;
    let mut remaining = 0;
    for key in keys {
        if state.storage.delete(&key).await.is_ok() {
            sqlx::query("DELETE FROM kb_blob_cleanup WHERE storage_key=?")
                .bind(&key)
                .execute(&state.db)
                .await?;
        } else {
            remaining += 1;
            sqlx::query("UPDATE kb_blob_cleanup SET attempts=attempts+1 WHERE storage_key=?")
                .bind(&key)
                .execute(&state.db)
                .await?;
        }
    }
    Ok(remaining)
}

fn hmac_sha256(key: &[u8], value: &str) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts every non-empty key");
    mac.update(value.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

async fn check_response(response: reqwest::Response, operation: &str) -> Result<(), ApiError> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(response_error(response, operation).await)
    }
}

async fn response_error(mut response: reqwest::Response, operation: &str) -> ApiError {
    let status = response.status().as_u16();
    // Never include an entire OSS error XML: it can echo signed requests.
    let mut body = Vec::new();
    while let Ok(Some(chunk)) = response.chunk().await {
        let remaining = 8192 - body.len();
        body.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
        if body.len() == 8192 {
            break;
        }
    }
    let xml = String::from_utf8_lossy(&body);
    let code = safe_xml_field(&xml, "Code").unwrap_or("Unknown");
    let request_id = safe_xml_field(&xml, "RequestId").unwrap_or("unavailable");
    ApiError::unavailable(format!(
        "OSS {operation}资料返回 HTTP {status}（{code}，请求编号 {request_id}）"
    ))
}

fn safe_xml_field<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    let start = format!("<{name}>");
    let end = format!("</{name}>");
    let value = xml.split_once(&start)?.1.split_once(&end)?.0;
    (!value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.".contains(&byte)))
    .then_some(value)
}

fn hex_sha256(value: &[u8]) -> String {
    hex_bytes(&Sha256::digest(value))
}

fn oss_query_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
                char::from(byte).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn hex_bytes(value: &[u8]) -> String {
    value.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn object_url(endpoint: &Url, bucket: &str, key: &str) -> Url {
    let mut url = endpoint.clone();
    let endpoint_host = url
        .host_str()
        .expect("OSS endpoint must contain a host")
        .to_owned();
    let bucket_prefix = format!("{bucket}.");
    if !endpoint_host.starts_with(&bucket_prefix) {
        url.set_host(Some(&format!("{bucket}.{endpoint_host}")))
            .expect("validated OSS endpoint host");
    }
    url.set_path(&format!("/{key}"));
    url
}

pub(crate) fn validate_key(key: &str) -> Result<(), ApiError> {
    if key.is_empty()
        || key.len() > 255
        || key.starts_with('.')
        || key
            .bytes()
            .any(|byte| !(byte.is_ascii_alphanumeric() || b"_-.".contains(&byte)))
    {
        return Err(ApiError::bad_request("无效的资料存储键"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::StorageConfig;

    fn oss_store() -> BlobStore {
        BlobStore::new(StorageConfig::Oss {
            local_dir: PathBuf::from("data/uploads"),
            endpoint: Box::new(Url::parse("https://oss-cn-beijing.aliyuncs.com").unwrap()),
            region: "cn-beijing".into(),
            bucket: "futuredev".into(),
            access_key_id: "test-ak".into(),
            access_key_secret: "test-sk".into(),
            prefix: "knowledge/".into(),
        })
    }

    #[test]
    fn remote_key_and_object_url_are_stable() {
        let store = oss_store();
        assert_eq!(store.remote_key("abc.png"), "knowledge/doc/abc.png");
        assert_eq!(
            store.remote_key("question-abc.png"),
            "knowledge/user_upload/question-abc.png"
        );
        let url = object_url(
            &Url::parse("https://oss-cn-beijing.aliyuncs.com").unwrap(),
            "futuredev",
            "knowledge/doc/abc.png",
        );
        assert_eq!(
            url.as_str(),
            "https://futuredev.oss-cn-beijing.aliyuncs.com/knowledge/doc/abc.png"
        );
    }

    #[test]
    fn v4_request_contains_required_signed_headers() {
        let request = oss_store()
            .signed_request(Method::PUT, "abc.png", "image/png")
            .build()
            .unwrap();
        assert_eq!(
            request.headers().get("x-oss-content-sha256").unwrap(),
            "UNSIGNED-PAYLOAD"
        );
        assert!(request.headers().get("x-oss-date").is_some());
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(authorization.starts_with("OSS4-HMAC-SHA256 Credential=test-ak/"));
        assert!(authorization.contains(",AdditionalHeaders=host,Signature="));
    }

    #[test]
    fn v4_signature_matches_independently_calculated_fixture() {
        let request = oss_store()
            .signed_request_at(
                Method::PUT,
                "abc.png",
                "image/png",
                chrono::DateTime::parse_from_rfc3339("2025-04-11T06:41:24Z")
                    .unwrap()
                    .with_timezone(&Utc),
                false,
            )
            .build()
            .unwrap();
        assert_eq!(request.headers().get(header::AUTHORIZATION).unwrap(),
            "OSS4-HMAC-SHA256 Credential=test-ak/20250411/cn-beijing/oss/aliyun_v4_request,AdditionalHeaders=host,Signature=6c73dd687d57196f277e9ab11ad08fc767960447b79094b178109754ee78c0e9");
    }

    #[test]
    fn office_url_signs_only_one_workbook_for_ten_minutes() {
        let now = chrono::DateTime::parse_from_rfc3339("2025-04-11T09:27:56Z")
            .unwrap()
            .with_timezone(&Utc);
        let url =
            Url::parse(&oss_store().office_read_url_at("preview.xlsx", now).unwrap()).unwrap();
        assert_eq!(
            url.host_str(),
            Some("futuredev.oss-cn-beijing.aliyuncs.com")
        );
        assert_eq!(url.path(), "/knowledge/doc/preview.xlsx");
        let query = url
            .query_pairs()
            .collect::<std::collections::HashMap<_, _>>();
        assert_eq!(query["x-oss-expires"], "600");
        assert_eq!(query["response-content-disposition"], "inline");
        // Independently calculated with Python hashlib/hmac and urllib.quote.
        assert_eq!(
            query["x-oss-signature"],
            "113bf4f8e1d3ebff763cbcdde923a61ca004d25da92eb6aca5f17406003b7d20"
        );
        assert!(!url.as_str().contains("test-sk"));
        for key in [
            "question-private.xlsx",
            "picture.png",
            "../file.xlsx",
            "https://other.test/a.xlsx",
        ] {
            assert!(oss_store().office_read_url(key).is_err());
        }
    }

    #[test]
    fn invalid_keys_cannot_escape_upload_cache() {
        for key in [
            "",
            ".env",
            "a/b",
            r"a\\b",
            "a\0b",
            "a b",
            "a%20b",
            "汉字.png",
        ] {
            assert!(validate_key(key).is_err(), "accepted unsafe key {key:?}");
        }
        assert!(validate_key("safe-file.mp4").is_ok());
    }

    #[tokio::test]
    #[ignore = "uses both configured OSS buckets, creating only unique selftest objects"]
    async fn live_oss_buckets_and_user_upload_paths_are_isolated() {
        let mut stores = Vec::new();
        let id = Uuid::new_v4();
        let keys = [
            format!("selftest-{id}.txt"),
            format!("question-selftest-{id}.txt"),
        ];
        for environment in [
            crate::config::Environment::Development,
            crate::config::Environment::Production,
        ] {
            let mut config = crate::config::AppConfig::for_environment(environment)
                .unwrap()
                .storage;
            let directory =
                std::env::temp_dir().join(format!("kb-oss-{}-{id}", environment.name()));
            match &mut config {
                StorageConfig::Oss {
                    local_dir, bucket, ..
                } => {
                    assert_eq!(
                        bucket,
                        if environment == crate::config::Environment::Development {
                            "futuredev"
                        } else {
                            "futureprod"
                        }
                    );
                    *local_dir = directory;
                }
                _ => panic!("OSS configuration required"),
            }
            stores.push((BlobStore::new(config), environment.name().as_bytes()));
        }
        let result = async {
            for (store, payload) in &stores {
                for key in &keys {
                    store.put_bytes(key, payload, "text/plain").await?;
                }
            }
            for (store, payload) in &stores {
                for key in &keys {
                    if store.remote_sha256(key).await? != Some(hex_sha256(payload)) {
                        return Err(ApiError::unavailable("OSS site isolation failed"));
                    }
                }
            }
            Ok::<(), ApiError>(())
        }
        .await;
        for (store, _) in &stores {
            for key in &keys {
                store
                    .delete(key)
                    .await
                    .expect("delete only this test's unique object");
            }
            let _ = tokio::fs::remove_dir(store.local_dir()).await;
        }
        result.expect(
            "development/production and course/question OSS paths must be writable and isolated",
        );
    }

    #[tokio::test]
    #[ignore = "uses configured futuredev OSS credentials and network"]
    async fn live_oss_roundtrip() {
        let mut config = crate::config::AppConfig::from_env()
            .expect("OSS test configuration")
            .storage;
        let temporary = std::env::temp_dir().join(format!("kb-oss-test-{}", Uuid::new_v4()));
        match &mut config {
            StorageConfig::Oss {
                local_dir, bucket, ..
            } => {
                assert_eq!(
                    bucket, "futuredev",
                    "live tests may only use the development bucket"
                );
                *local_dir = temporary.clone();
            }
            _ => panic!("configure KB_STORAGE_BACKEND=oss for this ignored test"),
        }
        let store = BlobStore::new(config);
        let key = format!("selftest-{}.txt", Uuid::new_v4());
        let payload = b"knowledge storage roundtrip";
        let result = async {
            store.put_bytes(&key, payload, "text/plain").await?;
            tokio::fs::remove_file(store.local_path(&key))
                .await
                .unwrap();
            let restored = store.ensure_local(&key).await?;
            assert_eq!(tokio::fs::read(&restored).await.unwrap(), payload);
            // Exercise the same streaming path used for video derivatives.
            store.put_file(&key, &restored, "text/plain").await?;
            tokio::fs::remove_file(&restored).await.unwrap();
            let restored = store.ensure_local(&key).await?;
            assert_eq!(tokio::fs::read(restored).await.unwrap(), payload);
            tokio::fs::remove_file(store.local_path(&key))
                .await
                .unwrap();
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(header::RANGE, "bytes=0-8".parse().unwrap());
            let response = store
                .media_response(&key, "text/plain", Method::GET, headers)
                .await?;
            assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "private, no-store, max-age=0"
            );
            assert_eq!(
                axum::body::to_bytes(response.into_body(), 100)
                    .await
                    .unwrap(),
                &payload[..9]
            );
            assert!(
                !store.local_path(&key).exists(),
                "preview must stream directly from OSS"
            );
            Ok::<(), ApiError>(())
        }
        .await;
        let cleanup = store.delete(&key).await;
        let _ = tokio::fs::remove_dir(&temporary).await;
        result.expect("OSS write and cache restore must succeed");
        cleanup.expect("remove only the test object");
        assert_eq!(
            store.ensure_local(&key).await.unwrap_err().status,
            StatusCode::NOT_FOUND
        );
        let _ = tokio::fs::remove_dir(&temporary).await;
    }
}
