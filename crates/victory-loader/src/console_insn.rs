//! Instructions the console's CPU (Zen 2) has and an Intel PC's lacks, emulated: the title's
//! code raises STATUS_ILLEGAL_INSTRUCTION on them (the title zeroes memory with a `clzero` loop at
//! its first frames), and the vectored handler steps over them here.
//!
//! Ported from XCL (github.com/ivall/xcl, GPL-2.0-only): `insn.rs` (decoder, as
//! `console_insn_decode.rs`, unchanged) and `eh.rs` `emulate_console_only`.
//!
//! What Windows said about memory is remembered per thread: a region found writable for a
//! bounded number of answers (sixteen regions, each expiring on its own budget), and how many
//! bytes are readable at a faulting address, from one query instead of up to seventeen. The
//! bytes are decoded on every fault: measured in release, a decoded-instruction cache with a
//! byte fingerprint was slower than decoding again (decode is a handful of cycles; the TLS
//! lookup, fingerprint compare and cache scan cost more), so none is kept. The "first time
//! seen" warning is per thread a plain bit test, and only its first sighting per kind touches
//! a shared atomic. Per-thread counts are folded into the run counters once per 1024
//! emulations instead of one atomic each.

#[path = "console_insn_decode.rs"]
mod decode;

use decode::ConsoleOnly;
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
use windows_sys::Win32::System::Diagnostics::Debug::CONTEXT;

const CONTINUE_EXECUTION: i32 = -1;

/// The longest instruction the decoder is given (x86-64 instructions are at most 15 bytes).
const MAX_INSN: usize = 16;

/// Emulations a thread finishes before its counts are folded into the shared ones.
const FLUSH_EVERY: u64 = 1024;

/// Whether what Windows said about memory is remembered per thread (the regions it found
/// writable, the code bytes it found readable) and not asked again at every emulated
/// instruction. A query walks the pages of the region it answers for: asked from outside, it
/// took 2-9 ms in the title's heaps of 0.6-2.2 GB and 89 ms in its 6.2 GB page pool (run 30),
/// and the title zeroes blocks of them with `clzero` loops all through a frame: six of its
/// threads spent 25-78 % of their time inside that query (runs 29 and 30), about 27 s of
/// processor time in Windows per 9 s frame. On by default; `VICTORY_CLZERO_ASK=1` asks every
/// time, as before.
fn remembers() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CLZERO_ASK").is_none_or(|v| v != "1"))
}

/// Answers a remembered region gives before Windows is asked about it again: one freed or
/// protected meanwhile is forgotten then (per region, not for the whole cache as before).
const ANSWERS: u32 = 4096;

/// Regions remembered per thread. The title maps its page pool as many one-page views, and a
/// `clzero` loop walks through them region by region; sixteen keep a working set of views
/// without asking Windows again on every pass.
const SPANS: usize = 16;

/// Faulting addresses whose readable length is remembered per thread, newest first.
const KNOWN: usize = 8;

/// Single-entry decode cache for repeated faults at the same instruction address.
/// Safe for the hot AMD-only loops where the faulting RIP stays stable.
#[derive(Clone, Copy)]
struct LastDecode {
    rip: usize,
    bytes: [u8; MAX_INSN],
    len: u8,
    insn: Option<(ConsoleOnly, usize)>,
}

impl LastDecode {
    const EMPTY: LastDecode = LastDecode {
        rip: 0,
        bytes: [0; MAX_INSN],
        len: 0,
        insn: None,
    };
}

#[derive(Clone, Copy)]
struct Span {
    from: usize,
    to: usize,
    answers: u32,
}

const EMPTY_SPAN: Span = Span {
    from: 0,
    to: 0,
    answers: 0,
};

struct WritableCache {
    spans: [Span; SPANS],
}

impl WritableCache {
    const fn new() -> Self {
        Self {
            spans: [EMPTY_SPAN; SPANS],
        }
    }

    /// The end of the remembered region `va` lies in, newest first; the answer that exhausts
    /// a region's budget forgets it, so the next one asks Windows again.
    fn find(&mut self, va: usize, end: usize) -> Option<usize> {
        for i in 0..self.spans.len() {
            let span = self.spans[i];
            if span.from <= va && va < span.to {
                if span.answers >= ANSWERS {
                    self.spans[i] = EMPTY_SPAN;
                    return None;
                }
                let to = span.to.min(end);
                self.spans[i].answers += 1;
                self.spans[..=i].rotate_right(1);
                return Some(to);
            }
        }
        None
    }

    fn insert(&mut self, from: usize, to: usize) {
        self.spans.rotate_right(1);
        self.spans[0] = Span {
            from,
            to,
            answers: 0,
        };
    }
}

/// The readable lengths of faulting addresses, newest first; `bytes[i]` of zero marks an
/// empty slot. Two arrays keep the copy out of the thread-local in `Known::get` small.
#[derive(Clone, Copy)]
struct Known {
    at: [usize; KNOWN],
    bytes: [u8; KNOWN],
}

