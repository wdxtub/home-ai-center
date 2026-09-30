pub mod load;
pub mod migrate;
pub mod pool;

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use sqlx::SqlitePool;

/// 建立连接池并套用连接级 PRAGMA。
///
/// `sqlite` 的 `sqlite` 特性默认已启用 bundled（自带 libsqlite3），容器内无需
/// 系统库。写操作靠 `busy_timeout` + WAL 排队，调用方再对「读-改-写」加事务。
pub async fn connect(path: &std::path::Path) -> anyhow::Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))?
        .create_if_missing(true)
        // WAL：读不阻塞写，写不阻塞读，备份只需读锁。
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(5))
        .foreign_keys(true);

    let pool = SqlitePoolOptions::new()
        // SQLite 单写者：连接数开小一点即可，避免无谓的写竞争。
        .max_connections(8)
        .min_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect_with(opts)
        .await?;

    Ok(pool)
}
