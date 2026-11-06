//! Short-lived, owner-scoped observations of an in-flight import. The upload
//! request remains authoritative; this is not a durable background job queue.
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

const TTL: Duration = Duration::from_secs(3600);
const MAX_ENTRIES: usize = 256;

#[derive(Default)]
pub struct ImportProgressRegistry(Mutex<HashMap<Uuid, Entry>>);

struct Entry {
    owner: String,
    _node: String,
    started: Instant,
    progress: ImportProgress,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProgress {
    state: &'static str,
    stage: &'static str,
    message: String,
    completed: u64,
    total: Option<u64>,
    unit: &'static str,
    elapsed_ms: u64,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ProgressQuery {
    pub progress_id: Option<Uuid>,
}

pub struct ImportReporter {
    registration: Option<(Arc<ImportProgressRegistry>, Uuid)>,
}

impl ImportProgressRegistry {
    pub fn begin(
        self: &Arc<Self>,
        id: Option<Uuid>,
        owner: &str,
        node: &str,
    ) -> Result<ImportReporter, ApiError> {
        if let Some(id) = id {
            let mut entries = self.0.lock().map_err(|_| ApiError::internal())?;
            entries.retain(|_, entry| entry.started.elapsed() < TTL);
            if entries.contains_key(&id) {
                return Err(ApiError::new(
                    axum::http::StatusCode::CONFLICT,
                    "progress_id_in_use",
                    "请使用新的导入进度标识重试",
                ));
            }
            if entries.len() >= MAX_ENTRIES {
                // Keep active imports observable; evict only the oldest terminal entry.
                let oldest = entries
                    .iter()
                    .filter(|(_, e)| e.progress.state != "running")
                    .min_by_key(|(_, e)| e.started)
                    .map(|(key, _)| *key);
                if let Some(key) = oldest {
                    entries.remove(&key);
                } else {
                    return Err(ApiError::unavailable("导入任务过多，请稍后重试"));
                }
            }
            entries.insert(
                id,
                Entry {
                    owner: owner.into(),
                    _node: node.into(),
                    started: Instant::now(),
                    progress: ImportProgress {
                        state: "running",
                        stage: "receiving",
                        message: "正在接收资料".into(),
                        completed: 0,
                        total: None,
                        unit: "bytes",
                        elapsed_ms: 0,
                    },
                },
            );
        }
        Ok(ImportReporter {
            registration: id.map(|id| (self.clone(), id)),
        })
    }

    pub fn get(&self, id: Uuid, owner: &str) -> Result<ImportProgress, ApiError> {
        let entries = self.0.lock().map_err(|_| ApiError::internal())?;
        let entry = entries
            .get(&id)
            .filter(|e| e.owner == owner && e.started.elapsed() < TTL)
            .ok_or_else(|| ApiError::not_found("导入进度暂不可用，请以导入请求结果为准"))?;
        let mut progress = entry.progress.clone();
        if progress.state == "running" {
            progress.elapsed_ms = elapsed(entry.started);
        }
        Ok(progress)
    }
}

fn elapsed(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

impl ImportReporter {
    pub fn stage(
        &self,
        stage: &'static str,
        message: &str,
        completed: u64,
        total: Option<u64>,
        unit: &'static str,
    ) {
        let Some((registry, id)) = &self.registration else {
            return;
        };
        if let Ok(mut entries) = registry.0.lock() {
            if let Some(entry) = entries
                .get_mut(id)
                .filter(|e| e.progress.state == "running")
            {
                entry.progress = ImportProgress {
                    state: "running",
                    stage,
                    message: message.into(),
                    completed,
                    total: total.filter(|&n| n > 0 && n >= completed),
                    unit,
                    elapsed_ms: elapsed(entry.started),
                };
            }
        }
    }

    pub fn finish<T>(&self, result: &Result<T, ApiError>) {
        match result {
            Ok(_) => self.terminal("succeeded", "资料已保存，等待教师确认和审核"),
            Err(error) => self.terminal("failed", &error.message),
        }
    }

    fn terminal(&self, state: &'static str, message: &str) {
        let Some((registry, id)) = &self.registration else {
            return;
        };
        if let Ok(mut entries) = registry.0.lock() {
            if let Some(entry) = entries
                .get_mut(id)
                .filter(|e| e.progress.state == "running")
            {
                entry.progress.state = state;
                entry.progress.elapsed_ms = elapsed(entry.started);
                entry.progress.message = message.into();
            }
        }
    }
}

impl Drop for ImportReporter {
    fn drop(&mut self) {
        self.terminal(
            "cancelled",
            "导入请求已中断，未完成的资料不会发布，请检查后重试",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn progress_is_owner_scoped_and_ids_cannot_replace_an_import() {
        let registry = Arc::new(ImportProgressRegistry::default());
        let id = Uuid::new_v4();
        let reporter = registry.begin(Some(id), "teacher-a", "node-a").unwrap();
        reporter.stage("rendering", "正在生成页面", 2, Some(40), "pages");
        let value = registry.get(id, "teacher-a").unwrap();
        assert_eq!(
            (value.completed, value.total, value.state),
            (2, Some(40), "running")
        );
        assert_eq!(
            registry.get(id, "teacher-b").unwrap_err().status,
            axum::http::StatusCode::NOT_FOUND
        );
        assert!(registry.begin(Some(id), "teacher-b", "node-b").is_err());
        assert_eq!(registry.0.lock().unwrap()[&id]._node, "node-a");
        let other_tenant = Arc::new(ImportProgressRegistry::default());
        assert!(other_tenant.get(id, "teacher-a").is_err());
    }
    #[test]
    fn failures_and_cancellation_are_terminal_and_do_not_turn_into_success() {
        let registry = Arc::new(ImportProgressRegistry::default());
        let cancelled = Uuid::new_v4();
        drop(registry.begin(Some(cancelled), "teacher", "node").unwrap());
        assert_eq!(
            registry.get(cancelled, "teacher").unwrap().state,
            "cancelled"
        );
        let failed = Uuid::new_v4();
        let reporter = registry.begin(Some(failed), "teacher", "node").unwrap();
        reporter.finish(&Err::<(), _>(ApiError::bad_request("第 6 页缺少预览图")));
        reporter.finish(&Ok::<_, ApiError>(()));
        reporter.stage("committing", "不能覆盖失败", 1, Some(1), "pages");
        drop(reporter);
        assert_eq!(
            registry.get(failed, "teacher").unwrap().message,
            "第 6 页缺少预览图"
        );
        let id = Uuid::new_v4();
        let reporter = registry.begin(Some(id), "teacher", "node").unwrap();
        reporter.finish(&Ok::<_, ApiError>(()));
        drop(reporter);
        assert_eq!(registry.get(id, "teacher").unwrap().state, "succeeded");
    }

    #[test]
    fn expired_entries_are_hidden_and_capacity_never_evicts_an_active_import() {
        let registry = Arc::new(ImportProgressRegistry::default());
        let active_id = Uuid::new_v4();
        let _active = registry.begin(Some(active_id), "teacher", "node").unwrap();
        let old = Uuid::new_v4();
        let reporter = registry.begin(Some(old), "teacher", "node").unwrap();
        reporter.finish(&Ok::<_, ApiError>(()));
        registry.0.lock().unwrap().get_mut(&old).unwrap().started = Instant::now() - TTL;
        assert!(registry.get(old, "teacher").is_err());
        for _ in 0..MAX_ENTRIES {
            drop(
                registry
                    .begin(Some(Uuid::new_v4()), "teacher", "node")
                    .unwrap(),
            );
        }
        assert_eq!(registry.0.lock().unwrap().len(), MAX_ENTRIES);
        assert_eq!(registry.get(active_id, "teacher").unwrap().state, "running");
        assert!(!registry.0.lock().unwrap().contains_key(&old));
    }
}
