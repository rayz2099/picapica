use crate::error::{Error, Result};
use serde::Serialize;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use url::Url;

const SPEED_WINDOW: Duration = Duration::from_secs(1);
const SPEED_BUCKET: Duration = Duration::from_millis(100);

/// 控制面看到的单条活动回源下载。
#[derive(Debug, Clone, Serialize)]
pub struct TransferRow {
    pub id: u64,
    pub repo: String,
    pub namespace: String,
    pub name: String,
    pub upstream: String,
    pub received: u64,
    pub attempt_received: u64,
    pub total: Option<u64>,
    pub speed_bps: u64,
    pub started_at: i64,
    pub attempt_started_at: i64,
}

struct SpeedSample {
    at: Instant,
    bytes: u64,
}

struct TransferState {
    row: TransferRow,
    samples: VecDeque<SpeedSample>,
}

struct RegistryInner {
    next_id: AtomicU64,
    active: Mutex<HashMap<u64, TransferState>>,
}

/// why: 下载进度只描述当前进程活动流，独立于持久化元数据才能保证重启自然清零。
#[derive(Clone)]
pub struct TransferRegistry {
    inner: Arc<RegistryInner>,
}

/// why: 流 future 可能被取消，守卫析构时移除条目可避免控制面残留假活动项。
pub struct TransferGuard {
    registry: TransferRegistry,
    id: u64,
    done: AtomicBool,
}

impl Default for TransferRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(RegistryInner {
                next_id: AtomicU64::new(1),
                active: Mutex::new(HashMap::new()),
            }),
        }
    }
}

impl TransferRegistry {
    pub fn begin(
        &self,
        repo: &str,
        namespace: &str,
        name: &str,
        upstream: &str,
        total: Option<u64>,
    ) -> Result<TransferGuard> {
        let upstream = upstream_host(upstream)?;
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let now = now_secs()?;
        let row = TransferRow {
            id,
            repo: repo.to_string(),
            namespace: namespace.to_string(),
            name: name.to_string(),
            upstream,
            received: 0,
            attempt_received: 0,
            total,
            speed_bps: 0,
            started_at: now,
            attempt_started_at: now,
        };
        self.inner.active.lock().expect("transfer registry").insert(
            id,
            TransferState {
                row,
                samples: VecDeque::new(),
            },
        );
        Ok(TransferGuard {
            registry: self.clone(),
            id,
            done: AtomicBool::new(false),
        })
    }

    pub fn list(&self, limit: u32) -> Vec<TransferRow> {
        let now = Instant::now();
        let mut active = self.inner.active.lock().expect("transfer registry");
        let mut rows: Vec<_> = active
            .values_mut()
            .map(|state| {
                trim_samples(&mut state.samples, now);
                state.row.speed_bps = state
                    .samples
                    .iter()
                    .fold(0u64, |total, sample| total.saturating_add(sample.bytes));
                state.row.clone()
            })
            .collect();
        rows.sort_by_key(|row| std::cmp::Reverse(row.id));
        rows.truncate(limit.clamp(1, 500) as usize);
        rows
    }

    pub fn active_count(&self) -> usize {
        self.inner.active.lock().expect("transfer registry").len()
    }

    /// why: 检查和 prune 必须共用同一把锁，避免检查后新下载插入的竞态。
    pub fn run_if_idle<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        let active = self.inner.active.lock().expect("transfer registry");
        if !active.is_empty() {
            return Err(Error::msg(format!(
                "有 {} 个传输进行中，不允许执行 prune",
                active.len()
            )));
        }
        operation()
    }

    fn received(&self, id: u64, delta: u64) -> Result<()> {
        let now = Instant::now();
        let mut active = self.inner.active.lock().expect("transfer registry");
        let state = active
            .get_mut(&id)
            .ok_or_else(|| Error::msg(format!("传输 {id} 不存在或已结束")))?;
        state.row.received = state.row.received.saturating_add(delta);
        state.row.attempt_received = state.row.attempt_received.saturating_add(delta);
        trim_samples(&mut state.samples, now);
        if let Some(sample) = state.samples.back_mut() {
            if now.saturating_duration_since(sample.at) < SPEED_BUCKET {
                sample.bytes = sample.bytes.saturating_add(delta);
                return Ok(());
            }
        }
        state.samples.push_back(SpeedSample {
            at: now,
            bytes: delta,
        });
        Ok(())
    }

    fn start_attempt(&self, id: u64, upstream: &str, total: Option<u64>) -> Result<()> {
        let upstream = upstream_host(upstream)?;
        let started_at = now_secs()?;
        let mut active = self.inner.active.lock().expect("transfer registry");
        let state = active
            .get_mut(&id)
            .ok_or_else(|| Error::msg(format!("传输 {id} 不存在或已结束")))?;
        state.row.upstream = upstream;
        state.row.attempt_received = 0;
        state.row.total = total;
        state.row.attempt_started_at = started_at;
        state.row.speed_bps = 0;
        state.samples.clear();
        Ok(())
    }

    fn remove(&self, id: u64) {
        self.inner
            .active
            .lock()
            .expect("transfer registry")
            .remove(&id);
    }
}

