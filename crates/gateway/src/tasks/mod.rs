//! 后台任务。
//!
//! 三个任务都是**幂等**的：重复跑不会把状态搞坏。
//! 容器重启后它们会自己重新开始，不需要任何外部调度器。

pub mod backup;
pub mod probe;
pub mod retention;

/// 按配置的并发数拉起全部后台任务。
pub fn spawn_all(s: std::sync::Arc<crate::state::AppState>) {
    let n = s.cfg.background_jobs.max(1);

    let t1 = s.clone();
    tokio::spawn(async move { backup::scheduler(t1).await });

    let t2 = s.clone();
    tokio::spawn(async move { probe::scheduler(t2).await });

    let t3 = s.clone();
    tokio::spawn(async move { retention::scheduler(t3).await });

    tracing::info!(workers = n, "后台任务已启动");
}
