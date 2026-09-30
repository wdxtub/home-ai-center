//! 内嵌 SQL 迁移，启动时自动执行。
//!
//! 用 `sqlx::migrate!` 把 `migrations/` 目录编译进二进制，
//! 因此容器镜像不需要额外拷贝迁移文件。

use sqlx::SqlitePool;

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

pub async fn run(pool: &SqlitePool) -> anyhow::Result<()> {
    MIGRATOR.run(pool).await?;
    Ok(())
}