impl TransferGuard {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn received(&self, delta: u64) -> Result<()> {
        if self.done.load(Ordering::Acquire) {
            return Err(Error::msg(format!("传输 {} 已结束", self.id)));
        }
        self.registry.received(self.id, delta)
    }

    /// why: 上游失败后的新尝试属于同一逻辑下载，累计流量保留而尝试进度必须归零。
    pub fn start_attempt(&self, upstream: &str, total: Option<u64>) -> Result<()> {
        if self.done.load(Ordering::Acquire) {
            return Err(Error::msg(format!("传输 {} 已结束", self.id)));
        }
        self.registry.start_attempt(self.id, upstream, total)
    }

    pub fn finish(&self) -> Result<()> {
        self.complete();
        Ok(())
    }

    pub fn fail(&self, _error: &str) -> Result<()> {
        self.complete();
        Ok(())
    }

    fn complete(&self) {
        if !self.done.swap(true, Ordering::AcqRel) {
            self.registry.remove(self.id);
        }
    }
}

impl Drop for TransferGuard {
    fn drop(&mut self) {
        self.complete();
    }
}

fn trim_samples(samples: &mut VecDeque<SpeedSample>, now: Instant) {
    while samples
        .front()
        .is_some_and(|sample| now.saturating_duration_since(sample.at) >= SPEED_WINDOW)
    {
        samples.pop_front();
    }
}

fn upstream_host(raw: &str) -> Result<String> {
    let url = Url::parse(raw)?;
    url.host_str()
        .map(str::to_string)
        .ok_or_else(|| Error::msg("上游 URL 缺少 host"))
}

fn now_secs() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Error::msg(format!("系统时间早于 UNIX_EPOCH: {error}")))?;
    i64::try_from(elapsed.as_secs()).map_err(|_| Error::msg("系统时间超出 i64 范围"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finish_and_drop_remove_active_transfer() {
        let registry = TransferRegistry::default();
        let first = registry
            .begin(
                "docker",
                "library/demo",
                "layer",
                "https://a.example/v2",
                Some(100),
            )
            .expect("begin transfer");
        first.received(25).expect("record progress");
        assert_eq!(registry.active_count(), 1);
        let row = registry.list(10).pop().expect("active row");
        assert_eq!(row.attempt_received, 25);
        assert_eq!(row.speed_bps, 25);
        first.finish().expect("finish transfer");
        assert_eq!(registry.active_count(), 0);

        let cancelled = registry
            .begin(
                "docker",
                "library/demo",
                "layer",
                "https://a.example/v2",
                None,
            )
            .expect("begin transfer");
        drop(cancelled);
        assert_eq!(registry.active_count(), 0);
    }

    #[test]
    fn next_attempt_keeps_actual_egress_bytes() {
        let registry = TransferRegistry::default();
        let transfer = registry
            .begin(
                "docker",
                "library/demo",
                "layer",
                "https://a.example/v2",
                Some(100),
            )
            .expect("begin transfer");
        transfer.received(25).expect("first attempt progress");
        transfer
            .start_attempt("https://b.example/v2", Some(80))
            .expect("switch upstream");
        transfer.received(10).expect("second attempt progress");

        let row = registry.list(10).pop().expect("active row");
        assert_eq!(row.received, 35);
        assert_eq!(row.attempt_received, 10);
        assert_eq!(row.total, Some(80));
        assert_eq!(row.upstream, "b.example");
    }

    #[test]
    fn prune_gate_rejects_active_transfer() {
        let registry = TransferRegistry::default();
        let _transfer = registry
            .begin(
                "docker",
                "library/demo",
                "layer",
                "https://a.example/v2",
                None,
            )
            .expect("begin transfer");

        let error = registry
            .run_if_idle(|| Ok(()))
            .expect_err("active transfer must block prune");
        assert!(error.to_string().contains("传输进行中"));
    }
}
