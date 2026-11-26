use std::sync::atomic::{AtomicU64, Ordering};

use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const RETRY_MS: u64 = 500;
const SAMPLE_MS: u64 = 64;
const UPLOAD_RESERVE: u64 = 512 * MIB;

static RETRY_AT: AtomicU64 = AtomicU64::new(0);
static NEXT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_SAMPLE
            .compare_exchange(next, now.saturating_add(SAMPLE_MS), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn reserve_for(bytes: u64) -> u64 {
    UPLOAD_RESERVE.max(bytes.saturating_mul(2))
}

pub(crate) fn admit_upload(bytes: u64) -> bool {
    let now = crate::host::now_ms();
    if now < RETRY_AT.load(Ordering::Relaxed) {
        return false;
    }
    let commit = sample_commit(now);
    let reserve = reserve_for(bytes);
    if commit < reserve {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(
                bytes_mb = bytes >> 20,
                commit_mb = commit >> 20,
                reserve_mb = reserve >> 20,
                "host commit is low: deferring upload allocations"
            );
        });
        return false;
    }
    true
}

pub(crate) fn allocation_failed() {
    let retry = crate::host::now_ms().saturating_add(RETRY_MS);
    RETRY_AT.fetch_max(retry, Ordering::Relaxed);
}

pub(crate) fn critical_commit() -> bool {
    sample_commit(crate::host::now_ms()) < 1536 * MIB
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_reserve_keeps_room_for_live_allocations() {
        assert_eq!(reserve_for(0), UPLOAD_RESERVE);
        assert_eq!(reserve_for(64 * MIB), UPLOAD_RESERVE);
        assert_eq!(reserve_for(512 * MIB), 1024 * MIB);
    }
}
