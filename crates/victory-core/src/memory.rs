//! How much memory the title and the host-side caches may use.
//!
//! The console reports a title budget (`victory-gpu`'s `console`), and the
//! layer keeps host copies, staging buffers, shadow snapshots and buffers in
//! the card's memory beside it. On a PC with 8 GiB of video memory and 16 GiB
//! of RAM those caches are what ends a run, not the title's own figure: the
//! title was told 13.5 GiB and asked for 6.1 GiB at once, while the host's
//! copies grew without a bound (27 GB of commit in one run).
//!
//! A profile is a set of caps for those caches and a title figure a low-memory
//! machine can meet, shaped after the Series S (7,776 MiB of title memory).
//! It is chosen by `VICTORY_MEMORY_PROFILE` (`auto`, `series-s`, `series-x`,
//! `off`) or by `[memory].profile` in the config file. `auto` picks the
//! Series S profile when the card has 8 GiB of its own memory or less, when
//! the PC has 20 GiB of RAM or less, or when Windows' commit limit is small.
//!
//! Caps are enforced where the allocation is made (`victory-gpu`'s
//! `resource`, `texture`, `shadow` and `host`, and `xmem`'s physical pages):
//! a request past a cap is refused with a line naming the cap, not left to
//! Windows to fail. The caps apply to sampled, streamed content (host copies
//! of textures the title fills, texture staging) and to host-side caches;
//! render targets, depth targets and unordered-access targets are never
//! shrunk. A sampled texture is admitted whole or not at all: a partial mip
//! chain would leave shader resource views naming levels the host texture
//! does not have, so no view-correct mip cut is possible without changing
//! descriptor creation (documented in `docs/architecture.md`).

use std::sync::Mutex;

pub const MIB: u64 = 1 << 20;
pub const GIB: u64 = 1 << 30;

/// Series S title memory (the published console-mode figure).
pub const SERIES_S_TITLE: u64 = 7_776 * MIB;
/// Series X title memory: GPU-optimal and standard.
pub const SERIES_X_GPU: u64 = 10 * GIB;
pub const SERIES_X_STANDARD: u64 = 7 * GIB / 2;
/// Title memory kept for anything but the physical-page pool.
pub const PAGE_POOL_RESERVE: u64 = 512 * MIB;

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProfileRequest {
    /// Follow the host: the Series S profile on a small machine.
    #[default]
    Auto,
    /// No caps: the console's own figure and the layer's former defaults.
    Off,
    /// The low-memory profile: about 7.6 GiB of title memory and small caches.
    SeriesS,
    /// A generous set of caps for a PC with room to spare.
    SeriesX,
}

impl ProfileRequest {
    /// `auto`, `off`, `series-s` or `series-x` (case and surrounding space
    /// ignored). Anything else is refused, not guessed.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "auto" => ProfileRequest::Auto,
            "off" => ProfileRequest::Off,
            "series-s" => ProfileRequest::SeriesS,
            "series-x" => ProfileRequest::SeriesX,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            ProfileRequest::Auto => "auto",
            ProfileRequest::Off => "off",
            ProfileRequest::SeriesS => "series-s",
            ProfileRequest::SeriesX => "series-x",
        }
    }
}

/// What `auto` (or an explicit request) settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryProfile {
    Off,
    SeriesS,
    SeriesX,
}

impl MemoryProfile {
    pub const fn as_str(self) -> &'static str {
        match self {
            MemoryProfile::Off => "off",
            MemoryProfile::SeriesS => "series-s",
            MemoryProfile::SeriesX => "series-x",
        }
    }
}

/// Why `auto` picked what it picked, for the one line the layer logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    Explicit,
    DedicatedVideoMemory,
    PhysicalMemory,
    CommitLimit,
    CommitHeadroom,
    NoConstraints,
}

impl Why {
    pub const fn as_str(self) -> &'static str {
        match self {
            Why::Explicit => "explicit",
            Why::DedicatedVideoMemory => "video memory <= 8 GiB",
            Why::PhysicalMemory => "physical memory <= 20 GiB",
            Why::CommitLimit => "commit limit <= 24 GiB",
            Why::CommitHeadroom => "commit headroom < 8 GiB",
            Why::NoConstraints => "no constraint met",
        }
    }
}

