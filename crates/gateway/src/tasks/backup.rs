//! 每日备份 + 滚动保留。
//!
//! 用 SQLite 的 `VACUUM INTO`：它产出一个**一致性快照**，且只取读锁，
//! 不阻塞在线写入。直接 `cp` 数据库文件在 WAL 模式下会拷到一个
//! 「主库与 WAL 不匹配」的坏副本——这是最常见的备份翻车方式。
//!
//! 写入顺序固定为 **`.tmp` → 原子改名**：中途崩溃或磁盘写满时，
//! 目录里只会多一个 `.tmp`，不会多一个「看起来正常但其实是半截」的备份。

use std::path::PathBuf;
use std::sync::Arc;

use crate::state::AppState;

pub struct BackupOutcome {
    pub ok: bool,
    pub path: String,
    pub size_bytes: i64,
    pub error: Option<String>,
}

fn now() -> i64 {
    crate::keys::unix_now()
}

fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 立刻备份一次。手动触发与每日任务走**同一条路径**。
pub async fn run_once(s: &Arc<AppState>) -> anyhow::Result<BackupOutcome> {
    let started = now();
    let dir = s.cfg.backup_dir();
    std::fs::create_dir_all(&dir)?;

    let final_path = dir.join(format!("hac-{}.db", stamp()));
    // VACUUM INTO 的目标文件**不得已存在且非空**，否则 SQLite 直接报错。
    // 同秒内重复触发（手动 + 定时撞车）会撞上这个约束，所以先清掉。
    let tmp_path = final_path.with_extension("db.tmp");
    let _ = std::fs::remove_file(&tmp_path);

    let sql = format!(
        "VACUUM INTO '{}'",
        tmp_path.to_string_lossy().replace('\'', "''")
    );
    let result: anyhow::Result<i64> = match sqlx::query(&sql).execute(&s.pool).await {
        Ok(_) => Ok(0),
        Err(e) => Err(e.into()),
    };

    let outcome = match result {
        Ok(_) => match std::fs::rename(&tmp_path, &final_path) {
            Ok(()) => {
                let size = std::fs::metadata(&final_path).map(|m| m.len() as i64).unwrap_or(0);
                record(s, &final_path, size, "ok", None, started).await;
                tracing::info!(path = %final_path.display(), size, "备份完成");
                BackupOutcome {
                    ok: true,
                    path: final_path.display().to_string(),
                    size_bytes: size,
                    error: None,
                }
            }
            Err(e) => {
                let _ = std::fs::remove_file(&tmp_path);
                finish_error(s, &final_path, &format!("改名失败: {e}"), started).await;
                BackupOutcome {
                    ok: false,
                    path: final_path.display().to_string(),
                    size_bytes: 0,
                    error: Some(format!("改名失败: {e}")),
                }
            }
        },
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            let msg = format!("VACUUM INTO 失败: {e}");
            finish_error(s, &final_path, &msg, started).await;
            BackupOutcome {
                ok: false,
                path: final_path.display().to_string(),
                size_bytes: 0,
                error: Some(msg),
            }
        }
    };
    Ok(outcome)
}

async fn record(
    s: &Arc<AppState>,
    path: &PathBuf,
    size: i64,
    status: &str,
    error: Option<&str>,
    started: i64,
) {
    let _ = sqlx::query(
        "INSERT INTO backup_record (path, size_bytes, status, error, started_at, finished_at)
         VALUES (?,?,?,?,?,?)",
    )
    .bind(path.display().to_string())
    .bind(size)
    .bind(status)
    .bind(error)
    .bind(started)
    .bind(now())
    .execute(&s.pool)
    .await;
}

async fn finish_error(s: &Arc<AppState>, path: &PathBuf, msg: &str, started: i64) {
    record(s, path, 0, "error", Some(msg), started).await;
    tracing::error!(error = %msg, "备份失败");
}

/// 滚动保留：只保留最近 `keep_days` 天。
///
/// 顺带清掉 `backup_record` 里的历史行——否则表会无限增长，
/// 而它的价值只在「最近 7 天」这个窗口内。
pub async fn purge(s: &Arc<AppState>, keep_days: i64) -> anyhow::Result<usize> {
    let dir = s.cfg.backup_dir();
    let cutoff = chrono::Local::now() - chrono::Duration::days(keep_days);

    let mut removed = 0usize;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if !name.starts_with("hac-") {
                continue;
            }
            // 文件名里带时间戳，直接比字符串即可
            let ts = name
                .trim_start_matches("hac-")
                .trim_end_matches(".db")
                .to_string();
            let Some(t) = parse_stamp(&ts) else {
                continue;
            };
            if t < cutoff {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }

    // 记录表只留 30 天
    let _ = sqlx::query("DELETE FROM backup_record WHERE started_at < ?")
        .bind(now() - 30 * 24 * 3600)
        .execute(&s.pool)
        .await;

    if removed > 0 {
        tracing::info!(removed, keep_days, "清理过期备份");
    }
    Ok(removed)
}

