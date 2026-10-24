use crate::{
    error::ApiError,
    models::{self, ModelConfig},
};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};

pub async fn embed(config: &ModelConfig, input: Vec<Value>) -> Result<Vec<f32>, ApiError> {
    if !config.enabled {
        return Err(ApiError::unavailable("Embedding 未启用"));
    }
    let key = config
        .api_key
        .as_ref()
        .ok_or_else(|| ApiError::unavailable("请配置火山方舟 Embedding 密钥"))?;
    let mut body = json!({"model":config.model,"input":input,"dimensions":config.dimension,"encoding_format":"float"});
    if !config.instructions.is_empty() {
        body["instructions"] = json!(config.instructions)
    }
    let response = models::client()?
        .post(&config.base_url)
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .map_err(|_| ApiError::unavailable("方舟向量化连接失败"))?;
    if !response.status().is_success() {
        return Err(ApiError::unavailable(format!(
            "方舟向量化返回 HTTP {}",
            response.status().as_u16()
        )));
    }
    let result: Value = response
        .json()
        .await
        .map_err(|_| ApiError::unavailable("方舟响应不是有效 JSON"))?;
    parse_embedding(&result, config.dimension)
}
pub fn parse_embedding(body: &Value, dimension: usize) -> Result<Vec<f32>, ApiError> {
    let values = body
        .pointer("/data/embedding")
        .and_then(Value::as_array)
        .ok_or_else(|| ApiError::unavailable("方舟未返回 data.embedding"))?;
    if values.len() != dimension {
        return Err(ApiError::unavailable("Embedding 返回维度与配置不一致"));
    }
    let vector = values
        .iter()
        .map(|v| {
            v.as_f64()
                .map(|n| n as f32)
                .filter(|n| n.is_finite())
                .ok_or_else(|| ApiError::unavailable("向量包含无效数值"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if vector.iter().all(|v| *v == 0.0) {
        return Err(ApiError::unavailable("方舟返回零向量"));
    }
    Ok(vector)
}
pub async fn media_input(
    root: &std::path::Path,
    storage_key: &str,
    modality: &str,
    fps: f64,
) -> Result<Value, ApiError> {
    // Storage keys are generated UUID filenames, never URLs or user paths.
    if storage_key.contains(['/', '\\']) || storage_key.starts_with('.') {
        return Err(ApiError::bad_request("媒体路径无效"));
    }
    let path = root.join(storage_key);
    let size = tokio::fs::metadata(&path)
        .await
        .map_err(|_| ApiError::not_found("媒体文件不存在"))?
        .len();
    let max = if modality == "video" {
        50 * 1024 * 1024
    } else {
        20 * 1024 * 1024
    };
    if size > max {
        return Err(ApiError::bad_request(
            "媒体片段超过模型输入限制，请分段或缩小",
        ));
    }
    let mime = match path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "jpg" | "jpeg" => "image/jpeg",
        "avi" => "video/avi",
        "mov" => "video/quicktime",
        _ => "video/mp4",
    };
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|_| ApiError::internal())?;
    let url = format!("data:{mime};base64,{}", STANDARD.encode(bytes));
    Ok(if modality == "video" {
        json!({"type":"video_url","video_url":{"url":url,"fps":fps}})
    } else {
        json!({"type":"image_url","image_url":{"url":url}})
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_wrong_shape_and_dimension() {
        assert!(parse_embedding(&json!({"data":[{"embedding":[1.0,2.0]}]}), 2).is_err());
        assert!(parse_embedding(&json!({"data":{"embedding":[1.0]}}), 2).is_err());
        assert!(parse_embedding(&json!({"data":{"embedding":[0.0,0.0]}}), 2).is_err());
        assert_eq!(
            parse_embedding(&json!({"data":{"embedding":[1.0,2.0]}}), 2).unwrap(),
            vec![1.0, 2.0]
        );
    }
}