/// The host facts `auto` reads. Zero means unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeviceFacts {
    pub dedicated_video_memory: u64,
    pub physical_memory: u64,
    pub commit_limit: u64,
    pub commit_available: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolution {
    pub profile: MemoryProfile,
    pub why: Why,
}

pub const AUTO_VIDEO_MEMORY: u64 = 8 * GIB;
pub const AUTO_PHYSICAL_MEMORY: u64 = 20 * GIB;
pub const AUTO_COMMIT_LIMIT: u64 = 24 * GIB;
pub const AUTO_COMMIT_HEADROOM: u64 = 8 * GIB;

/// Resolves a request against what the host says about itself.
pub fn resolve(request: ProfileRequest, facts: DeviceFacts) -> Resolution {
    let profile = match request {
        ProfileRequest::Off => MemoryProfile::Off,
        ProfileRequest::SeriesS => MemoryProfile::SeriesS,
        ProfileRequest::SeriesX => MemoryProfile::SeriesX,
        ProfileRequest::Auto => return auto(facts),
    };
    Resolution {
        profile,
        why: Why::Explicit,
    }
}

fn auto(facts: DeviceFacts) -> Resolution {
    if facts.dedicated_video_memory > 0 && facts.dedicated_video_memory <= AUTO_VIDEO_MEMORY {
        return Resolution {
            profile: MemoryProfile::SeriesS,
            why: Why::DedicatedVideoMemory,
        };
    }
    if facts.physical_memory > 0 && facts.physical_memory <= AUTO_PHYSICAL_MEMORY {
        return Resolution {
            profile: MemoryProfile::SeriesS,
            why: Why::PhysicalMemory,
        };
    }
    if facts.commit_limit > 0 && facts.commit_limit <= AUTO_COMMIT_LIMIT {
        return Resolution {
            profile: MemoryProfile::SeriesS,
            why: Why::CommitLimit,
        };
    }
    if facts.commit_available > 0 && facts.commit_available < AUTO_COMMIT_HEADROOM {
        return Resolution {
            profile: MemoryProfile::SeriesS,
            why: Why::CommitHeadroom,
        };
    }
    Resolution {
        profile: MemoryProfile::SeriesX,
        why: Why::NoConstraints,
    }
}

/// A set of caps in bytes; 0 means "no cap" (the former behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub profile: MemoryProfile,
    /// Title memory the console reports (0: the console's own split).
    pub title_bytes: u64,
    /// The split when `title_bytes` applies.
    pub gpu_optimal_bytes: u64,
    pub standard_bytes: u64,
    /// Host copies of sampled, streamed textures (the card's memory).
    pub texture_bytes: u64,
    pub texture_single_bytes: u64,
    /// Texture staging buffers (the PC's memory).
    pub staging_bytes: u64,
    pub staging_single_bytes: u64,
    /// Upload scratch chunks kept for lists (the PC's memory).
    pub tile_pool_bytes: u64,
    /// Host copies of title buffers in the card's memory.
    pub card_buffers_bytes: u64,
    /// The smallest title buffer whose copy goes to the card.
    pub card_min_bytes: u64,
    /// All host copies of title memory (buffers and windows, the PC's memory).
    pub host_copy_bytes: u64,
    /// Snapshot buffers kept for reuse, their bytes, and buffers kept.
    pub snapshot_pool_bytes: u64,
    pub snapshot_kept_bytes: u64,
    pub snapshot_buffers: usize,
    /// Advice only: host descriptor heaps and the shader cache (their
    /// owners are outside this profile's files).
    pub descriptor_bytes: u64,
    pub shader_cache_bytes: u64,
    /// The render target / depth view rings made when the device is made.
    pub rtv_ring: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self::for_profile(MemoryProfile::Off, DeviceFacts::default())
    }
}