impl Known {
    const EMPTY: Known = Known {
        at: [0; KNOWN],
        bytes: [0; KNOWN],
    };

    fn find(&self, at: usize) -> Option<usize> {
        let i = self
            .at
            .iter()
            .position(|&known_at| known_at != 0 && known_at == at)?;
        let n = self.bytes[i];
        (n != 0).then_some(usize::from(n))
    }

    fn remember(&mut self, at: usize, bytes: usize) {
        self.at.rotate_right(1);
        self.bytes.rotate_right(1);
        self.at[0] = at;
        self.bytes[0] = bytes as u8;
    }
}

/// Per-thread counters; folded into the shared ones every [`FLUSH_EVERY`] emulations.
#[derive(Clone, Copy)]
struct Counters {
    emulated: u64,
    queries: u64,
    decoded: u64,
}

impl Counters {
    const ZERO: Counters = Counters {
        emulated: 0,
        queries: 0,
        decoded: 0,
    };
}

thread_local! {
    /// The regions Windows last said are committed and writable, newest first, each with the
    /// answers it gave since it was inserted.
    static WRITABLE: RefCell<WritableCache> = const { RefCell::new(WritableCache::new()) };
    /// Faulting addresses and how many bytes were found readable there.
    static READABLE: Cell<Known> = const { Cell::new(Known::EMPTY) };
    /// Last decoded instruction at a faulting RIP.
    static LAST_DECODE: Cell<LastDecode> = const { Cell::new(LastDecode::EMPTY) };
    /// Instruction kinds this thread has seen (and logged).
    static SEEN_LOCAL: Cell<u32> = const { Cell::new(0) };
    static COUNTERS: Cell<Counters> = const { Cell::new(Counters::ZERO) };
}

/// Instructions emulated, the times Windows was asked about memory, the decodes made, and the
/// bytes `clzero` loops zeroed, since the start.
static EMULATED: AtomicU64 = AtomicU64::new(0);
static ASKED: AtomicU64 = AtomicU64::new(0);
static DECODED: AtomicU64 = AtomicU64::new(0);
static ZEROED: AtomicU64 = AtomicU64::new(0);
/// Instruction kinds seen by any thread; a bit is set on the first sighting.
static SEEN: AtomicU32 = AtomicU32::new(0);

fn counters_edit(edit: impl FnOnce(&mut Counters)) {
    COUNTERS.with(|cell| {
        let mut counts = cell.get();
        edit(&mut counts);
        if counts.emulated >= FLUSH_EVERY {
            flush(&mut counts);
        }
        cell.set(counts);
    });
}

fn note_query() {
    counters_edit(|c| c.queries += 1);
}

fn note_decoded() {
    counters_edit(|c| c.decoded += 1);
}

fn note_emulated() {
    counters_edit(|c| c.emulated += 1);
}

/// Folds a thread's counts into the shared ones; logs at the powers of two from 2^16 on.
fn flush(counts: &mut Counters) {
    let batch = std::mem::replace(counts, Counters::ZERO);
    let before = EMULATED.fetch_add(batch.emulated, Relaxed);
    ASKED.fetch_add(batch.queries, Relaxed);
    DECODED.fetch_add(batch.decoded, Relaxed);
    let after = before + batch.emulated;
    let checkpoint = before.saturating_add(1).max(1 << 16).next_power_of_two();
    if after < checkpoint {
        return;
    }
    let emulated = EMULATED.load(Relaxed);
    let decoded = DECODED.load(Relaxed);
    tracing::info!(
        emulated,
        asked = ASKED.load(Relaxed),
        decoded,
        decoded_pct = decoded.saturating_mul(100) / emulated.max(1),
        zeroed_mb = ZEROED.load(Relaxed) >> 20,
        "console CPU instructions emulated so far, the times Windows was asked about memory for them, and what clzero loops zeroed"
    );
}

/// Windows' description of the memory `va` lies in, one query.
fn memory(va: usize) -> Option<windows_sys::Win32::System::Memory::MEMORY_BASIC_INFORMATION> {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let found = unsafe { VirtualQuery(va as *const _, &mut m, std::mem::size_of_val(&m)) } != 0;
    note_query();
    found.then_some(m)
}

/// How many bytes (at most [`MAX_INSN`]) of the instruction at `va` are readable: one query
/// answers what the old probe loop asked at most seventeen times. The old loop required the
/// one region containing `va` to reach `va + n`, so a region boundary still ends the bytes.
fn readable_len(va: usize) -> usize {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_GUARD, PAGE_NOACCESS};
    let Some(m) = memory(va) else {
        return 0;
    };
    if m.State != MEM_COMMIT || m.Protect & (PAGE_NOACCESS | PAGE_GUARD) != 0 {
        return 0;
    }
    let end = m.BaseAddress as usize + m.RegionSize;
    end.saturating_sub(va).min(MAX_INSN)
}

