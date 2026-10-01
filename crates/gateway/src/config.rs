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
    /// 节点健康复检间隔（**分钟**）。`tasks/probe.rs` 内部会 `* 60` 换成秒，
    /// 且下限为 10 —— 字段名里的 `_secs` 是历史遗留，别按秒理解。
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

    pub fn ensure_dirs(&self) -> anyhow::Result<()> {
        // 刻意**不建** images/：出图归档已移除，图片由调用方自己保存。
        for dir in [&self.data_dir, &self.backup_dir()] {
            std::fs::create_dir_all(dir)?;
        }
        self.reap_legacy_image_dir();
        Ok(())
    }

    /// 清理归档时代留下的 `data/images/`。
    ///
    /// **只删「实质为空」的目录。** 判定时忽略 `.` 开头的点文件——macOS 上
    /// Finder 会往任何被浏览过的目录塞 `.DS_Store`，不忽略它的话这个清理在
    /// mac 上会被永久卡住。
    ///
    /// 有实际文件就**绝不删**：那可能是唯一一份图。只告警，由人处理。
    fn reap_legacy_image_dir(&self) {
        let dir = self.data_dir.join("images");
        if !dir.exists() {
            return;
        }
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!("检查 {} 失败：{e}", dir.display());
                return;
            }
        };

        let mut dotfiles: Vec<std::path::PathBuf> = Vec::new();
        let mut has_real_content = false;
        for e in entries.flatten() {
            let name = e.file_name();
            if name.to_string_lossy().starts_with('.') {
                dotfiles.push(e.path());
            } else {
                has_real_content = true;
                break;
            }
        }

        if has_real_content {
            tracing::warn!(
                "数据目录里还有旧的归档目录 {}，出图归档已移除，这个目录不会再被写入；\
                 确认里面不需要了可以手动删掉",
                dir.display()
            );
            return;
        }

        // 只删点文件，不递归：上面已经确认没有别的内容了
        for f in dotfiles {
            if let Err(e) = std::fs::remove_file(&f) {
                tracing::debug!("删除 {} 失败：{e}", f.display());
            }
        }
        match std::fs::remove_dir(&dir) {
            Ok(()) => tracing::info!("已清理空的归档目录 {}", dir.display()),
            Err(e) => tracing::warn!("清理空目录 {} 失败：{e}", dir.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_in(dir: &std::path::Path) -> Config {
        Config {
            data_dir: dir.to_path_buf(),
            bind: "127.0.0.1:0".into(),
            admin_token: None,
            background_jobs: 1,
            backup_at: "03:30".into(),
            timezone: "Asia/Shanghai".into(),
            probe_interval_secs: 15,
            queue_timeout_secs: 120,
            max_key_rotation: 3,
        }
    }

    /// 归档时代留下的空目录会被清掉——它已经没有任何用途了。
    #[test]
    fn empty_legacy_dir_is_reaped() {
        let dir = tempfile::tempdir().expect("建临时目录");
        std::fs::create_dir_all(dir.path().join("images")).unwrap();

        cfg_in(dir.path()).ensure_dirs().unwrap();
        assert!(!dir.path().join("images").exists(), "空归档目录应当被清掉");
    }

    /// 承重边界：目录里有文件就**绝不能删**。那可能是唯一一份图。
    #[test]
    fn non_empty_legacy_dir_is_kept() {
        let dir = tempfile::tempdir().expect("建临时目录");
        let images = dir.path().join("images");
        std::fs::create_dir_all(&images).unwrap();
        let file = images.join("abc.png");
        std::fs::write(&file, b"png").unwrap();

        cfg_in(dir.path()).ensure_dirs().unwrap();
        assert!(images.exists(), "有文件的归档目录必须保留");
        assert_eq!(std::fs::read(&file).unwrap(), b"png", "文件内容不能被动过");
    }

    /// 压根没有 images/ 时不该报错，也不该把它建出来。
    #[test]
    fn missing_legacy_dir_is_not_created() {
        let dir = tempfile::tempdir().expect("建临时目录");
        cfg_in(dir.path()).ensure_dirs().unwrap();
        assert!(!dir.path().join("images").exists());
        // 正常该建的还是要建
        assert!(dir.path().join("backups").exists());
    }

    /// macOS 上 Finder 会往任何浏览过的目录塞 `.DS_Store`。不忽略点文件的話，
    /// 这个清理在 mac 上会被永久卡住——而这台机器就是 mac。
    #[test]
    fn dotfiles_alone_do_not_block_the_reap() {
        let dir = tempfile::tempdir().expect("建临时目录");
        let images = dir.path().join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join(".DS_Store"), b"\x00\x01").unwrap();

        cfg_in(dir.path()).ensure_dirs().unwrap();
        assert!(!images.exists(), "只剩 .DS_Store 时应当照样清理掉");
    }

    /// 点文件 + 真实文件混在一起时，仍以「有真实内容」为准，一个都不动。
    #[test]
    fn dotfiles_do_not_excuse_leaving_real_files() {
        let dir = tempfile::tempdir().expect("建临时目录");
        let images = dir.path().join("images");
        std::fs::create_dir_all(&images).unwrap();
        std::fs::write(images.join(".DS_Store"), b"x").unwrap();
        let keep = images.join("real.png");
        std::fs::write(&keep, b"png").unwrap();

        cfg_in(dir.path()).ensure_dirs().unwrap();
        assert!(keep.exists(), "有真实文件时整目录都要保留");
        assert!(images.join(".DS_Store").exists(), "点文件也不该动");
    }
}