impl Limits {
    pub fn for_profile(profile: MemoryProfile, facts: DeviceFacts) -> Limits {
        let (title, gpu, standard) = match profile {
            MemoryProfile::Off => (0, 0, 0),
            MemoryProfile::SeriesX => (
                SERIES_X_GPU + SERIES_X_STANDARD,
                SERIES_X_GPU,
                SERIES_X_STANDARD,
            ),
            MemoryProfile::SeriesS => (SERIES_S_TITLE, 0, SERIES_S_TITLE),
        };
        match profile {
            MemoryProfile::Off => Limits {
                profile,
                title_bytes: 0,
                gpu_optimal_bytes: 0,
                standard_bytes: 0,
                texture_bytes: 0,
                texture_single_bytes: 0,
                staging_bytes: 0,
                staging_single_bytes: 0,
                tile_pool_bytes: 0,
                card_buffers_bytes: 0,
                card_min_bytes: 0,
                host_copy_bytes: 0,
                snapshot_pool_bytes: 0,
                snapshot_kept_bytes: 0,
                snapshot_buffers: 0,
                descriptor_bytes: 0,
                shader_cache_bytes: 0,
                rtv_ring: 1024,
            },
            MemoryProfile::SeriesX => Limits {
                profile,
                title_bytes: title,
                gpu_optimal_bytes: gpu,
                standard_bytes: standard,
                texture_bytes: 4 * GIB,
                texture_single_bytes: 2 * GIB,
                staging_bytes: GIB,
                staging_single_bytes: 512 * MIB,
                tile_pool_bytes: 512 * MIB,
                card_buffers_bytes: 512 * MIB,
                card_min_bytes: 16 * MIB,
                host_copy_bytes: 6 * GIB,
                snapshot_pool_bytes: 512 * MIB,
                snapshot_kept_bytes: 256 * MIB,
                snapshot_buffers: 8,
                descriptor_bytes: 512 * MIB,
                shader_cache_bytes: GIB,
                rtv_ring: 1024,
            },
            MemoryProfile::SeriesS => {
                let texture = series_s_texture(facts.dedicated_video_memory);
                Limits {
                    profile,
                    title_bytes: title,
                    gpu_optimal_bytes: gpu,
                    standard_bytes: standard,
                    texture_bytes: texture,
                    texture_single_bytes: (texture / 2).max(256 * MIB),
                    staging_bytes: 256 * MIB,
                    staging_single_bytes: 128 * MIB,
                    tile_pool_bytes: 256 * MIB,
                    card_buffers_bytes: 256 * MIB,
                    card_min_bytes: 64 * MIB,
                    host_copy_bytes: 5 * GIB / 2,
                    snapshot_pool_bytes: 64 * MIB,
                    snapshot_kept_bytes: 64 * MIB,
                    snapshot_buffers: 4,
                    descriptor_bytes: 64 * MIB,
                    shader_cache_bytes: 128 * MIB,
                    // The 1,024-slot rings are already small and proven with
                    // this title; a smaller ring could overwrite a descriptor
                    // a recorded list still names. Left as they are.
                    rtv_ring: 1024,
                }
            }
        }
    }

    /// The page pool the title may ask of `xmem`: title memory less the
    /// reserve for its other allocations.
    pub const fn page_pool_bytes(&self) -> u64 {
        if self.title_bytes == 0 {
            0
        } else {
            self.title_bytes.saturating_sub(PAGE_POOL_RESERVE)
        }
    }

    /// The title memory `console`'s split becomes under this profile; the
    /// profile only lowers a figure, never raises one.
    pub const fn effective_title(&self, console_gpu: u64, console_standard: u64) -> (u64, u64) {
        let total = console_gpu + console_standard;
        if self.title_bytes == 0 || total <= self.title_bytes {
            (console_gpu, console_standard)
        } else {
            (self.gpu_optimal_bytes, self.standard_bytes)
        }
    }

    pub const fn admits_texture(&self, used: u64, bytes: u64) -> bool {
        self.texture_bytes == 0
            || (bytes <= self.texture_single_bytes
                && used.saturating_add(bytes) <= self.texture_bytes)
    }

