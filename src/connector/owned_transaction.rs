use super::*;
use crate::{
    ast::*,
};
use async_trait::async_trait;
use metrics::{decrement_gauge, increment_gauge};
use std::sync::Arc;

extern crate metrics as metrics;

/// A representation of an SQL database transaction. If not commited, a
/// transaction will be rolled back by default when dropped.
///
/// Currently does not support nesting, so starting a new transaction using the
/// transaction object will panic.
pub struct OwnedTransaction {
    pub(crate) inner: Arc<dyn Queryable>,
}

impl OwnedTransaction {
    pub(crate) async fn new(
        inner: Arc<dyn Queryable>,
        begin_stmt: &str,
        tx_opts: TransactionOptions,
    ) -> crate::Result<OwnedTransaction> {
        let this = Self { inner: inner.clone() };

        if tx_opts.isolation_first {
            if let Some(isolation) = tx_opts.isolation_level {
                inner.set_tx_isolation_level(isolation).await?;
            }
        }

        inner.raw_cmd(begin_stmt).await?;

        if !tx_opts.isolation_first {
            if let Some(isolation) = tx_opts.isolation_level {
                inner.set_tx_isolation_level(isolation).await?;
            }
        }

        inner.server_reset_query_owned(&this).await?;

        increment_gauge!("prisma_client_queries_active", 1.0);
        Ok(this)
    }

    /// Commit the changes to the database and consume the transaction.
    pub async fn commit(&self) -> crate::Result<()> {
        decrement_gauge!("prisma_client_queries_active", 1.0);
        self.inner.raw_cmd("COMMIT").await?;

        Ok(())
    }

    /// Rolls back the changes to the database.
    pub async fn rollback(&self) -> crate::Result<()> {
        decrement_gauge!("prisma_client_queries_active", 1.0);
        self.inner.raw_cmd("ROLLBACK").await?;

        Ok(())
    }
}

#[async_trait]
impl Queryable for OwnedTransaction {
    async fn query(&self, q: Query<'_>) -> crate::Result<ResultSet> {
        self.inner.query(q).await
    }

    async fn execute(&self, q: Query<'_>) -> crate::Result<u64> {
        self.inner.execute(q).await
    }

    async fn query_raw(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<ResultSet> {
        self.inner.query_raw(sql, params).await
    }

    async fn query_raw_typed(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<ResultSet> {
        self.inner.query_raw_typed(sql, params).await
    }

    async fn execute_raw(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<u64> {
        self.inner.execute_raw(sql, params).await
    }

    async fn execute_raw_typed(&self, sql: &str, params: &[Value<'_>]) -> crate::Result<u64> {
        self.inner.execute_raw_typed(sql, params).await
    }

    async fn raw_cmd(&self, cmd: &str) -> crate::Result<()> {
        self.inner.raw_cmd(cmd).await
    }

    async fn version(&self) -> crate::Result<Option<String>> {
        self.inner.version().await
    }

    fn is_healthy(&self) -> bool {
        self.inner.is_healthy()
    }

    async fn set_tx_isolation_level(&self, isolation_level: IsolationLevel) -> crate::Result<()> {
        self.inner.set_tx_isolation_level(isolation_level).await
    }

    fn requires_isolation_first(&self) -> bool {
        self.inner.requires_isolation_first()
    }
}
