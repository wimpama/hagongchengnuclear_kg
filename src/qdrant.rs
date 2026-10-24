use std::time::Duration;

use crate::error::ApiError;
use anyhow::{Context, Result};
use reqwest::{header, Client, StatusCode, Url};
use serde::Serialize;
use serde_json::{json, Value};

use crate::config::QdrantConfig;

#[derive(Clone)]
pub struct QdrantAdapter {
    client: Client,
    config: QdrantConfig,
}

#[derive(Debug, Serialize)]
pub struct QdrantHealth {
    pub ok: bool,
    pub http_url: String,
    pub grpc_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl QdrantAdapter {
    async fn request(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Value,
    ) -> Result<Value, ApiError> {
        let url = self
            .config
            .http_url
            .join(path.trim_start_matches('/'))
            .map_err(|_| ApiError::internal())?;
        let response = self
            .client
            .request(method, url)
            .json(&body)
            .send()
            .await
            .map_err(|_| ApiError::unavailable("向量数据库连接失败"))?;
        if !response.status().is_success() {
            return Err(ApiError::unavailable(format!(
                "向量数据库返回 HTTP {}",
                response.status().as_u16()
            )));
        }
        response
            .json()
            .await
            .map_err(|_| ApiError::unavailable("向量数据库响应无效"))
    }
    pub async fn ensure_collection(&self, name: &str, dimension: usize) -> Result<(), ApiError> {
        let response = self
            .get(&format!("collections/{name}"))
            .await
            .map_err(|_| ApiError::unavailable("向量数据库不可用"))?;
        if response.status() == StatusCode::NOT_FOUND {
            self.request(reqwest::Method::PUT,&format!("collections/{name}"),json!({"vectors":{"size":dimension,"distance":"Cosine","on_disk":true},"hnsw_config":{"on_disk":true},"optimizers_config":{"indexing_threshold":1000}})).await?;
            for key in ["course_id", "node_id", "revision_id", "asset_id"] {
                self.request(
                    reqwest::Method::PUT,
                    &format!("collections/{name}/index?wait=true"),
                    json!({"field_name":key,"field_schema":"keyword"}),
                )
                .await?;
            }
        } else if !response.status().is_success() {
            return Err(ApiError::unavailable("向量 collection 检查失败"));
        } else {
            let body: Value = response
                .json()
                .await
                .map_err(|_| ApiError::unavailable("向量 collection 结构无效"))?;
            if body
                .pointer("/result/config/params/vectors/size")
                .and_then(Value::as_u64)
                != Some(dimension as u64)
                || body
                    .pointer("/result/config/params/vectors/distance")
                    .and_then(Value::as_str)
                    != Some("Cosine")
            {
                return Err(ApiError::unavailable(
                    "现有向量 collection 的维度或距离类型不匹配",
                ));
            }
        }
        Ok(())
    }
    pub async fn upsert(&self, name: &str, point: Value) -> Result<(), ApiError> {
        self.request(
            reqwest::Method::PUT,
            &format!("collections/{name}/points?wait=true"),
            json!({"points":[point]}),
        )
        .await?;
        Ok(())
    }
    pub async fn verify(&self, name: &str, ids: &[String]) -> Result<bool, ApiError> {
        let result = self
            .request(
                reqwest::Method::POST,
                &format!("collections/{name}/points"),
                json!({"ids":ids,"with_payload":false,"with_vector":false}),
            )
            .await?;
        Ok(result
            .get("result")
            .and_then(Value::as_array)
            .is_some_and(|v| v.len() == ids.len()))
    }
    pub async fn query(
        &self,
        name: &str,
        vector: Vec<f32>,
        nodes: &[String],
        revisions: &[String],
        course: &str,
    ) -> Result<Vec<Value>, ApiError> {
        if nodes.is_empty() || revisions.is_empty() {
            return Ok(vec![]);
        }
        let filter = json!({"must":[{"key":"course_id","match":{"value":course}},{"key":"node_id","match":{"any":nodes}},{"key":"revision_id","match":{"any":revisions}}]});
        let result=self.request(reqwest::Method::POST,&format!("collections/{name}/points/query"),json!({"query":vector,"filter":filter,"limit":12,"with_payload":true,"with_vector":false})).await?;
        result
            .pointer("/result/points")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| ApiError::unavailable("向量查询结构无效"))
    }
    pub async fn delete_asset(&self, name: &str, asset: &str) -> Result<(), ApiError> {
        self.request(
            reqwest::Method::POST,
            &format!("collections/{name}/points/delete?wait=true"),
            json!({"filter":{"must":[{"key":"asset_id","match":{"value":asset}}]}}),
        )
        .await?;
        Ok(())
    }
    pub fn new(config: QdrantConfig) -> Result<Self> {
        let mut headers = header::HeaderMap::new();
        if let Some(api_key) = &config.api_key {
            let value = header::HeaderValue::from_str(api_key)
                .context("QDRANT_API_KEY contains invalid header characters")?;
            headers.insert("api-key", value);
        }

        let client = Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(60))
            .build()
            .context("build Qdrant HTTP client")?;
        Ok(Self { client, config })
    }

    pub async fn health(&self) -> QdrantHealth {
        let health_url = health_url(&self.config.http_url);
        match self
            .client
            .get(health_url)
            .timeout(Duration::from_secs(4))
            .send()
            .await
        {
            Ok(response) if response.status().is_success() => QdrantHealth {
                ok: true,
                http_url: redact_url(&self.config.http_url),
                grpc_url: self.config.grpc_url.clone(),
                error: None,
            },
            Ok(response) => QdrantHealth {
                ok: false,
                http_url: redact_url(&self.config.http_url),
                grpc_url: self.config.grpc_url.clone(),
                error: Some(format!("Qdrant returned HTTP {}", response.status())),
            },
            Err(error) => QdrantHealth {
                ok: false,
                http_url: redact_url(&self.config.http_url),
                grpc_url: self.config.grpc_url.clone(),
                error: Some(error.to_string()),
            },
        }
    }

    /// Thin REST request helper used by the future collection/index adapter.
    /// Keeping it here makes Qdrant the only module that knows transport details.
    pub async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let url = self
            .config
            .http_url
            .join(path.trim_start_matches('/'))
            .context("invalid Qdrant REST path")?;
        let response = self.client.get(url).send().await.context("Qdrant GET")?;
        Ok(response)
    }
}