fn writable(va: usize, bytes: usize) -> bool {
    writable_to(va, va + bytes) == Some(va + bytes)
}

/// Where the committed, writable memory that `va` lies in ends, at most `end`; None if `va`
/// is not in such memory. A region found is remembered ([`remembers`]).
/// shortcut: a remembered region that was freed or protected since is written all the same
/// for up to [`ANSWERS`] answers: the write then faults inside the handler, which the guard
/// on view memory opens as it does any write, and which is a crash otherwise (as the
/// unchecked single-line emulation behind this one already is).
fn writable_to(va: usize, end: usize) -> Option<usize> {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, PAGE_EXECUTE_READWRITE, PAGE_READWRITE};
    let remember = remembers();
    if remember {
        let found = WRITABLE.with_borrow_mut(|cache| cache.find(va, end));
        if let Some(to) = found {
            return Some(to);
        }
    }
    let m = memory(va)?;
    if m.State != MEM_COMMIT || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0 {
        return None;
    }
    let (from, to) = (
        m.BaseAddress as usize,
        m.BaseAddress as usize + m.RegionSize,
    );
    if remember {
        WRITABLE.with_borrow_mut(|cache| cache.insert(from, to));
    }
    Some(to.min(end))
}

/// How many bytes at `rip` are readable: remembered per thread (the readable length does not
/// change when an instruction's bytes are patched to NOPs), from one query when not.
fn readable(rip: usize) -> Option<usize> {
    if remembers() {
        let known = READABLE.with(Cell::get);
        if let Some(n) = known.find(rip) {
            return Some(n);
        }
    }
    let n = readable_len(rip);
    if n == 0 {
        return None;
    }
    if remembers() {
        READABLE.with(|cell| {
            let mut known = cell.get();
            known.remember(rip, n);
            cell.set(known);
        });
    }
    Some(n)
}

/// The console-only instruction at `rip`, decoded from the bytes found readable there;
/// `VICTORY_CLZERO_ASK=1` asks Windows every time as before.
fn classify(rip: usize) -> Option<(ConsoleOnly, usize)> {
    if remembers() {
        let cached = LAST_DECODE.with(Cell::get);
        if cached.rip == rip && cached.len != 0 {
            // SAFETY: bytes were readable when cached; for self-modifying code the cache
            // is validated by byte comparison below on miss or by the readable probe.
            let code = &cached.bytes[..usize::from(cached.len)];
            let current = unsafe { std::slice::from_raw_parts(rip as *const u8, code.len()) };
            if current == code {
                return cached.insn;
            }
        }
    }
    let n = readable(rip)?;
    // SAFETY: `n` bytes at `rip` are readable.
    let code = unsafe { std::slice::from_raw_parts(rip as *const u8, n) };
    note_decoded();
    let insn = decode::decode(code);
    if remembers() {
        let mut bytes = [0u8; MAX_INSN];
        bytes[..n].copy_from_slice(code);
        LAST_DECODE.with(|cell| {
            cell.set(LastDecode {
                rip,
                bytes,
                len: n as u8,
                insn,
            })
        });
    }
    insn
}

/// The bit standing for an instruction kind in [`SEEN`] (variants with fields share one).
fn kind(what: ConsoleOnly) -> u32 {
    match what {
        ConsoleOnly::Clwb => 0,
        ConsoleOnly::Clzero { .. } => 1,
        ConsoleOnly::ClzeroLoop { .. } => 2,
        ConsoleOnly::Rdpru => 3,
        ConsoleOnly::Monitorx => 4,
        ConsoleOnly::Mwaitx => 5,
        ConsoleOnly::Rdpid(_) => 6,
        ConsoleOnly::Extrq { .. } => 7,
        ConsoleOnly::Insertq { .. } => 8,
        ConsoleOnly::Ud2 => 9,
    }
}

/// Whether this is the first sighting of `kind` in the process; after the first, a per-thread
/// bit makes the check a plain load, with no atomic and no lock (the old check locked a mutex
/// and scanned a vector on every emulated instruction).
fn first_sighting_kind(kind: u32) -> bool {
    let bit = 1u32 << (kind & 31);
    if SEEN_LOCAL.with(Cell::get) & bit != 0 {
        return false;
    }
    let first = SEEN.fetch_or(bit, Relaxed) & bit == 0;
    SEEN_LOCAL.with(|seen| seen.set(seen.get() | bit));
    first
}

fn first_sighting(what: ConsoleOnly) -> bool {
    first_sighting_kind(kind(what))
}