/// 解析文件名里的 `YYYYMMDD-HHMMSS`。
pub fn parse_stamp(s: &str) -> Option<chrono::DateTime<chrono::Local>> {
    chrono::NaiveDateTime::parse_from_str(s, "%Y%m%d-%H%M%S")
        .ok()
        .and_then(|d| d.and_local_timezone(chrono::Local).single())
}

/// 每日调度：每天在配置的时刻跑一次。
///
/// 用「下一次触发时刻」而不是「睡 24 小时」——后者会在机器休眠
/// 或容器重启后整体漂移，而且补不上错过的窗口。
pub async fn scheduler(s: Arc<AppState>) {
    let (hour, minute) = parse_hhmm(&s.cfg.backup_at);
    let tz = s.cfg.timezone.clone();

    loop {
        let next = next_run(hour, minute, &tz);
        let wait = (next - chrono::Utc::now()).num_milliseconds().max(0) as u64;
        tracing::info!(at = %next.to_rfc3339(), wait_ms = wait, "下一次备份");
        tokio::time::sleep(std::time::Duration::from_millis(wait)).await;

        if let Err(e) = run_once(&s).await {
            tracing::error!(error = %e, "定时备份异常");
        }
        if let Err(e) = purge(&s, 7).await {
            tracing::error!(error = %e, "清理过期备份异常");
        }
    }
}

/// 下一次触发的 UTC 时刻。
pub fn next_run(hour: u32, minute: u32, tz: &str) -> chrono::DateTime<chrono::Utc> {
    let tz: chrono_tz::Tz = tz.parse().unwrap_or(chrono_tz::Asia::Shanghai);
    let local = chrono::Utc::now().with_timezone(&tz);
    let today = local
        .date_naive()
        .and_hms_opt(hour, minute, 0)
        .expect("小时/分钟已 clamp 到合法范围");
    let mut target = today;
    // 今天这一刻已经过去（含刚好相等）→ 排到明天。
    // 返回过去时刻会让 `sleep` 拿到负数、定时任务空转烧 CPU。
    if local.naive_local() >= today {
        target += chrono::Duration::days(1);
    }
    target
        .and_local_timezone(tz)
        .single()
        .map(|d| d.with_timezone(&chrono::Utc))
        .unwrap_or_else(|| chrono::Utc::now() + chrono::Duration::hours(1))
}

pub fn parse_hhmm(v: &str) -> (u32, u32) {
    let mut it = v.split(':');
    let h = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(3);
    let m = it.next().and_then(|s| s.trim().parse().ok()).unwrap_or(30);
    (h.min(23), m.min(59))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn stamp_round_trips() {
        let t = parse_stamp("20260115-033000").unwrap();
        assert_eq!(t.format("%Y-%m-%d %H:%M:%S").to_string(), "2026-01-15 03:30:00");
    }

    #[test]
    fn bad_stamp_is_none_not_panic() {
        assert!(parse_stamp("not-a-stamp").is_none());
        assert!(parse_stamp("").is_none());
    }

    #[test]
    fn hhmm_falls_back_on_garbage() {
        assert_eq!(parse_hhmm("04:05"), (4, 5));
        assert_eq!(parse_hhmm("garbage"), (3, 30));
        assert_eq!(parse_hhmm("99:99"), (23, 59));
    }

    /// 调度必须永远指向未来，否则定时任务会空转烧 CPU。
    #[test]
    fn next_run_is_always_in_the_future() {
        let now = chrono::Utc::now();
        for h in 0..24u32 {
            let n = next_run(h, 30, "Asia/Shanghai");
            assert!(n > now, "{h}:30 算出了过去时间 {n}");
        }
    }

    #[test]
    fn next_run_picks_tomorrow_when_today_has_passed() {
        // 现在是 UTC，换算到上海时区；如果上海已经过了 3:30，就该排到明天
        let shanghai = chrono::Utc::now().with_timezone(&chrono_tz::Asia::Shanghai);
        let (h, m) = (shanghai.hour(), shanghai.minute());
        let n = next_run(h, m, "Asia/Shanghai");
        assert!(n > chrono::Utc::now());
    }
}
