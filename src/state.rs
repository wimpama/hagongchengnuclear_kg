use sqlx::SqlitePool;
use std::{
    path::PathBuf,
    sync::{atomic::AtomicBool, Arc},
};
use tokio::sync::Mutex;

use crate::{auth::AuthUpstream, config::AppConfig, qdrant::QdrantAdapter, storage::BlobStore};

#[derive(Clone)]
pub struct AppState {
    pub environment: crate::config::Environment,
    pub workflow_lock: Arc<Mutex<()>>,
    pub review_lock: Arc<Mutex<()>>,
    pub learning_lock: Arc<Mutex<()>>,
    pub worker_running: Arc<AtomicBool>,
    pub db: SqlitePool,
    pub ai_log_dir: PathBuf,
    pub qdrant: QdrantAdapter,
    pub auth: AuthUpstream,
    pub storage: BlobStore,
}

impl AppState {
    pub fn new(
        config: AppConfig,
        db: SqlitePool,
        qdrant: QdrantAdapter,
        auth: AuthUpstream,
    ) -> Self {
        let ai_log_dir = config.ai_log_dir.clone();
        let storage = BlobStore::new(config.storage);
        Self {
            environment: config.environment,
            workflow_lock: Arc::new(Mutex::new(())),
            review_lock: Arc::new(Mutex::new(())),
            learning_lock: Arc::new(Mutex::new(())),
            worker_running: Arc::new(AtomicBool::new(false)),
            db,
            ai_log_dir,
            qdrant,
            auth,
            storage,
        }
    }
}