fn health_url(base: &Url) -> Url {
    base.join("healthz").unwrap_or_else(|_| base.clone())
}

fn redact_url(url: &Url) -> String {
    let mut sanitized = url.clone();
    sanitized.set_username("").ok();
    sanitized.set_password(None).ok();
    sanitized.set_query(None);
    sanitized.to_string()
}

#[allow(dead_code)]
fn _status_is_healthy(status: StatusCode) -> bool {
    status.is_success()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    #[ignore = "requires a real isolated Qdrant via KB_TEST_QDRANT_URL"]
    async fn live_ann_filters_and_idempotency() {
        let url = std::env::var("KB_TEST_QDRANT_URL").expect("KB_TEST_QDRANT_URL");
        let q = QdrantAdapter::new(QdrantConfig {
            http_url: Url::parse(&url).unwrap(),
            grpc_url: String::new(),
            api_key: None,
        })
        .unwrap();
        let name = format!("kb_test_{}", uuid::Uuid::new_v4().simple());
        q.ensure_collection(&name, 4).await.unwrap();
        q.ensure_collection(&name, 4).await.unwrap();
        assert!(q.ensure_collection(&name, 8).await.is_err());
        let mut ids = Vec::new();
        for (course, node, revision, part) in [
            ("course", "leaf", "current", "allowed"),
            ("other", "leaf", "current", "wrong-course"),
            ("course", "other", "current", "wrong-node"),
            ("course", "leaf", "draft", "wrong-revision"),
        ] {
            let id = uuid::Uuid::new_v4().to_string();
            ids.push(id.clone());
            let point = json!({"id":id,"vector":[1.,0.1,0.,0.],"payload":{"course_id":course,"node_id":node,"revision_id":revision,"asset_id":"asset","part_id":part}});
            q.upsert(&name, point.clone()).await.unwrap();
            q.upsert(&name, point).await.unwrap();
        }
        assert!(q.verify(&name, &ids).await.unwrap());
        let hits = q
            .query(
                &name,
                vec![1., 0., 0., 0.],
                &["leaf".into()],
                &["current".into()],
                "course",
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0]["payload"]["part_id"], "allowed");
        assert!(q
            .query(
                &name,
                vec![1., 0., 0., 0.],
                &[],
                &["current".into()],
                "course"
            )
            .await
            .unwrap()
            .is_empty());
        q.delete_asset(&name, "asset").await.unwrap();
        assert!(q
            .query(
                &name,
                vec![1., 0., 0., 0.],
                &["leaf".into()],
                &["current".into()],
                "course"
            )
            .await
            .unwrap()
            .is_empty());
        let cleanup = q
            .request(
                reqwest::Method::DELETE,
                &format!("collections/{name}"),
                json!({}),
            )
            .await;
        if cfg!(windows) && cleanup.is_err() {
            eprintln!(
                "Windows Qdrant collection cleanup failed; isolated empty test collection retained"
            );
        } else {
            cleanup.unwrap();
        }
    }
}