    pub const fn admits_staging(&self, used: u64, bytes: u64) -> bool {
        self.staging_bytes == 0
            || (bytes <= self.staging_single_bytes
                && used.saturating_add(bytes) <= self.staging_bytes)
    }

    pub const fn admits_tile(&self, used: u64, bytes: u64) -> bool {
        self.tile_pool_bytes == 0 || used.saturating_add(bytes) <= self.tile_pool_bytes
    }

    pub const fn admits_host_copy(&self, used: u64, bytes: u64) -> bool {
        self.host_copy_bytes == 0 || used.saturating_add(bytes) <= self.host_copy_bytes
    }

    pub const fn admits_page_pool(&self, reserved: u64, bytes: u64) -> bool {
        self.page_pool_bytes() == 0 || reserved.saturating_add(bytes) <= self.page_pool_bytes()
    }
}

/// The sampled-texture budget by video memory: a sixteenth of it, at least
/// 512 MiB (an 8 GiB card), at most 2 GiB, on a 64 MiB step.
pub fn series_s_texture(video_memory: u64) -> u64 {
    let scaled = if video_memory == 0 {
        512 * MIB
    } else {
        (video_memory / 16).max(512 * MIB)
    };
    let capped = scaled.min(2 * GIB);
    capped / (64 * MIB) * (64 * MIB)
}

/// The request the process uses: what `Config::load` set (the config file),
/// else `VICTORY_MEMORY_PROFILE`, else `auto`.
pub fn request() -> ProfileRequest {
    if let Some(request) = *SET.lock().unwrap_or_else(|e| e.into_inner()) {
        return request;
    }
    request_from_env()
}

/// Reads `VICTORY_MEMORY_PROFILE`; an empty value is `auto` and an unknown
/// one is refused with a line (and `auto`).
pub fn request_from_env() -> ProfileRequest {
    let Ok(value) = std::env::var("VICTORY_MEMORY_PROFILE") else {
        return ProfileRequest::Auto;
    };
    if value.trim().is_empty() {
        return ProfileRequest::Auto;
    }
    match ProfileRequest::parse(&value) {
        Some(request) => request,
        None => {
            tracing::warn!(
                value,
                "VICTORY_MEMORY_PROFILE is not auto, off, series-s or series-x; using auto"
            );
            ProfileRequest::Auto
        }
    }
}

static SET: Mutex<Option<ProfileRequest>> = Mutex::new(None);

