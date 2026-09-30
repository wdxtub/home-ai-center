//! 连接池辅助：把「读-改-写」包进写事务，串行化避免 `SQLITE_BUSY`。

use sqlx::SqlitePool;

/// 开一个立即取写锁的事务。余额预扣、结算、key 状态更新都走这里。
pub async fn write_tx<'a, T, F, E>(pool: &'a SqlitePool, f: F) -> Result<T, E>
where
    F: for<'c> FnOnce(
        &'c mut sqlx::Transaction<'a, sqlx::Sqlite>,
    )
        -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<T, E>> + Send + 'c>>,
    E: From<sqlx::Error>,
{
    let mut tx = pool.begin().await?;
    let out = f(&mut tx).await?;
    tx.commit().await?;
    Ok(out)
}
