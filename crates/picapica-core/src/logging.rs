use crate::config::Config;
use crate::error::{Error, Result};
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::fmt;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

const LOG_PREFIX: &str = "picapica";

/// why: 文件日志只描述当前进程，目录跟 data_dir 走，避免再引入一套路径约定。
pub fn install(cfg: &Config) -> Result<Option<WorkerGuard>> {
    let level = parse_log_level(&cfg.log_level)?;
    let filter = EnvFilter::new(format!("picapica={level},picapica_core={level}"));
    let stdout = fmt::layer();
    match cfg.log_retain_std()? {
        None => {
            tracing_subscriber::registry()
                .with(filter)
                .with(stdout)
                .init();
            Ok(None)
        }
        Some(retain) => {
            let dir = cfg.data_dir.join("logs");
            fs::create_dir_all(&dir)
                .map_err(|e| Error::msg(format!("创建日志目录 {} 失败: {e}", dir.display())))?;
            prune_logs(&dir, LOG_PREFIX, retain)?;
            let appender = tracing_appender::rolling::daily(&dir, LOG_PREFIX);
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let file = fmt::layer().with_ansi(false).with_writer(writer);
            // why: 配了文件保留就不要再刷终端，前台启动也不会滚屏。
            tracing_subscriber::registry()
                .with(filter)
                .with(file)
                .init();
            Ok(Some(guard))
        }
    }
}

pub fn parse_log_level(raw: &str) -> Result<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => Ok("error"),
        "warn" => Ok("warn"),
        "info" => Ok("info"),
        "debug" => Ok("debug"),
        "trace" => Ok("trace"),
        other => Err(Error::msg(format!("log_level 非法: {other}"))),
    }
}

/// why: 按文件修改时间裁掉超出窗口的滚动日志，避免 warn 文件无限堆积。
pub fn prune_logs(dir: &Path, prefix: &str, retain: Duration) -> Result<()> {
    let cutoff = SystemTime::now()
        .checked_sub(retain)
        .ok_or_else(|| Error::msg("log_retain 过大"))?;
    let needle = format!("{prefix}.");
    let entries = fs::read_dir(dir)
        .map_err(|e| Error::msg(format!("读日志目录 {} 失败: {e}", dir.display())))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::msg(format!("读日志目录 {} 失败: {e}", dir.display())))?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(&needle) {
            continue;
        }
        let meta = entry
            .metadata()
            .map_err(|e| Error::msg(format!("读日志 {} 失败: {e}", entry.path().display())))?;
        let modified = meta
            .modified()
            .map_err(|e| Error::msg(format!("读日志时间 {} 失败: {e}", entry.path().display())))?;
        if modified < cutoff {
            fs::remove_file(entry.path()).map_err(|e| {
                Error::msg(format!("删除日志 {} 失败: {e}", entry.path().display()))
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::Duration;

    static SEQ: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "picapica-log-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).expect("temp log dir");
        dir
    }

    #[test]
    fn parse_log_level_accepts_known_values() {
        assert_eq!(parse_log_level("WARN").expect("warn"), "warn");
        assert!(parse_log_level("verbose").is_err());
    }

    #[test]
    fn prune_logs_deletes_stale_prefix_files_only() {
        let dir = temp_dir();
        let stale = dir.join("picapica.2020-01-01");
        let keep = dir.join("picapica.keep");
        let other = dir.join("notes.txt");
        fs::write(&stale, b"old").expect("stale");
        fs::write(&other, b"other").expect("other");
        thread::sleep(Duration::from_millis(20));
        fs::write(&keep, b"new").expect("keep");
        prune_logs(&dir, "picapica", Duration::from_millis(10)).expect("prune");
        assert!(!stale.exists());
        assert!(keep.exists());
        assert!(other.exists());
    }
}