/// Sets the request the process uses (from the config file). The first call
/// wins; later ones are ignored, so a test cannot change another's.
pub fn set_request(request: ProfileRequest) {
    let mut set = SET.lock().unwrap_or_else(|e| e.into_inner());
    if set.is_none() {
        *set = Some(request);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(vram: u64, ram: u64, commit: u64, available: u64) -> DeviceFacts {
        DeviceFacts {
            dedicated_video_memory: vram,
            physical_memory: ram,
            commit_limit: commit,
            commit_available: available,
        }
    }

    #[test]
    fn requests_are_parsed_exactly() {
        for (text, expected) in [
            ("auto", ProfileRequest::Auto),
            (" Off ", ProfileRequest::Off),
            ("SERIES-S", ProfileRequest::SeriesS),
            ("series-x", ProfileRequest::SeriesX),
        ] {
            assert_eq!(ProfileRequest::parse(text), Some(expected), "{text}");
        }
        for text in ["", "series", "s", "x", "low", "0"] {
            assert_eq!(ProfileRequest::parse(text), None, "{text}");
        }
    }

    #[test]
    fn auto_picks_series_s_on_an_eight_gib_card() {
        let resolution = resolve(
            ProfileRequest::Auto,
            facts(8 * GIB - MIB, 64 * GIB, 64 * GIB, 48 * GIB),
        );
        assert_eq!(resolution.profile, MemoryProfile::SeriesS);
        assert_eq!(resolution.why, Why::DedicatedVideoMemory);
    }

    #[test]
    fn auto_picks_series_s_on_a_sixteen_gib_host() {
        let resolution = resolve(
            ProfileRequest::Auto,
            facts(24 * GIB, 16 * GIB - MIB, 32 * GIB, 24 * GIB),
        );
        assert_eq!(resolution.profile, MemoryProfile::SeriesS);
        assert_eq!(resolution.why, Why::PhysicalMemory);
    }

    #[test]
    fn auto_picks_series_s_on_a_small_commit_limit_or_headroom() {
        let limit = resolve(
            ProfileRequest::Auto,
            facts(24 * GIB, 64 * GIB, 20 * GIB, 16 * GIB),
        );
        assert_eq!(limit.profile, MemoryProfile::SeriesS);
        assert_eq!(limit.why, Why::CommitLimit);

        let headroom = resolve(
            ProfileRequest::Auto,
            facts(24 * GIB, 64 * GIB, 64 * GIB, 4 * GIB),
        );
        assert_eq!(headroom.profile, MemoryProfile::SeriesS);
        assert_eq!(headroom.why, Why::CommitHeadroom);
    }

    #[test]
    fn auto_leaves_a_big_machine_on_series_x() {
        let resolution = resolve(
            ProfileRequest::Auto,
            facts(24 * GIB, 64 * GIB, 96 * GIB, 64 * GIB),
        );
        assert_eq!(resolution.profile, MemoryProfile::SeriesX);
        assert_eq!(resolution.why, Why::NoConstraints);
        // Unknown facts cannot make the profile smaller.
        assert_eq!(
            resolve(ProfileRequest::Auto, DeviceFacts::default()).profile,
            MemoryProfile::SeriesX
        );
    }

    #[test]
    fn explicit_requests_win_over_auto() {
        let big = facts(24 * GIB, 64 * GIB, 96 * GIB, 64 * GIB);
        for (request, profile) in [
            (ProfileRequest::Off, MemoryProfile::Off),
            (ProfileRequest::SeriesS, MemoryProfile::SeriesS),
            (ProfileRequest::SeriesX, MemoryProfile::SeriesX),
        ] {
            let resolution = resolve(request, big);
            assert_eq!(resolution.profile, profile);
            assert_eq!(resolution.why, Why::Explicit);
        }
    }

    #[test]
    fn series_s_limits_are_a_low_memory_profile() {
        let limits = Limits::for_profile(
            MemoryProfile::SeriesS,
            facts(8 * GIB, 16 * GIB, 32 * GIB, 24 * GIB),
        );
        assert_eq!(limits.title_bytes, SERIES_S_TITLE);
        assert_eq!(limits.gpu_optimal_bytes, 0);
        assert_eq!(limits.standard_bytes, SERIES_S_TITLE);
        assert!((7 * GIB..=8 * GIB).contains(&limits.title_bytes));
        assert_eq!(limits.texture_bytes, 512 * MIB);
        assert_eq!(limits.texture_single_bytes, 256 * MIB);
        assert_eq!(limits.staging_bytes, 256 * MIB);
        assert_eq!(limits.tile_pool_bytes, 256 * MIB);
        assert_eq!(limits.card_buffers_bytes, 256 * MIB);
        assert_eq!(limits.snapshot_pool_bytes, 64 * MIB);
        assert_eq!(limits.rtv_ring, 1024);
        // The title's 6.1 GiB page pool still fits.
        assert!(limits.admits_page_pool(0, 6 * GIB + 100 * MIB));
    }

    #[test]
    fn series_s_caps_fit_a_sixteen_gib_host() {
        let limits = Limits::for_profile(
            MemoryProfile::SeriesS,
            facts(8 * GIB, 16 * GIB, 32 * GIB, 24 * GIB),
        );
        let total = limits.title_bytes
            + limits.texture_bytes
            + limits.staging_bytes
            + limits.tile_pool_bytes
            + limits.card_buffers_bytes
            + limits.host_copy_bytes
            + limits.snapshot_pool_bytes
            + limits.descriptor_bytes
            + limits.shader_cache_bytes;
        assert!(
            total <= 12 * GIB,
            "the profile's caps alone must leave room for Windows: {total} bytes"
        );
    }

    #[test]
    fn the_texture_budget_follows_video_memory() {
        assert_eq!(series_s_texture(0), 512 * MIB);
        assert_eq!(series_s_texture(4 * GIB), 512 * MIB);
        assert_eq!(series_s_texture(8 * GIB), 512 * MIB);
        assert_eq!(series_s_texture(16 * GIB), GIB);
        assert_eq!(series_s_texture(64 * GIB), 2 * GIB);
        assert_eq!(series_s_texture(8 * GIB) % (64 * MIB), 0);
    }

    #[test]
    fn admits_is_inclusive_at_the_cap() {
        let limits = Limits::for_profile(
            MemoryProfile::SeriesS,
            facts(8 * GIB, 16 * GIB, 32 * GIB, 24 * GIB),
        );
        assert!(limits.admits_texture(0, 256 * MIB));
        assert!(limits.admits_texture(256 * MIB, 256 * MIB));
        assert!(!limits.admits_texture(256 * MIB + 1, 256 * MIB));
        assert!(!limits.admits_texture(0, 256 * MIB + 1));
        assert!(limits.admits_staging(0, 128 * MIB));
        assert!(limits.admits_staging(128 * MIB, 128 * MIB));
        assert!(!limits.admits_staging(128 * MIB + 1, 128 * MIB));
        assert!(!limits.admits_staging(0, 128 * MIB + 1));
        assert!(limits.admits_tile(0, 256 * MIB));
        assert!(!limits.admits_tile(1, 256 * MIB));
        assert!(limits.admits_host_copy(0, 5 * GIB / 2));
        assert!(!limits.admits_host_copy(1, 5 * GIB / 2));
        assert!(limits.admits_page_pool(0, limits.page_pool_bytes()));
        assert!(!limits.admits_page_pool(1, limits.page_pool_bytes()));
    }

    #[test]
    fn off_has_no_caps_and_series_x_is_generous() {
        let off = Limits::for_profile(MemoryProfile::Off, DeviceFacts::default());
        assert_eq!(off.title_bytes, 0);
        assert!(off.admits_texture(u64::MAX / 2, u64::MAX / 2));
        assert!(off.admits_staging(u64::MAX / 2, u64::MAX / 2));
        assert!(off.admits_host_copy(u64::MAX / 2, u64::MAX / 2));
        assert!(off.admits_page_pool(u64::MAX / 2, u64::MAX / 2));

        let x = Limits::for_profile(MemoryProfile::SeriesX, DeviceFacts::default());
        assert_eq!(x.title_bytes, SERIES_X_GPU + SERIES_X_STANDARD);
        assert_eq!(
            (x.gpu_optimal_bytes, x.standard_bytes),
            (SERIES_X_GPU, SERIES_X_STANDARD)
        );
        assert_eq!(x.texture_bytes, 4 * GIB);
        assert_eq!(x.card_buffers_bytes, 512 * MIB);
    }

    #[test]
    fn the_profile_lowers_the_console_figure_never_raises_it() {
        let s = Limits::for_profile(MemoryProfile::SeriesS, DeviceFacts::default());
        assert_eq!(
            s.effective_title(SERIES_X_GPU, SERIES_X_STANDARD),
            (0, SERIES_S_TITLE)
        );
        assert_eq!(s.effective_title(0, SERIES_S_TITLE), (0, SERIES_S_TITLE));

        let x = Limits::for_profile(MemoryProfile::SeriesX, DeviceFacts::default());
        assert_eq!(
            x.effective_title(SERIES_X_GPU, SERIES_X_STANDARD),
            (SERIES_X_GPU, SERIES_X_STANDARD)
        );
        // A Series S console title stays small under the Series X profile.
        assert_eq!(x.effective_title(0, SERIES_S_TITLE), (0, SERIES_S_TITLE));

        let off = Limits::for_profile(MemoryProfile::Off, DeviceFacts::default());
        assert_eq!(
            off.effective_title(SERIES_X_GPU, SERIES_X_STANDARD),
            (SERIES_X_GPU, SERIES_X_STANDARD)
        );
    }
}
