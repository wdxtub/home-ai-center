//! 进程配置：全部来自环境变量，便于单容器部署。
//!
//! 数据目录（SQLite + 备份 + 图片归档）必须能挂载到容器外，
//! 因此只有一个 `HOME_AI_DATA_DIR` 入口，不散落配置。

use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址，默认 `0.0.0.0:8080`。
    pub bind: String,
    /// 数据目录，默认 `./data`（容器内为 `/data`）。
    pub data_dir: PathBuf,
    /// 首次启动时用于初始化管理员口令；之后仅用于比对。
    pub admin_token: Option<String>,
    /// 后台任务线程数。
    pub background_jobs: usize,
    /// 每日备份的本地时刻，形如 `03:30`。
    pub backup_at: String,
    /// 时区名，用于备份调度与「每日」统计的分桶。
    pub timezone: String,
    /// 节点健康复检间隔（秒）。
    pub probe_interval_secs: u64,
    /// 单次排队等待上限（秒）。
    pub queue_timeout_secs: u64,
    /// 单个请求内最多轮换几把 key。
    pub max_key_rotation: usize,
}

/// 未显式指定时区时用的默认时区（写库校验要用，不能读环境变量）。
pub const DEFAULT_TZ: &str = "Asia/Shanghai";

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

impl Config {
    pub fn from_env() -> Self {
        let data_dir = PathBuf::from(env_or("HOME_AI_DATA_DIR", "./data"));
        let timezone = env_or("HOME_AI_TIMEZONE", DEFAULT_TZ);
        let backup_at = env_or("HOME_AI_BACKUP_AT", "03:30");
        let admin_token = std::env::var("HOME_AI_ADMIN_TOKEN").ok().filter(|s| !s.is_empty());

        Self {
            bind: env_or("HOME_AI_BIND", "0.0.0.0:8080"),
            data_dir,
            admin_token,
            background_jobs: env_or("HOME_AI_BACKGROUND_JOBS", "4")
                .parse()
                .unwrap_or(4),
            backup_at,
            timezone,
            probe_interval_secs: env_or("HOME_AI_PROBE_INTERVAL", "15")
                .parse()
                .unwrap_or(15),
            queue_timeout_secs: env_or("HOME_AI_QUEUE_TIMEOUT", "120")
                .parse()
                .unwrap_or(120),
            max_key_rotation: env_or("HOME_AI_MAX_KEY_ROTATION", "3")
                .parse()
                .unwrap_or(3),
        }
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("home-ai-center.db")
    }

    pub fn backup_dir(&self) -> PathBuf {
        self.data_dir.join("backups")
    }

    pub fn image_dir(&self) -> PathBuf {
        self.data_dir.join("images")
    }

    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        for dir in [&self.data_dir, &self.backup_dir(), &self.image_dir()] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }
}