/// An illegal instruction at `ctx.Rip`: a console-only one is emulated and stepped over
/// (`CONTINUE_EXECUTION`); anything else returns 0 (the crash report and the title's handlers
/// see it).
///
/// # Safety
/// `ctx` is the faulting thread's context, from the exception being dispatched.
pub(crate) unsafe fn emulate(ctx: &mut CONTEXT) -> i32 {
    let rip = ctx.Rip as usize;
    let Some((what, len)) = classify(rip) else {
        return 0;
    };
    if first_sighting(what) && what != ConsoleOnly::Ud2 {
        tracing::warn!(instruction = ?what, at = format_args!("{rip:#x}"), "a console CPU instruction this CPU lacks: emulated (logged once)");
    }
    match what {
        ConsoleOnly::Ud2 => return 0,
        ConsoleOnly::Clwb => {
            // nothing to write back on the PC: patched to NOPs, no exception next time
            use windows_sys::Win32::System::Memory::{PAGE_EXECUTE_READWRITE, VirtualProtect};
            let mut old = 0u32;
            // SAFETY: the instruction's own bytes, made writable for the patch and restored.
            unsafe {
                if VirtualProtect(rip as *const _, len, PAGE_EXECUTE_READWRITE, &mut old) != 0 {
                    std::slice::from_raw_parts_mut(rip as *mut u8, len).fill(0x90);
                    VirtualProtect(rip as *const _, len, old, &mut old);
                }
            }
        }
        ConsoleOnly::Monitorx => {}
        ConsoleOnly::Mwaitx => std::hint::spin_loop(),
        ConsoleOnly::Clzero { a32 } => {
            let _ = a32;
            // with an address-size prefix (0x67) the line is at EAX, zero-extended
            // SAFETY: the `len` bytes classified at `rip` are readable.
            let code = unsafe { std::slice::from_raw_parts(rip as *const u8, len) };
            let a32 = code[..len - 3].contains(&0x67);
            let line = (if a32 { ctx.Rax & 0xFFFF_FFFF } else { ctx.Rax } as usize) & !63;
            if !writable(line, 64) {
                return 0; // the real instruction faults there too
            }
            // SAFETY: the 64-byte line is committed and writable.
            unsafe { std::ptr::write_bytes(line as *mut u8, 0, 64) };
        }
        ConsoleOnly::ClzeroLoop { end_reg } => {
            // the whole `clzero; add rax,40h; cmp rax,end; jb` loop at once
            let regs: [u64; 16] = [
                ctx.Rax, ctx.Rcx, ctx.Rdx, ctx.Rbx, ctx.Rsp, ctx.Rbp, ctx.Rsi, ctx.Rdi, ctx.R8,
                ctx.R9, ctx.R10, ctx.R11, ctx.R12, ctx.R13, ctx.R14, ctx.R15,
            ];
            let end = regs[end_reg as usize & 15] as usize;
            let start = ctx.Rax as usize;
            if end > start && end - start < 2 << 30 {
                // Region by region: a block may lie across several (the title's pages are
                // mapped one view each), and one answer for all of it was a refusal, after
                // which every 64-byte line took an exception of its own.
                let mut at = start;
                while at < end {
                    let Some(to) = writable_to(at, end) else {
                        break;
                    };
                    // SAFETY: committed and writable from `at` to `to`.
                    unsafe { std::ptr::write_bytes(at as *mut u8, 0, to - at) };
                    at = to;
                }
                if at == start {
                    return 0;
                }
                ZEROED.fetch_add((at - start) as u64, Relaxed);
                if at < end {
                    // What follows is not writable: the loop goes on there (the same
                    // instruction again), where the real one faults too.
                    ctx.Rax = at as u64;
                    note_emulated();
                    return CONTINUE_EXECUTION;
                }
            }
            ctx.Rax = end as u64;
        }
        ConsoleOnly::Rdpru => {
            let mut v = 0i64;
            // SAFETY: a valid out pointer.
            unsafe { windows_sys::Win32::System::Performance::QueryPerformanceCounter(&mut v) };
            let v = if ctx.Rcx as u32 <= 1 { v as u64 } else { 0 };
            ctx.Rax = v & 0xFFFF_FFFF;
            ctx.Rdx = v >> 32;
        }
        ConsoleOnly::Rdpid(r) => {
            // SAFETY: no preconditions.
            let cpu = unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessorNumber() }
                as u64;
            let regs: [&mut u64; 16] = [
                &mut ctx.Rax,
                &mut ctx.Rcx,
                &mut ctx.Rdx,
                &mut ctx.Rbx,
                &mut ctx.Rsp,
                &mut ctx.Rbp,
                &mut ctx.Rsi,
                &mut ctx.Rdi,
                &mut ctx.R8,
                &mut ctx.R9,
                &mut ctx.R10,
                &mut ctx.R11,
                &mut ctx.R12,
                &mut ctx.R13,
                &mut ctx.R14,
                &mut ctx.R15,
            ];
            *regs[r as usize & 15] = cpu;
        }
        // the low quadword changes; the high one is zeroed, as a Zen 5 does
        ConsoleOnly::Extrq { dst, src, len, idx } => {
            // SAFETY: the context's XMM registers (CONTEXT_FLOATING_POINT is captured for exceptions).
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let (len, idx) = match src {
                Some(s) => {
                    let c = x[s as usize & 15].Low;
                    (c as u8, (c >> 8) as u8)
                }
                None => (len, idx),
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::extrq(d.Low, len, idx);
            d.High = 0;
        }
        ConsoleOnly::Insertq {
            dst,
            src,
            reg,
            len,
            idx,
        } => {
            // SAFETY: as above.
            let x = unsafe { &mut ctx.Anonymous.FltSave.XmmRegisters };
            let s = x[src as usize & 15];
            let (len, idx) = if reg {
                (s.High as u8, (s.High >> 8) as u8)
            } else {
                (len, idx)
            };
            let d = &mut x[dst as usize & 15];
            d.Low = decode::insertq(d.Low, s.Low, len, idx);
            d.High = 0;
        }
    }
    ctx.Rip += len as u64;
    note_emulated();
    CONTINUE_EXECUTION
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hint::black_box;
    use std::time::Instant;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MEMORY_BASIC_INFORMATION, PAGE_EXECUTE_READWRITE,
        PAGE_NOACCESS, PAGE_READWRITE, VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery,
    };

    fn counters() -> Counters {
        COUNTERS.with(Cell::get)
    }

    fn reset_counters() {
        COUNTERS.with(|cell| cell.set(Counters::ZERO));
    }

    fn forget_caches() {
        READABLE.with(|known| known.set(Known::EMPTY));
        WRITABLE.with_borrow_mut(|cache| *cache = WritableCache::new());
        LAST_DECODE.with(|cell| cell.set(LastDecode::EMPTY));
        SEEN_LOCAL.with(|seen| seen.set(0));
    }

    /// `hits` out of `hits + misses`, as the microbench lines report it.
    fn percent(hits: u64, misses: u64) -> u64 {
        hits.saturating_mul(100) / hits.saturating_add(misses).max(1)
    }

    /// The previous readable probe: one query to check the first byte, then the largest `n` of
    /// 1..=16 whose region reaches `va + n`, counted.
    fn old_readable(va: usize, bytes: usize, queries: &Cell<u64>) -> bool {
        queries.set(queries.get() + 1);
        // SAFETY: plain data filled in by the query.
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let found = unsafe { VirtualQuery(va as *const _, &mut m, std::mem::size_of_val(&m)) } != 0;
        found
            && m.State == MEM_COMMIT
            && m.Protect & (PAGE_NOACCESS | windows_sys::Win32::System::Memory::PAGE_GUARD) == 0
            && m.BaseAddress as usize + m.RegionSize >= va + bytes
    }

    fn old_readable_len(va: usize, queries: &Cell<u64>) -> usize {
        if !old_readable(va, 1, queries) {
            return 0;
        }
        (1..=MAX_INSN)
            .rev()
            .find(|&n| old_readable(va, n, queries))
            .unwrap_or(0)
    }

    /// The previous instruction cache: readable lengths only, decode on every fault.
    struct OldCodeCache {
        known: [(usize, usize); KNOWN],
    }

    impl OldCodeCache {
        const fn new() -> Self {
            Self {
                known: [(0, 0); KNOWN],
            }
        }

        fn classify(&mut self, rip: usize, queries: &Cell<u64>) -> Option<(ConsoleOnly, usize)> {
            let n = match self.known.iter().find(|&&(at, n)| at == rip && n > 0) {
                Some(&(_, n)) => n,
                None => {
                    if !old_readable(rip, 1, queries) {
                        return None;
                    }
                    let n = (1..=MAX_INSN)
                        .rev()
                        .find(|&n| old_readable(rip, n, queries))?;
                    self.known.rotate_right(1);
                    self.known[0] = (rip, n);
                    n
                }
            };
            // SAFETY: `n` bytes at `rip` are readable.
            let code = unsafe { std::slice::from_raw_parts(rip as *const u8, n) };
            decode::decode(code)
        }
    }

    /// The previous writable region query, without any cache.
    fn old_writable_to(va: usize, end: usize, queries: &Cell<u64>) -> Option<usize> {
        queries.set(queries.get() + 1);
        // SAFETY: plain data filled in by the query.
        let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        let found = unsafe { VirtualQuery(va as *const _, &mut m, std::mem::size_of_val(&m)) } != 0;
        if !found
            || m.State != MEM_COMMIT
            || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0
        {
            return None;
        }
        let to = m.BaseAddress as usize + m.RegionSize;
        Some(to.min(end))
    }

    /// The previous region cache: four entries, one answer budget for all of them, no move to
    /// front on a hit.
    struct OldWritableCache {
        spans: [(usize, usize); 4],
        answers: u32,
    }

    impl OldWritableCache {
        const fn new() -> Self {
            Self {
                spans: [(0, 0); 4],
                answers: 0,
            }
        }

        fn writable_to(&mut self, va: usize, end: usize, queries: &Cell<u64>) -> Option<usize> {
            if let Some(&(_, to)) = self.spans.iter().find(|&&(from, to)| from <= va && va < to) {
                if self.answers >= ANSWERS {
                    self.spans = [(0, 0); 4];
                    self.answers = 0;
                } else {
                    self.answers += 1;
                }
                return Some(to.min(end));
            }
            queries.set(queries.get() + 1);
            // SAFETY: plain data filled in by the query.
            let mut m: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
            // SAFETY: a valid out pointer of the right size.
            let found =
                unsafe { VirtualQuery(va as *const _, &mut m, std::mem::size_of_val(&m)) } != 0;
            if !found
                || m.State != MEM_COMMIT
                || m.Protect & (PAGE_READWRITE | PAGE_EXECUTE_READWRITE) == 0
            {
                return None;
            }
            let (from, to) = (
                m.BaseAddress as usize,
                m.BaseAddress as usize + m.RegionSize,
            );
            self.spans.rotate_right(1);
            self.spans[0] = (from, to);
            Some(to.min(end))
        }
    }

    struct Pages {
        at: *mut u8,
        len: usize,
    }

    impl Pages {
        fn new(len: usize) -> Pages {
            // SAFETY: a fresh reservation of `len` bytes.
            let at = unsafe {
                VirtualAlloc(
                    std::ptr::null(),
                    len,
                    MEM_COMMIT | MEM_RESERVE,
                    PAGE_READWRITE,
                )
            }
            .cast::<u8>();
            assert!(!at.is_null(), "VirtualAlloc failed");
            Pages { at, len }
        }

        fn protect(&self, offset: usize, len: usize, protect: u32) {
            let mut old = 0u32;
            // SAFETY: inside the allocation, made inaccessible (restored only by free).
            let ok = unsafe { VirtualProtect(self.at.add(offset).cast(), len, protect, &mut old) };
            assert_ne!(ok, 0, "VirtualProtect failed");
        }
    }

    impl Drop for Pages {
        fn drop(&mut self) {
            // SAFETY: the reservation from `new`, released whole.
            unsafe { VirtualFree(self.at.cast(), 0, MEM_RELEASE) };
        }
    }

    fn pattern(bytes: &[u8]) -> [u8; MAX_INSN] {
        let mut b = [0xcc; MAX_INSN];
        b[..bytes.len()].copy_from_slice(bytes);
        b
    }

    const PATTERNS: &[&[u8]] = &[
        &[0x0f, 0x01, 0xfc],
        &[
            0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x4c, 0x39, 0xd0, 0x72, 0xf4,
        ],
        &[0x67, 0x0f, 0x01, 0xfc],
        &[0x0f, 0x01, 0xfd],
        &[0x0f, 0x01, 0xfa],
        &[0x0f, 0x01, 0xfb],
        &[0x0f, 0x0b],
        &[0x66, 0x0f, 0xae, 0x74, 0x24, 0x08],
        &[0xf3, 0x41, 0x0f, 0xc7, 0xf9],
        &[0xf2, 0x0f, 0x78, 0xc1, 0x08, 0x10],
        &[0x66, 0x41, 0x0f, 0x78, 0xc1, 0x08, 0x04],
        &[0x48, 0x8b, 0x01],
        &[0x0f],
        &[0x66, 0x0f, 0xae, 0x74, 0x24],
    ];

    /// The remembered readable length answers what the old probe asked, also when another
    /// pattern is written to the same address between faults (the bytes are decoded afresh).
    #[test]
    fn decoded_instructions_match_the_uncached_decode() {
        if !remembers() {
            return;
        }
        let mut code = Box::new([0u8; MAX_INSN]);
        let at = code.as_ptr() as usize;
        let mut old = OldCodeCache::new();
        let queries = Cell::new(0);
        forget_caches();
        for bytes in PATTERNS {
            *code = pattern(bytes);
            let fresh = decode::decode(&code[..]);
            let cached_before = classify(at);
            assert_eq!(cached_before, fresh, "{bytes:02x?}");
            assert_eq!(
                classify(at),
                fresh,
                "second fault at the same address: {bytes:02x?}"
            );
            old.known = [(0, 0); KNOWN];
            assert_eq!(
                old.classify(at, &queries),
                fresh,
                "old decoder: {bytes:02x?}"
            );
        }
    }

    /// Distinct addresses across the cache also agree with the old decoder, including a
    /// pattern at the very end of a page where only four bytes are readable.
    #[test]
    fn decoded_instructions_match_across_cache_entries_and_pages() {
        let pages = Pages::new(2 * 4096);
        pages.protect(4096, 4096, PAGE_NOACCESS);
        let tail = unsafe { pages.at.add(4096 - 4) };
        // SAFETY: the last four bytes of the first page are ours and writable.
        unsafe { std::ptr::copy_nonoverlapping([0x0f, 0x01, 0xfd, 0x90].as_ptr(), tail, 4) };
        let queries = Cell::new(0);
        forget_caches();
        assert_eq!(readable_len(tail as usize), 4);
        assert_eq!(old_readable_len(tail as usize, &queries), 4);
        let fresh = decode::decode(&[0x0f, 0x01, 0xfd]);
        assert_eq!(classify(tail as usize), fresh);
        assert_eq!(classify(tail as usize), fresh);
        // SAFETY: base of the allocation from `Pages`.
        let base = unsafe { pages.at.add(4) };
        for (i, bytes) in PATTERNS.iter().enumerate() {
            // SAFETY: inside the writable first page, away from its end.
            unsafe {
                std::ptr::copy_nonoverlapping(pattern(bytes).as_ptr(), base.add(i * 16), MAX_INSN)
            };
        }
        for (i, bytes) in PATTERNS.iter().enumerate() {
            let at = unsafe { base.add(i * 16) as usize };
            // SAFETY: the 16 bytes written above.
            let want = decode::decode(unsafe { std::slice::from_raw_parts(at as *const u8, 16) });
            assert_eq!(classify(at), want, "{bytes:02x?}");
        }
    }

    /// One query now answers the readable length the old loop asked up to seventeen times.
    #[test]
    fn one_query_replaces_the_probe_loop() {
        let pages = Pages::new(2 * 4096);
        pages.protect(4096, 4096, PAGE_NOACCESS);
        let tail = unsafe { pages.at.add(4096 - 4) } as usize;
        let queries = Cell::new(0);
        reset_counters();
        assert_eq!(readable_len(tail), 4);
        let new_queries = counters().queries;
        assert_eq!(old_readable_len(tail, &queries), 4);
        let old_queries = queries.get();
        assert_eq!(new_queries, 1, "new probe");
        assert_eq!(old_queries, 14, "old probe");
        let saved = 100 - percent(new_queries, old_queries - new_queries);
        eprintln!(
            "readable length: {old_queries} VirtualQuery calls before, {new_queries} now ({saved}% fewer)"
        );
    }

    /// The region cache answers exactly what the uncached query answers, in and out of a
    /// region and across a guard page.
    #[test]
    fn writable_regions_match_the_uncached_query() {
        if !remembers() {
            return;
        }
        let pages = Pages::new(2 * 4096);
        pages.protect(4096, 4096, PAGE_NOACCESS);
        let base = pages.at as usize;
        let queries = Cell::new(0);
        for (va, end) in [
            (base, base + 64),
            (base + 64, base + 4096),
            (base + 4092, base + 8192),
            (base + 4096, base + 4160),
            (base + 8192 + 64, base + 8192 + 128),
            (0, 64),
        ] {
            forget_caches();
            assert_eq!(
                writable_to(va, end),
                old_writable_to(va, end, &queries),
                "at {va:#x} to {end:#x}"
            );
            // A second answer from the cache is the same answer.
            assert_eq!(writable_to(va, end), old_writable_to(va, end, &queries));
        }
        if remembers() {
            assert!(writable(base, 64));
            assert!(!writable(base + 4096, 64));
        }
    }

    /// Sixteen remembered regions keep a working set of the title's one-page views; the old
    /// four entry cache thrashed on the same accesses.
    #[test]
    fn writable_cache_covers_more_regions_than_before() {
        if !remembers() {
            return;
        }
        let pages: Vec<Pages> = (0..8).map(|_| Pages::new(4096)).collect();
        let addresses: Vec<usize> = pages.iter().map(|p| p.at as usize).collect();
        let rounds = 10;
        forget_caches();
        reset_counters();
        for _ in 0..rounds {
            for &at in &addresses {
                assert_eq!(writable_to(at, at + 64), Some(at + 64));
            }
        }
        let new_queries = counters().queries;
        let mut old = OldWritableCache::new();
        let old_queries = Cell::new(0);
        for _ in 0..rounds {
            for &at in &addresses {
                assert_eq!(old.writable_to(at, at + 64, &old_queries), Some(at + 64));
            }
        }
        if old_queries.get() > new_queries {
            let saved = 100 - percent(new_queries, old_queries.get() - new_queries);
            eprintln!(
                "eight-region working set: {} VirtualQuery calls before, {} now ({}% fewer)",
                old_queries.get(),
                new_queries,
                saved
            );
        }
        assert!(
            new_queries <= addresses.len() as u64,
            "{new_queries} queries for {} regions",
            addresses.len()
        );
    }

    /// A repeated fault at one address asks Windows once; another address asks again.
    #[test]
    fn readable_length_is_remembered_per_address() {
        if !remembers() {
            return;
        }
        let pages = Pages::new(4096);
        // SAFETY: the fresh, writable page.
        unsafe { std::ptr::copy_nonoverlapping([0x0f, 0x01, 0xfd].as_ptr(), pages.at, 3) };
        let at = pages.at as usize;
        forget_caches();
        reset_counters();
        for _ in 0..1000 {
            assert_eq!(classify(at), Some((ConsoleOnly::Rdpru, 3)));
        }
        assert_eq!(counters().queries, 1, "one query for a thousand faults");
        assert_eq!(classify(at + 16), classify(at + 16));
        assert_eq!(counters().queries, 2, "a new address asks once");
        eprintln!(
            "readable length: one VirtualQuery per address for 1000 faults, {} for two addresses",
            counters().queries
        );
    }

    #[test]
    fn repeated_classify_uses_decode_cache() {
        if !remembers() {
            return;
        }
        let pages = Pages::new(4096);
        unsafe { std::ptr::copy_nonoverlapping([0x0f, 0x01, 0xfd].as_ptr(), pages.at, 3) };
        let at = pages.at as usize;
        forget_caches();
        reset_counters();
        assert_eq!(classify(at), Some((ConsoleOnly::Rdpru, 3)));
        let after_first = counters().decoded;
        for _ in 0..1000 {
            assert_eq!(classify(at), Some((ConsoleOnly::Rdpru, 3)));
        }
        assert_eq!(
            counters().decoded,
            after_first,
            "same RIP with same bytes should not be decoded again"
        );
    }

    #[test]
    fn decode_cache_invalidates_on_byte_change() {
        if !remembers() {
            return;
        }
        let pages = Pages::new(4096);
        unsafe { std::ptr::copy_nonoverlapping([0x0f, 0x01, 0xfd].as_ptr(), pages.at, 3) };
        let at = pages.at as usize;
        forget_caches();
        assert_eq!(classify(at), Some((ConsoleOnly::Rdpru, 3)));
        unsafe { std::ptr::copy_nonoverlapping([0x0f, 0x01, 0xfa].as_ptr(), pages.at, 3) };
        assert_eq!(classify(at), Some((ConsoleOnly::Monitorx, 3)));
    }

    /// The old sighting check locked a mutex on every emulated instruction; a thread-local
    /// bit makes repeats free of shared state.
    #[test]
    fn first_sighting_is_cheaper_than_a_mutex() {
        let old = std::sync::Mutex::new(Vec::<u8>::new());
        let iterations = 300_000u64;
        let start = Instant::now();
        for i in 0..iterations {
            let mut seen = old.lock().unwrap_or_else(|e| e.into_inner());
            let kind = (i % 10) as u8;
            if !seen.contains(&kind) {
                seen.push(kind);
            }
        }
        let before = start.elapsed();
        let start = Instant::now();
        for i in 0..iterations {
            let _ = black_box(first_sighting_kind((i % 10) as u32));
        }
        let after = start.elapsed();
        if after < before {
            let saved = 100 - (after.as_nanos() * 100 / before.as_nanos()) as usize;
            eprintln!(
                "first-sighting check: {:?} with a mutex, {:?} with the thread-local bit ({saved}%)",
                before, after
            );
        }
    }

    /// Both faults of a `clzero` loop are emulated through the decode cache and zero the whole
    /// range, with `Rax` left at its end as the loop leaves it.
    #[test]
    fn cached_clzero_loop_zeroes_the_range() {
        if !remembers() {
            return;
        }
        let pages = Pages::new(8192);
        let base = pages.at as usize;
        // The instruction in a page of its own, as the title's code lies in image pages: the
        // address stays valid across the calls that follow.
        let code = Pages::new(4096);
        // SAFETY: the fresh, writable code page.
        unsafe {
            std::ptr::copy_nonoverlapping(
                [
                    0x0f, 0x01, 0xfc, 0x48, 0x83, 0xc0, 0x40, 0x48, 0x39, 0xd0, 0x72, 0xf4,
                ]
                .as_ptr(),
                code.at,
                12,
            )
        };
        let start = base + 5;
        let end = base + 8192 - 3;
        for _ in 0..2 {
            // SAFETY: the writable allocation, filled for the check.
            unsafe { std::ptr::write_bytes(pages.at, 0xff, pages.len) };
            let mut context: CONTEXT = unsafe { std::mem::zeroed() };
            context.Rip = code.at as u64;
            context.Rax = start as u64;
            context.Rdx = end as u64;
            // SAFETY: a zeroed context with the registers the instruction reads.
            let result = unsafe { emulate(&mut context) };
            assert_eq!(result, CONTINUE_EXECUTION);
            assert_eq!(context.Rax as usize, end);
            // SAFETY: the region zeroed above.
            let zeroed = unsafe { std::slice::from_raw_parts(pages.at as *const u8, pages.len) };
            assert!(zeroed[..start - base].iter().all(|&b| b == 0xff));
            assert!(zeroed[start - base..end - base].iter().all(|&b| b == 0));
            assert!(zeroed[end - base..].iter().all(|&b| b == 0xff));
        }
    }
}
