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

#[cfg(test)]
mod migration_tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use sqlx::{migrate::Migrator, Row};
    use std::{borrow::Cow, collections::BTreeMap};

    #[tokio::test]
    async fn applied_learning_migration_upgrades_without_rewriting_checksums_or_dialogues() {
        let all = sqlx::migrate!("./migrations");
        let original = all
            .iter()
            .find(|migration| migration.version == 11)
            .unwrap();
        let original_checksum = original
            .checksum
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(original_checksum, "882a191a6afc36a2c953b26b4b245bd6a4d5e277e433b420e995f032c07aba3d0b289e02d907b0f11b3c2c91e825bc42");
        let old = Migrator {
            migrations: Cow::Owned(
                all.iter()
                    .filter(|migration| migration.version <= 11)
                    .cloned()
                    .collect(),
            ),
            ..Migrator::DEFAULT
        };
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        old.run(&pool).await.unwrap();
        sqlx::raw_sql("INSERT INTO kb_courses(course_id,title) VALUES('c','课程');
            INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id,created_at,creator_role) VALUES
                ('old','legacy','c','course','c','2000-01-01 00:00:00','student'),
                ('new','student','c','course','c','2099-01-01 00:00:00','student'),
                ('admin','teacher','c','course','c','2000-01-01 00:00:00','admin');
            INSERT INTO kb_messages(message_id,conversation_id,role,content_json,client_message_id) VALUES
                ('system','old','system','\"旧记录\"','system'),
                ('q1','old','user','\"原问题一\"','one'),
                ('a1','old','assistant','\"原回答一\"','one'),
                ('q2','new','user','\"原问题二\"','two');
            INSERT INTO kb_learning_collections VALUES('f','learning_dev_f',1024);
            INSERT INTO kb_learning_turns(turn_id,source_rowid,conversation_id,user_id,course_id,scope_type,scope_id,question,answer,citations_json,created_at,byte_size,indexed_fingerprint,vector_bytes)
                VALUES('q2',4,'new','student','c','course','c','原问题二','原回答二','[]','2099-01-01 00:00:00',24,'f',4096);
            UPDATE kb_learning_state SET data_cutoff_rowid=2,vector_cutoff_rowid=4;")
            .execute(&pool).await.unwrap();
        let before = preserved_records(&pool).await;

        // Validate that SQLx still rejects an actually changed migration. The
        // repair is an additive upgrade, never a disabled checksum check.
        let mut modified = original.clone();
        modified.checksum = Cow::Owned(vec![0; 48]);
        let invalid = Migrator {
            migrations: Cow::Owned(vec![modified]),
            ignore_missing: true,
            ..Migrator::DEFAULT
        };
        assert!(matches!(
            invalid.run(&pool).await,
            Err(sqlx::migrate::MigrateError::VersionMismatch(11))
        ));

        all.run(&pool).await.unwrap();
        assert_eq!(before, preserved_records(&pool).await);
        let checksum: Vec<u8> =
            sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=11")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(checksum.as_slice(), original.checksum.as_ref());
        let roles: Vec<(String, String)> = sqlx::query_as(
            "SELECT conversation_id,creator_role FROM kb_conversations ORDER BY conversation_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            roles,
            vec![
                ("admin".into(), "admin".into()),
                ("new".into(), "student".into()),
                ("old".into(), "unknown".into())
            ]
        );
        let source: i64 =
            sqlx::query_scalar("SELECT source_rowid FROM kb_learning_turns WHERE turn_id='q2'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(source, 2);
        let vector: (i64, i64) = sqlx::query_as(
            "SELECT source_rowid,vector_bytes FROM kb_learning_vectors WHERE turn_id='q2'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(vector, (2, 4096));
        let cutoffs: (i64, i64) = sqlx::query_as(
            "SELECT data_cutoff,vector_cutoff FROM kb_learning_resets WHERE scope_type='all'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(cutoffs, (1, 2));
        sqlx::query("INSERT INTO kb_conversations(conversation_id,user_id,course_id,scope_type,scope_id) VALUES('after','student','c','course','c')").execute(&pool).await.unwrap();
        let role: String = sqlx::query_scalar(
            "SELECT creator_role FROM kb_conversations WHERE conversation_id='after'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(role, "unknown");
        sqlx::query("INSERT INTO kb_messages(message_id,conversation_id,role,content_json,client_message_id) VALUES('q3','after','user','\"新问题\"','three')").execute(&pool).await.unwrap();
        let seq: i64 =
            sqlx::query_scalar("SELECT seq FROM kb_learning_source_seq WHERE turn_id='q3'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(seq > 2);
        all.run(&pool).await.unwrap();
        assert!(sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&pool)
            .await
            .unwrap()
            .is_empty());
    }

    // Digests avoid printing private course text or student messages if an
    // upgrade comparison fails. Role classification is the intended metadata
    // change; all other original conversation columns must remain unchanged.
    async fn preserved_records(pool: &SqlitePool) -> BTreeMap<String, (usize, String)> {
        let mut result = BTreeMap::new();
        for table in [
            "kb_assets",
            "kb_asset_revisions",
            "kb_content_parts",
            "kb_conversations",
            "kb_messages",
            "kb_teacher_conversations",
            "kb_teacher_messages",
        ] {
            let columns = sqlx::query(&format!("PRAGMA table_info({table})"))
                .fetch_all(pool)
                .await
                .unwrap();
            let columns = columns
                .iter()
                .map(|row| row.get::<String, _>("name"))
                .filter(|name| name != "creator_role" && name != "creator_role_legacy")
                .map(|name| format!("\"{name}\""))
                .collect::<Vec<_>>()
                .join(",");
            let rows: Vec<String> = sqlx::query_scalar(&format!(
                "SELECT json_array({columns}) FROM {table} ORDER BY rowid"
            ))
            .fetch_all(pool)
            .await
            .unwrap();
            let digest = format!("{:x}", Sha256::digest(rows.join("\n").as_bytes()));
            result.insert(table.to_owned(), (rows.len(), digest));
        }
        result
    }

    #[tokio::test]
    #[ignore = "requires immutable dev/prod SQLite backups via KB_TEST_MIGRATION_SNAPSHOT_DIR; upgrades copies only"]
    async fn copied_local_snapshots_upgrade_without_changing_course_or_chat_records() {
        let snapshots = std::path::PathBuf::from(
            std::env::var("KB_TEST_MIGRATION_SNAPSHOT_DIR")
                .expect("set KB_TEST_MIGRATION_SNAPSHOT_DIR to an immutable backup folder"),
        );
        let output = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("data/migration-validation")
            .join(uuid::Uuid::new_v4().to_string());
        tokio::fs::create_dir_all(&output).await.unwrap();
        for environment in ["dev", "prod"] {
            let destination = output.join(format!("{environment}.sqlite"));
            tokio::fs::copy(
                snapshots.join(format!("{environment}.sqlite")),
                &destination,
            )
            .await
            .unwrap();
            let url = format!(
                "sqlite://{}?mode=rw",
                destination.to_string_lossy().replace('\\', "/")
            );
            let pool = connect(&url).await.unwrap();
            let before = preserved_records(&pool).await;
            let prior: Vec<(i64, Vec<u8>)> =
                sqlx::query_as("SELECT version,checksum FROM _sqlx_migrations ORDER BY version")
                    .fetch_all(&pool)
                    .await
                    .unwrap();
            sqlx::migrate!("./migrations").run(&pool).await.unwrap();
            assert_eq!(
                before,
                preserved_records(&pool).await,
                "{environment} original content changed"
            );
            for (version, checksum) in prior {
                let saved: Vec<u8> =
                    sqlx::query_scalar("SELECT checksum FROM _sqlx_migrations WHERE version=?")
                        .bind(version)
                        .fetch_one(&pool)
                        .await
                        .unwrap();
                assert_eq!(saved, checksum, "prior checksum was rewritten");
            }
            let version: i64 =
                sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations WHERE success=1")
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(version, 13);
            let integrity: String = sqlx::query_scalar("PRAGMA integrity_check")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(integrity, "ok");
            println!("{environment} SQLite copy upgraded to migration 13; original record counts and hashes unchanged");
            pool.close().await;
        }
    }
}
