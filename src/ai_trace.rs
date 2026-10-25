//! Per-answer trace files for diagnosing retrieval, generation and validation.
//!
//! Trace files intentionally redact media URLs and data URLs. They keep the
//! text that was sent to the model, retrieved evidence, model responses and
//! stage timings so an administrator can reproduce a slow or rejected answer
//! without exposing storage tickets or provider credentials.

use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};
use std::path::PathBuf;

const MAX_VALUE_BYTES: usize = 400_000;

pub struct AiTrace {
    path: PathBuf,
    document: Value,
}

impl AiTrace {
    pub fn new(
        directory: PathBuf,
        started_at: DateTime<Utc>,
        session_name: &str,
        user_id: &str,
        conversation_id: &str,
        question: &str,
        scope: Value,
    ) -> Self {
        let stamp = started_at.format("%Y%m%dT%H%M%S%.3fZ");
        let filename = format!(
            "{}_{}_{}_{}.json",
            stamp,
            safe_component(session_name, "session"),
            safe_component(user_id, "user"),
            safe_component(conversation_id, "conversation"),
        );
        let path = directory.join(filename);
        let document = json!({
            "traceVersion": 1,
            "startedAt": started_at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "sessionName": session_name,
            "userId": user_id,
            "conversationId": conversation_id,
            "question": question,
            "scope": scope,
            "stages": [],
            "result": Value::Null,
        });
        Self { path, document }
    }

    pub fn record(
        &mut self,
        stage: &str,
        input: Option<Value>,
        output: Option<Value>,
        error: Option<&str>,
    ) {
        let mut event = Map::new();
        event.insert("stage".into(), Value::String(stage.into()));
        if let Some(input) = input {
            event.insert("input".into(), bound(redact(input)));
        }
        if let Some(output) = output {
            event.insert("output".into(), bound(redact(output)));
        }
        if let Some(error) = error {
            event.insert("error".into(), Value::String(error.into()));
        }
        if let Some(stages) = self
            .document
            .get_mut("stages")
            .and_then(Value::as_array_mut)
        {
            stages.push(Value::Object(event));
        }
    }

    pub fn finish(&mut self, outcome: &str, elapsed_ms: u64, diagnostics: Value, result: Value) {
        self.document["result"] = json!({
            "outcome": outcome,
            "elapsedMs": elapsed_ms,
            "diagnostics": diagnostics,
            "answer": bound(redact(result)),
        });
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub async fn write(self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let text = serde_json::to_vec_pretty(&self.document).map_err(std::io::Error::other)?;
        tokio::fs::write(self.path, text).await
    }
}

fn redact(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.into_iter().map(redact).collect()),
        Value::Object(object) => {
            let mut result = Map::new();
            for (key, value) in object {
                if matches!(
                    key.as_str(),
                    "url" | "imageUrl" | "image_url" | "input_image" | "inputImage"
                ) {
                    result.insert(key, Value::String("[media URL redacted]".into()));
                } else if value.as_str().is_some_and(|text| text.starts_with("data:")) {
                    result.insert(key, Value::String("[data URL redacted]".into()));
                } else {
                    result.insert(key, redact(value));
                }
            }
            Value::Object(result)
        }
        other => other,
    }
}

fn bound(value: Value) -> Value {
    let Ok(serialized) = serde_json::to_string(&value) else {
        return json!({"truncated": true});
    };
    if serialized.len() <= MAX_VALUE_BYTES {
        return value;
    }
    let preview: String = serialized.chars().take(16_000).collect();
    json!({"truncated": true, "bytes": serialized.len(), "preview": preview})
}

fn safe_component(value: &str, fallback: &str) -> String {
    let mut result = String::new();
    let mut last_separator = false;
    for character in value.chars() {
        let allowed = character.is_alphanumeric() || matches!(character, '-' | '_' | '.');
        if allowed {
            result.push(character);
            last_separator = false;
        } else if !last_separator {
            result.push('_');
            last_separator = true;
        }
        if result.chars().count() >= 64 {
            break;
        }
    }
    let result = result.trim_matches(['_', '.', ' ']).to_owned();
    if result.is_empty() {
        fallback.into()
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_name_keeps_session_and_user_without_path_separators() {
        let trace = AiTrace::new(
            PathBuf::from("data/logs"),
            Utc::now(),
            "自持链式/裂变反应",
            "student/42",
            "conversation-1",
            "问题",
            json!({"title":"课程"}),
        );
        let name = trace.path().file_name().unwrap().to_string_lossy();
        assert!(name.contains("自持链式_裂变反应"));
        assert!(name.contains("student_42"));
        assert!(!name.contains('/'));
    }

    #[test]
    fn trace_redacts_media_urls_and_bounds_large_values() {
        let mut trace = AiTrace::new(
            PathBuf::from("data/logs"),
            Utc::now(),
            "会话",
            "user",
            "conversation",
            "问题",
            json!({}),
        );
        trace.record(
            "generation",
            Some(json!({
                "messages":[{"role":"user","content":"文字"}],
                "image_url":{"url":"https://signed.example/token"},
                "data":"data:image/png;base64,abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz"
            })),
            Some(Value::String("回答".into())),
            None,
        );
        let stages = trace.document["stages"].as_array().unwrap();
        let input = &stages[0]["input"];
        assert_eq!(input["image_url"].as_str(), Some("[media URL redacted]"));
        assert_eq!(input["data"].as_str(), Some("[data URL redacted]"));
    }
}
