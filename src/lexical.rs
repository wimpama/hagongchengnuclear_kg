//! Bounded Chinese bigram / Latin token index supplementing Qdrant ANN.
use crate::error::ApiError;
use sqlx::{Row, Sqlite, SqlitePool, Transaction};
use std::collections::BTreeSet;

fn han(c: char) -> bool {
    matches!(c,'\u{3400}'..='\u{4dbf}'|'\u{4e00}'..='\u{9fff}'|'\u{20000}'..='\u{2fa1f}')
}
pub fn tokens(input: &str) -> BTreeSet<String> {
    let mut tokens = BTreeSet::new();
    let mut latin = String::new();
    let mut last_han = None;
    for c in input.chars().take(100_000) {
        if han(c) {
            if !latin.is_empty() {
                tokens.insert(std::mem::take(&mut latin));
            }
            if let Some(previous) = last_han {
                tokens.insert(format!("{previous}{c}"));
            }
            last_han = Some(c);
        } else {
            last_han = None;
            if c.is_ascii_alphanumeric() {
                if latin.len() < 80 {
                    latin.push(c.to_ascii_lowercase());
                }
            } else if !latin.is_empty() {
                tokens.insert(std::mem::take(&mut latin));
            }
        }
    }
    if !latin.is_empty() {
        tokens.insert(latin);
    }
    tokens
}
pub fn query(input: &str) -> String {
    tokens(input)
        .into_iter()
        .take(40)
        .map(|t| format!("\"{t}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}
pub async fn index_revision(
    tx: &mut Transaction<'_, Sqlite>,
    revision: &str,
) -> Result<(), ApiError> {
    let rows=sqlx::query("SELECT part_id,content_text,content_sha256 FROM kb_content_parts WHERE revision_id=? AND evidence_status='approved' AND content_text IS NOT NULL")
        .bind(revision).fetch_all(&mut **tx).await?;
    for row in rows {
        let part: String = row.get("part_id");
        let text: String = row.get("content_text");
        sqlx::query("DELETE FROM kb_lexical_fts WHERE part_id=?")
            .bind(&part)
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO kb_lexical_fts(part_id,tokens) VALUES(?,?)")
            .bind(&part)
            .bind(tokens(&text).into_iter().collect::<Vec<_>>().join(" "))
            .execute(&mut **tx)
            .await?;
        sqlx::query("INSERT INTO kb_lexical_versions(part_id,content_sha256) VALUES(?,?) ON CONFLICT(part_id) DO UPDATE SET content_sha256=excluded.content_sha256")
            .bind(&part).bind(row.get::<String,_>("content_sha256")).execute(&mut **tx).await?;
    }
    Ok(())
}
pub async fn backfill(db: &SqlitePool) -> Result<(), ApiError> {
    let revisions:Vec<String>=sqlx::query_scalar("SELECT DISTINCT p.revision_id FROM kb_content_parts p LEFT JOIN kb_lexical_versions v USING(part_id) WHERE p.evidence_status='approved' AND p.content_text IS NOT NULL AND (v.part_id IS NULL OR v.content_sha256!=p.content_sha256)").fetch_all(db).await?;
    for revision in revisions {
        let mut tx = db.begin().await?;
        index_revision(&mut tx, &revision).await?;
        tx.commit().await?;
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn chinese_question_matches_embedded_term() {
        let db = SqlitePool::connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE VIRTUAL TABLE kb_lexical_fts USING fts5(part_id UNINDEXED,tokens)")
            .execute(&db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO kb_lexical_fts VALUES('p',?)")
            .bind(
                tokens("速度等于位移与时间之比；F = m a")
                    .into_iter()
                    .collect::<Vec<_>>()
                    .join(" "),
            )
            .execute(&db)
            .await
            .unwrap();
        let result: String =
            sqlx::query_scalar("SELECT part_id FROM kb_lexical_fts WHERE kb_lexical_fts MATCH ?")
                .bind(query("速度是什么？"))
                .fetch_one(&db)
                .await
                .unwrap();
        assert_eq!(result, "p");
        assert!(query("\" OR NEAR(foo) --")
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "\" ".contains(c)));
    }
}
