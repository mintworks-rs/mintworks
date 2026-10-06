//! `DocumentStore` over SQLite: the `documents` table.

use async_trait::async_trait;
use mintworks_core::{ids::DocId, prelude::*};
use mintworks_pdf::{Document, DocumentStore};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::SqliteStore;
use crate::util::DbExt;

#[async_trait]
impl DocumentStore for SqliteStore {
	async fn document_insert(
		&self,
		org_id: i64,
		uid: &DocId,
		template: &str,
		job_key: &str,
	) -> ClResult<()> {
		sqlx::query(
			"INSERT INTO documents (uid, org_id, template, job_key, created_at)
			 VALUES (?, ?, ?, ?, ?)",
		)
		.bind(uid.as_str())
		.bind(org_id)
		.bind(template)
		.bind(job_key)
		.bind(Timestamp::now().0)
		.execute(&mut *self.conn().await?)
		.await
		.db()?;
		Ok(())
	}

	async fn document_rendered(&self, uid: &DocId, sha256: &str, bytes: i64) -> ClResult<()> {
		sqlx::query("UPDATE documents SET sha256 = ?, bytes = ? WHERE uid = ?")
			.bind(sha256)
			.bind(bytes)
			.bind(uid.as_str())
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}

	async fn document_get(&self, org_id: i64, uid: &DocId) -> ClResult<Option<Document>> {
		let row = sqlx::query(
			"SELECT uid, template, job_key, sha256, bytes, created_at
			   FROM documents WHERE uid = ? AND org_id = ?",
		)
		.bind(uid.as_str())
		.bind(org_id)
		.fetch_optional(&mut *self.reader().await?)
		.await
		.db()?;
		row.as_ref().map(document).transpose()
	}

	async fn documents_for_account(&self, acc: &AccountId) -> ClResult<Vec<Document>> {
		let sql = format!(
			"SELECT uid, template, job_key, sha256, bytes, created_at FROM documents
			 WHERE org_id IN ({PERSONAL_ORG}) ORDER BY id"
		);
		let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(acc.as_str())
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()?;
		rows.iter().map(document).collect()
	}

	async fn documents_orphaned_by_account(&self, acc: &AccountId) -> ClResult<Vec<String>> {
		let sql = format!(
			"SELECT DISTINCT d.sha256 FROM documents d
			 WHERE d.org_id IN ({PERSONAL_ORG}) AND d.sha256 IS NOT NULL
			   AND NOT EXISTS (SELECT 1 FROM documents o
			                   WHERE o.sha256 = d.sha256 AND o.org_id NOT IN ({PERSONAL_ORG}))
			   AND NOT EXISTS (SELECT 1 FROM invoice_documents i WHERE i.sha256 = d.sha256)
			 ORDER BY d.sha256"
		);
		sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
			.bind(acc.as_str())
			.bind(acc.as_str())
			.fetch_all(&mut *self.reader().await?)
			.await
			.db()
	}

	async fn documents_erase_account(&self, acc: &AccountId) -> ClResult<()> {
		let sql = format!("DELETE FROM documents WHERE org_id IN ({PERSONAL_ORG})");
		sqlx::query(sqlx::AssertSqlSafe(sql))
			.bind(acc.as_str())
			.execute(&mut *self.conn().await?)
			.await
			.db()?;
		Ok(())
	}
}

/// The id of the personal org of the account whose uid is bound.
const PERSONAL_ORG: &str = "SELECT o.id FROM orgs o JOIN accounts a ON a.id = o.owner_account_id
	 WHERE a.uid = ? AND o.kind = 'PERSONAL'";

fn document(r: &SqliteRow) -> ClResult<Document> {
	Ok(Document {
		uid: DocId::from_trusted(r.try_get("uid").db()?),
		template: r.try_get("template").db()?,
		job_key: r.try_get("job_key").db()?,
		sha256: r.try_get("sha256").db()?,
		bytes: r.try_get("bytes").db()?,
		created_at: Timestamp(r.try_get("created_at").db()?),
	})
}

// vim: ts=4
