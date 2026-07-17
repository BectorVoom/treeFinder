//! SQLite metadata repository (documents, revisions, index catalog, search
//! runs) with WAL mode and a recoverable two-phase write protocol.
//!
//! A content update first records a `pending` revision row (with expected
//! before/after hashes), then the file is atomically replaced, then the
//! revision is finalized. `pending_revisions()` lets startup reconcile
//! interrupted writes by comparing hashes; nothing ambiguous is discarded.

use crate::domain::{
    Actor, ActorType, Document, HdsError, HdsResult, IndexStatus, Operation, PatchFormat, Revision,
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;
use std::path::Path;

pub struct MetadataDb {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct IndexRecord {
    pub document_id: String,
    pub index_version: String,
    pub builder: String,
    pub builder_version: String,
    pub config_hash: String,
    pub revision_id: String,
    pub created_at: DateTime<Utc>,
    pub current: bool,
}

#[derive(Debug, Clone)]
pub struct PendingRevision {
    pub revision: Revision,
    pub logical_path: String,
}

impl MetadataDb {
    pub fn open(path: &Path) -> HdsResult<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        let db = MetadataDb { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> HdsResult<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS documents (
                document_id      TEXT PRIMARY KEY,
                logical_path     TEXT NOT NULL UNIQUE,
                title            TEXT NOT NULL,
                current_revision TEXT NOT NULL,
                content_hash     TEXT NOT NULL,
                created_at       TEXT NOT NULL,
                updated_at       TEXT NOT NULL,
                index_status     TEXT NOT NULL,
                metadata         TEXT NOT NULL DEFAULT '{}',
                deleted          INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS revisions (
                revision_id        TEXT PRIMARY KEY,
                parent_revision_id TEXT,
                document_id        TEXT NOT NULL,
                actor_type         TEXT NOT NULL,
                actor_id           TEXT NOT NULL,
                operation          TEXT NOT NULL,
                before_hash        TEXT,
                after_hash         TEXT,
                message            TEXT,
                created_at         TEXT NOT NULL,
                patch_format       TEXT NOT NULL,
                status             TEXT NOT NULL DEFAULT 'committed'
            );
            CREATE INDEX IF NOT EXISTS idx_revisions_document
                ON revisions(document_id, created_at);
            CREATE TABLE IF NOT EXISTS indexes (
                document_id     TEXT NOT NULL,
                index_version   TEXT NOT NULL,
                builder         TEXT NOT NULL,
                builder_version TEXT NOT NULL,
                config_hash     TEXT NOT NULL,
                revision_id     TEXT NOT NULL,
                created_at      TEXT NOT NULL,
                current         INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (document_id, index_version)
            );
            CREATE TABLE IF NOT EXISTS search_runs (
                search_run_id    TEXT PRIMARY KEY,
                created_at       TEXT NOT NULL,
                query            TEXT NOT NULL,
                strategy         TEXT NOT NULL,
                strategy_version TEXT NOT NULL,
                config_hash      TEXT NOT NULL,
                result_json      TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    // ----- documents -----

    pub fn insert_document(&self, doc: &Document) -> HdsResult<()> {
        self.conn.execute(
            "INSERT INTO documents (document_id, logical_path, title, current_revision,
                content_hash, created_at, updated_at, index_status, metadata, deleted)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                doc.document_id,
                doc.logical_path,
                doc.title,
                doc.current_revision,
                doc.content_hash,
                doc.created_at.to_rfc3339(),
                doc.updated_at.to_rfc3339(),
                doc.index_status.as_str(),
                serde_json::to_string(&doc.metadata)?,
                doc.deleted as i64,
            ],
        )?;
        Ok(())
    }

    pub fn update_document(&self, doc: &Document) -> HdsResult<()> {
        let n = self.conn.execute(
            "UPDATE documents SET logical_path=?2, title=?3, current_revision=?4,
                content_hash=?5, updated_at=?6, index_status=?7, metadata=?8, deleted=?9
             WHERE document_id=?1",
            params![
                doc.document_id,
                doc.logical_path,
                doc.title,
                doc.current_revision,
                doc.content_hash,
                doc.updated_at.to_rfc3339(),
                doc.index_status.as_str(),
                serde_json::to_string(&doc.metadata)?,
                doc.deleted as i64,
            ],
        )?;
        if n == 0 {
            return Err(HdsError::not_found(format!("document {}", doc.document_id)));
        }
        Ok(())
    }

    pub fn set_index_status(&self, document_id: &str, status: IndexStatus) -> HdsResult<()> {
        self.conn.execute(
            "UPDATE documents SET index_status=?2 WHERE document_id=?1",
            params![document_id, status.as_str()],
        )?;
        Ok(())
    }

    fn row_to_document(row: &rusqlite::Row<'_>) -> rusqlite::Result<Document> {
        let metadata: String = row.get("metadata")?;
        let created: String = row.get("created_at")?;
        let updated: String = row.get("updated_at")?;
        let status: String = row.get("index_status")?;
        Ok(Document {
            document_id: row.get("document_id")?,
            logical_path: row.get("logical_path")?,
            title: row.get("title")?,
            current_revision: row.get("current_revision")?,
            content_hash: row.get("content_hash")?,
            created_at: parse_ts(&created),
            updated_at: parse_ts(&updated),
            index_status: IndexStatus::parse(&status),
            metadata: serde_json::from_str::<BTreeMap<String, serde_json::Value>>(&metadata)
                .unwrap_or_default(),
            deleted: row.get::<_, i64>("deleted")? != 0,
        })
    }

    pub fn document_by_id(&self, document_id: &str) -> HdsResult<Option<Document>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM documents WHERE document_id=?1",
                params![document_id],
                Self::row_to_document,
            )
            .optional()?)
    }

    pub fn document_by_path(&self, logical_path: &str) -> HdsResult<Option<Document>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM documents WHERE logical_path=?1 AND deleted=0",
                params![logical_path],
                Self::row_to_document,
            )
            .optional()?)
    }

    /// Keyset pagination ordered by logical path; cursor is the last path seen.
    pub fn list_documents(
        &self,
        prefix: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
        include_deleted: bool,
    ) -> HdsResult<(Vec<Document>, Option<String>)> {
        let prefix = prefix.unwrap_or("");
        let cursor = cursor.unwrap_or("");
        let like = format!("{}%", prefix.replace('%', "\\%").replace('_', "\\_"));
        let mut stmt = self.conn.prepare(
            "SELECT * FROM documents
             WHERE logical_path LIKE ?1 ESCAPE '\\'
               AND logical_path > ?2
               AND (deleted=0 OR ?3)
             ORDER BY logical_path
             LIMIT ?4",
        )?;
        let rows = stmt.query_map(
            params![like, cursor, include_deleted as i64, (limit + 1) as i64],
            Self::row_to_document,
        )?;
        let mut docs: Vec<Document> = rows.collect::<Result<_, _>>()?;
        let next = if docs.len() > limit {
            docs.truncate(limit);
            docs.last().map(|d| d.logical_path.clone())
        } else {
            None
        };
        Ok((docs, next))
    }

    pub fn all_documents(&self, include_deleted: bool) -> HdsResult<Vec<Document>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM documents WHERE (deleted=0 OR ?1) ORDER BY logical_path")?;
        let rows = stmt.query_map(params![include_deleted as i64], Self::row_to_document)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- revisions -----

    pub fn insert_revision(&self, rev: &Revision, status: &str) -> HdsResult<()> {
        self.conn.execute(
            "INSERT INTO revisions (revision_id, parent_revision_id, document_id, actor_type,
                actor_id, operation, before_hash, after_hash, message, created_at,
                patch_format, status)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                rev.revision_id,
                rev.parent_revision_id,
                rev.document_id,
                rev.actor.actor_type.as_str(),
                rev.actor.id,
                rev.operation.as_str(),
                rev.before_hash,
                rev.after_hash,
                rev.message,
                rev.created_at.to_rfc3339(),
                rev.patch_format.as_str(),
                status,
            ],
        )?;
        Ok(())
    }

    pub fn set_revision_status(&self, revision_id: &str, status: &str) -> HdsResult<()> {
        self.conn.execute(
            "UPDATE revisions SET status=?2 WHERE revision_id=?1",
            params![revision_id, status],
        )?;
        Ok(())
    }

    fn row_to_revision(row: &rusqlite::Row<'_>) -> rusqlite::Result<Revision> {
        let created: String = row.get("created_at")?;
        let actor_type: String = row.get("actor_type")?;
        let op: String = row.get("operation")?;
        let fmt: String = row.get("patch_format")?;
        Ok(Revision {
            revision_id: row.get("revision_id")?,
            parent_revision_id: row.get("parent_revision_id")?,
            document_id: row.get("document_id")?,
            actor: Actor {
                actor_type: ActorType::parse(&actor_type),
                id: row.get("actor_id")?,
            },
            operation: Operation::parse(&op),
            before_hash: row.get("before_hash")?,
            after_hash: row.get("after_hash")?,
            message: row.get("message")?,
            created_at: parse_ts(&created),
            patch_format: PatchFormat::parse(&fmt).unwrap_or(PatchFormat::FullSnapshot),
        })
    }

    pub fn revision(&self, revision_id: &str) -> HdsResult<Option<Revision>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM revisions WHERE revision_id=?1 AND status='committed'",
                params![revision_id],
                Self::row_to_revision,
            )
            .optional()?)
    }

    /// Newest first; cursor is the last revision_id seen (ULIDs sort by time).
    pub fn list_revisions(
        &self,
        document_id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> HdsResult<(Vec<Revision>, Option<String>)> {
        let mut stmt = self.conn.prepare(
            "SELECT * FROM revisions
             WHERE document_id=?1 AND status='committed'
               AND (?2 = '' OR revision_id < ?2)
             ORDER BY revision_id DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![document_id, cursor.unwrap_or(""), (limit + 1) as i64],
            Self::row_to_revision,
        )?;
        let mut revs: Vec<Revision> = rows.collect::<Result<_, _>>()?;
        let next = if revs.len() > limit {
            revs.truncate(limit);
            revs.last().map(|r| r.revision_id.clone())
        } else {
            None
        };
        Ok((revs, next))
    }

    pub fn pending_revisions(&self) -> HdsResult<Vec<PendingRevision>> {
        let mut stmt = self.conn.prepare(
            "SELECT r.*, d.logical_path AS logical_path
             FROM revisions r JOIN documents d ON d.document_id = r.document_id
             WHERE r.status='pending'",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(PendingRevision {
                revision: Self::row_to_revision(row)?,
                logical_path: row.get("logical_path")?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ----- index catalog -----

    pub fn record_index(&self, rec: &IndexRecord) -> HdsResult<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE indexes SET current=0 WHERE document_id=?1",
            params![rec.document_id],
        )?;
        tx.execute(
            "INSERT OR REPLACE INTO indexes (document_id, index_version, builder,
                builder_version, config_hash, revision_id, created_at, current)
             VALUES (?1,?2,?3,?4,?5,?6,?7,1)",
            params![
                rec.document_id,
                rec.index_version,
                rec.builder,
                rec.builder_version,
                rec.config_hash,
                rec.revision_id,
                rec.created_at.to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn row_to_index_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<IndexRecord> {
        let created: String = row.get("created_at")?;
        Ok(IndexRecord {
            document_id: row.get("document_id")?,
            index_version: row.get("index_version")?,
            builder: row.get("builder")?,
            builder_version: row.get("builder_version")?,
            config_hash: row.get("config_hash")?,
            revision_id: row.get("revision_id")?,
            created_at: parse_ts(&created),
            current: row.get::<_, i64>("current")? != 0,
        })
    }

    pub fn current_index(&self, document_id: &str) -> HdsResult<Option<IndexRecord>> {
        Ok(self
            .conn
            .query_row(
                "SELECT * FROM indexes WHERE document_id=?1 AND current=1",
                params![document_id],
                Self::row_to_index_record,
            )
            .optional()?)
    }

    // ----- search runs -----

    #[allow(clippy::too_many_arguments)]
    pub fn record_search_run(
        &self,
        run_id: &str,
        created_at: DateTime<Utc>,
        query: &str,
        strategy: &str,
        strategy_version: &str,
        config_hash: &str,
        result_json: &str,
    ) -> HdsResult<()> {
        self.conn.execute(
            "INSERT INTO search_runs (search_run_id, created_at, query, strategy,
                strategy_version, config_hash, result_json)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                run_id,
                created_at.to_rfc3339(),
                query,
                strategy,
                strategy_version,
                config_hash,
                result_json,
            ],
        )?;
        Ok(())
    }

    pub fn search_run(&self, run_id: &str) -> HdsResult<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT result_json FROM search_runs WHERE search_run_id=?1",
                params![run_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Quick integrity probe used by `hds doctor`.
    pub fn integrity_check(&self) -> HdsResult<String> {
        Ok(self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))?)
    }
}

fn parse_ts(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}
