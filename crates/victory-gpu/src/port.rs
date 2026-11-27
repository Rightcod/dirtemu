use crate::inplace::Placed;
use std::sync::{Mutex, OnceLock};
use windows::Win32::Graphics::Direct3D12::*;
use windows_sys::Win32::System::Memory as mem;

const PAGE: u64 = 64 << 10;
const LOW: u64 = 1 << 32;
const MOST: u64 = 512 << 20;

struct Region {
    start: u64,
    end: u64,
    heap: ID3D12Heap,
}

unsafe impl Send for Region {}

static REGIONS: Mutex<Vec<Region>> = Mutex::new(Vec::new());

pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("VICTORY_PORT_CONSTANTS").is_ok_and(|v| v == "1"))
}

pub(crate) fn place_for(start: u64, len: u64, window: bool) -> Option<Placed> {
    if !on() || len == 0 || start.checked_add(len).is_none_or(|end| end > LOW) {
        return None;
    }
    let host = crate::host::host()?;
    match place_in(&host.device, start, len) {
        Ok(placed) => {
            crate::inplace::counted(window, len);
            Some(placed)
        }
        Err(error) => {
            static REFUSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 8 {
                tracing::warn!(start, len, error, "constants heap stays on the copy path");
            }
            None
        }
    }
}

pub(super) fn place_in(device: &ID3D12Device, start: u64, len: u64) -> Result<Placed, String> {
    let at = start & !(PAGE - 1);
    let end = start
        .checked_add(len.max(1))
        .ok_or("address overflow")?
        .next_multiple_of(PAGE);
    let mut regions = REGIONS.lock().unwrap_or_else(|e| e.into_inner());
    let index = match regions.iter().position(|r| r.start <= at && end <= r.end) {
        Some(index) => index,
        None => {
            regions.push(open_region(device, at, end)?);
            regions.len() - 1
        }
    };
    let region = &regions[index];
    let buffer = crate::inplace::place(device, &region.heap, at - region.start, end - at)
        .map_err(|e| e.to_string())?;
    let gpu = unsafe { buffer.GetGPUVirtualAddress() } + start - at;
    Ok(Placed {
        buffer,
        heap: region.heap.clone(),
        gpu,
        bias: start - at,
    })
}

fn open_region(device: &ID3D12Device, at: u64, end: u64) -> Result<Region, String> {
    let Some((first, bytes, section, mut offset)) =
        crate::inplace::stretch(at as usize, (end - at) as usize)
    else {
        return Err(format!(
            "not contiguous section memory: {:?}",
            crate::inplace::cover(at as usize, (end - at) as usize)
        ));
    };
    let (mut first, mut bytes) = (first as u64, bytes as u64);
    if first < LOW {
        bytes = bytes.min(LOW - first);
    }
    if bytes > MOST {
        offset += at - first;
        (first, bytes) = (at, end - at);
    }
    if bytes > MOST || first > at || first + bytes < end || offset % PAGE != 0 {
        return Err("section extent is outside the constants region".into());
    }
    let view = unsafe {
        mem::MapViewOfFile(
            section as _,
            mem::FILE_MAP_WRITE,
            (offset >> 32) as u32,
            offset as u32,
            bytes as usize,
        )
    };
    if view.Value.is_null() {
        return Err(std::io::Error::last_os_error().to_string());
    }
    let unmap = || unsafe { mem::UnmapViewOfFile(view) };
    let mut here = view.Value as usize;
    while here < view.Value as usize + bytes as usize {
        let mut info: mem::MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { mem::VirtualQuery(here as *const _, &mut info, size_of_val(&info)) } == 0
            || info.State != mem::MEM_COMMIT
        {
            unmap();
            return Err("section pages have not all been committed".into());
        }
        let next = (info.BaseAddress as usize).saturating_add(info.RegionSize);
        if next <= here {
            unmap();
            return Err("invalid section extent".into());
        }
        here = next;
    }
    match crate::inplace::open(
        device,
        crate::inplace::From::Part(view.Value as usize, bytes as usize),
    ) {
        Ok(heap) => {
            tracing::info!(
                start = first,
                bytes,
                "constants section opened in place through a separate view"
            );
            Ok(Region {
                start: first,
                end: first + bytes,
                heap,
            })
        }
        Err(error) => {
            unmap();
            Err(error.to_string())
        }
    }
}

pub(crate) fn forget(at: usize, len: usize) {
    if !on() && !cfg!(test) {
        return;
    }
    let end = at.saturating_add(len) as u64;
    REGIONS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|region| region.end <= at as u64 || region.start >= end);
}
