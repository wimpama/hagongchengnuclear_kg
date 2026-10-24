use std::{path::Path, str::FromStr};

use anyhow::{Context, Result};
use sqlx::{sqlite::SqliteConnectOptions, SqlitePool};

pub async fn connect(database_url: &str) -> Result<SqlitePool> {
    create_parent_directory(database_url)?;

    let options = SqliteConnectOptions::from_str(database_url)
        .with_context(|| format!("invalid SQLite URL: {database_url}"))?
        .create_if_missing(true)
        .foreign_keys(true)
        .busy_timeout(std::time::Duration::from_secs(5));

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(8)
        .min_connections(1)
        .connect_with(options)
        .await
        .context("connect SQLite")?;

    // WAL allows the read-heavy chat path to run alongside publishing workers.
    sqlx::query("PRAGMA journal_mode = WAL")
        .execute(&pool)
        .await
        .context("enable SQLite WAL mode")?;
    sqlx::query("PRAGMA synchronous = NORMAL")
        .execute(&pool)
        .await
        .context("set SQLite synchronous mode")?;

    Ok(pool)
}

fn create_parent_directory(database_url: &str) -> Result<()> {
    // The default is a file URL. Do not attempt to interpret :memory: or URI query
    // parameters as filesystem paths; SQLite itself handles those forms.
    if database_url == ":memory:" || database_url.starts_with("sqlite::memory:") {
        return Ok(());
    }

    let raw = database_url
        .strip_prefix("sqlite://")
        .or_else(|| database_url.strip_prefix("sqlite:"))
        .unwrap_or(database_url);
    let path = raw.split('?').next().unwrap_or(raw);
    if path.is_empty() || path == ":memory:" {
        return Ok(());
    }

    if let Some(parent) = Path::new(path)
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create SQLite parent directory {}", parent.display()))?;
    }
    Ok(())
}

#[cfg(test)]
pub async fn test_state() -> crate::state::AppState {
    let db = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::migrate!("./migrations").run(&db).await.unwrap();
    let mut config = crate::config::AppConfig::test_config();
    config.storage = crate::config::StorageConfig::Local {
        local_dir: crate::ingest::uploads_dir(),
    };
    crate::state::AppState::new(
        config.clone(),
        db,
        crate::qdrant::QdrantAdapter::new(config.qdrant.clone()).unwrap(),
        crate::auth::AuthUpstream::new(config.auth.clone()).unwrap(),
    )
}

#[cfg(test)]
pub async fn test_published_state() -> (crate::state::AppState, crate::catalog::Scope) {
    let state = test_state().await;
    sqlx::raw_sql(
        "INSERT INTO kb_courses(course_id,title) VALUES('course','课程');
         INSERT INTO kb_course_nodes(node_id,course_id,level,title) VALUES('leaf','course',2,'人物图片');
         INSERT INTO kb_assets(asset_id,node_id,asset_type,original_name,storage_key,byte_size,mime_type,created_by) VALUES('a','leaf','image','人物.png','portrait.png',1,'image/png','admin');
         INSERT INTO kb_embedding_configs(embedding_config_id,provider,model,dimensions,config_fingerprint,created_by) VALUES('ec','ark','embedding',1024,'f1','admin');
         INSERT INTO kb_vector_collections(collection_id,course_id,embedding_config_id,generation,qdrant_collection,status) VALUES('vc','course','ec',1,'collection1','active');
         INSERT INTO kb_active_indexes(course_id,collection_id) VALUES('course','vc');
         INSERT INTO kb_asset_revisions(revision_id,asset_id,revision_number,source_sha256,parser_version,status,created_by,review_status,index_status,embedding_config_id,collection_id,expected_vector_count) VALUES('r','a',1,'hash','v1','published','admin','approved','verified','ec','vc',1);
         UPDATE kb_assets SET published_revision_id='r' WHERE asset_id='a';
         INSERT INTO kb_content_parts(part_id,revision_id,part_index,modality,content_text,content_sha256,evidence_status) VALUES('p','r',0,'image','李浩然','hash','approved');
         INSERT INTO kb_vector_records(vector_record_id,collection_id,revision_id,part_id,qdrant_point_id,modality,dimensions,status) VALUES('vr','vc','r','p','point','image',1024,'verified');"
    ).execute(&state.db).await.unwrap();
    (
        state,
        crate::catalog::Scope {
            course_id: "course".into(),
            node_ids: vec!["leaf".into()],
            title: "课程".into(),
            level: 0,
        },
    )
}
