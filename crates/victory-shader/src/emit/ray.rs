use std::sync::OnceLock;

use crate::native::{HwStage, NativeProgram, StageIo};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RayProfile {
    Full,
    StaticMiss,
    Limited(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BvhRequest {
    Auto,
    Profile(RayProfile),
}

static AUTO_BVH_PROFILE: OnceLock<RayProfile> = OnceLock::new();

impl RayProfile {
    pub fn cache_key(self) -> String {
        match self {
            RayProfile::Full => "full".to_owned(),
            RayProfile::StaticMiss => "static-miss".to_owned(),
            RayProfile::Limited(budget) => format!("limited:{budget}"),
        }
    }

    pub fn cache_encoding(self) -> u64 {
        match self {
            RayProfile::Full => 0,
            RayProfile::StaticMiss => 1,
            RayProfile::Limited(budget) => 2 | u64::from(budget) << 2,
        }
    }

    pub fn budget(self) -> Option<u32> {
        match self {
            RayProfile::Limited(budget) => Some(budget),
            RayProfile::Full | RayProfile::StaticMiss => None,
        }
    }
}

fn normalized_profile(profile: RayProfile) -> RayProfile {
    match profile {
        RayProfile::Limited(0) => RayProfile::Full,
        profile => profile,
    }
}

fn limited_profile(value: &str) -> Option<RayProfile> {
    value
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|&budget| budget > 0)
        .map(RayProfile::Limited)
}

fn parse_bvh_profile(value: Option<&str>, budget: Option<&str>) -> BvhRequest {
    match value.map(str::trim) {
        Some("auto") => BvhRequest::Auto,
        Some("low" | "static-miss") => BvhRequest::Profile(RayProfile::StaticMiss),
        Some("limited") => {
            BvhRequest::Profile(budget.and_then(limited_profile).unwrap_or(RayProfile::Full))
        }
        Some("full" | "off") | None => BvhRequest::Profile(RayProfile::Full),
        Some(value) => {
            let budget = value
                .strip_prefix("limited:")
                .or_else(|| value.strip_prefix("limited="));
            BvhRequest::Profile(budget.and_then(limited_profile).unwrap_or(RayProfile::Full))
        }
    }
}

fn resolve_profile(requested: BvhRequest, automatic: RayProfile) -> RayProfile {
    match requested {
        BvhRequest::Auto => normalized_profile(automatic),
        BvhRequest::Profile(profile) => normalized_profile(profile),
    }
}

fn select_profile(requested: BvhRequest, automatic: RayProfile, eligible: bool) -> RayProfile {
    match resolve_profile(requested, automatic) {
        RayProfile::StaticMiss if eligible => RayProfile::StaticMiss,
        RayProfile::Limited(budget) if eligible && budget > 0 => RayProfile::Limited(budget),
        _ => RayProfile::Full,
    }
}

fn requested_bvh_profile() -> BvhRequest {
    static PROFILE: OnceLock<BvhRequest> = OnceLock::new();
    *PROFILE.get_or_init(|| {
        parse_bvh_profile(
            std::env::var("VICTORY_BVH_PROFILE").ok().as_deref(),
            std::env::var("VICTORY_BVH_BUDGET").ok().as_deref(),
        )
    })
}

fn automatic_bvh_profile() -> RayProfile {
    AUTO_BVH_PROFILE.get().copied().unwrap_or(RayProfile::Full)
}

pub fn bvh_profile() -> RayProfile {
    resolve_profile(requested_bvh_profile(), automatic_bvh_profile())
}

pub fn set_auto_bvh_profile(profile: RayProfile) -> Result<(), RayProfile> {
    AUTO_BVH_PROFILE.set(normalized_profile(profile))
}

fn ray_profile_eligible(native: &NativeProgram) -> bool {
    native.stage == HwStage::Cs
        && matches!(&native.io, StageIo::Cs(_))
        && !crate::spill::is_spilled(native)
}

fn effective_profile(
    requested: BvhRequest,
    automatic: RayProfile,
    native: &NativeProgram,
) -> RayProfile {
    select_profile(requested, automatic, ray_profile_eligible(native))
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_BVH_PROFILE: std::cell::Cell<Option<RayProfile>> =
        const { std::cell::Cell::new(None) };
}

pub(super) fn effective_bvh_profile(native: &NativeProgram) -> RayProfile {
    #[cfg(test)]
    if let Some(profile) = TEST_BVH_PROFILE.get() {
        return select_profile(
            BvhRequest::Profile(profile),
            RayProfile::Full,
            ray_profile_eligible(native),
        );
    }
    effective_profile(requested_bvh_profile(), automatic_bvh_profile(), native)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::native::{ComputeIo, RootParam, RootType};

    fn compute_program(spilled: bool) -> NativeProgram {
        NativeProgram {
            stage: HwStage::Cs,
            wave64: false,
            vgprs: 0,
            layout: vec![RootParam {
                root: RootType::Constants,
                table_offset_dwords: 0,
                sgprs: 4,
                base_register: 0,
                user_data: if spilled { 16 } else { 0 },
                slot: 0,
            }],
            used: Vec::new(),
            io: StageIo::Cs(ComputeIo {
                rsrc2: 2 << 1,
                threads: [1, 1, 1],
            }),
            code: Vec::new(),
        }
    }

    fn explicit(profile: RayProfile) -> BvhRequest {
        BvhRequest::Profile(profile)
    }

    #[test]
    fn parser_defaults_to_full_and_only_exact_auto_opts_in() {
        for value in [None, Some(""), Some("full"), Some(" off ")] {
            assert_eq!(
                parse_bvh_profile(value, Some("4")),
                explicit(RayProfile::Full),
                "{value:?}"
            );
        }
        for value in ["LOW", "1", "unknown", "ful", "offf", "static-mis"] {
            assert_eq!(
                parse_bvh_profile(Some(value), Some("4")),
                explicit(RayProfile::Full),
                "{value}"
            );
        }
        assert_eq!(parse_bvh_profile(Some("auto"), None), BvhRequest::Auto);
    }

    #[test]
    fn parser_maps_low_to_static_miss_and_reads_limited_budgets() {
        for value in ["low", "static-miss"] {
            assert_eq!(
                parse_bvh_profile(Some(value), None),
                explicit(RayProfile::StaticMiss)
            );
        }
        assert_eq!(
            parse_bvh_profile(Some("limited"), Some(" 17 ")),
            explicit(RayProfile::Limited(17))
        );
        assert_eq!(
            parse_bvh_profile(Some("limited:23"), None),
            explicit(RayProfile::Limited(23))
        );
        assert_eq!(
            parse_bvh_profile(Some("limited=29"), None),
            explicit(RayProfile::Limited(29))
        );
        assert_eq!(
            parse_bvh_profile(Some("limited:1"), None),
            explicit(RayProfile::Limited(1))
        );
    }

    #[test]
    fn invalid_limited_budgets_fall_back_to_full() {
        for (profile, budget) in [
            ("limited", None),
            ("limited", Some("")),
            ("limited", Some("0")),
            ("limited", Some("-1")),
            ("limited", Some("nope")),
            ("limited:0", Some("8")),
            ("limited:", Some("8")),
            ("limited=4294967296", Some("8")),
        ] {
            assert_eq!(
                parse_bvh_profile(Some(profile), budget),
                explicit(RayProfile::Full),
                "{profile} {budget:?}"
            );
        }
    }

    #[test]
    fn explicit_full_overrides_automatic_and_typos_cannot_select_automatic_misses() {
        assert_eq!(
            resolve_profile(BvhRequest::Auto, RayProfile::StaticMiss),
            RayProfile::StaticMiss
        );
        assert_eq!(
            resolve_profile(BvhRequest::Auto, RayProfile::Limited(7)),
            RayProfile::Limited(7)
        );
        for automatic in [RayProfile::StaticMiss, RayProfile::Limited(7)] {
            assert_eq!(
                resolve_profile(explicit(RayProfile::Full), automatic),
                RayProfile::Full
            );
            let parsed = parse_bvh_profile(Some("flul"), None);
            assert_eq!(select_profile(parsed, automatic, true), RayProfile::Full);
        }
        assert_eq!(
            resolve_profile(BvhRequest::Auto, RayProfile::Limited(0)),
            RayProfile::Full
        );
    }

    #[test]
    fn eligibility_gates_every_reduced_profile() {
        for (requested, resolved) in [
            (explicit(RayProfile::StaticMiss), RayProfile::StaticMiss),
            (explicit(RayProfile::Limited(5)), RayProfile::Limited(5)),
            (BvhRequest::Auto, RayProfile::Limited(5)),
        ] {
            let automatic = if requested == BvhRequest::Auto {
                RayProfile::Limited(5)
            } else {
                RayProfile::Full
            };
            assert_eq!(select_profile(requested, automatic, true), resolved);
            assert_eq!(
                select_profile(requested, automatic, false),
                RayProfile::Full
            );
        }
        assert_eq!(
            select_profile(
                explicit(RayProfile::Limited(0)),
                RayProfile::StaticMiss,
                true
            ),
            RayProfile::Full
        );
    }

    #[test]
    fn non_compute_mismatched_and_spilled_builder_programs_stay_full() {
        let compute = compute_program(false);
        assert!(ray_profile_eligible(&compute));
        for requested in [
            explicit(RayProfile::StaticMiss),
            explicit(RayProfile::Limited(3)),
        ] {
            assert_eq!(
                effective_profile(requested, RayProfile::Full, &compute_program(true)),
                RayProfile::Full
            );
            let mut other_stage = compute.clone();
            other_stage.stage = HwStage::Ps;
            assert_eq!(
                effective_profile(requested, RayProfile::Full, &other_stage),
                RayProfile::Full
            );
            let mut mismatched = compute.clone();
            mismatched.io = StageIo::None;
            assert_eq!(
                effective_profile(requested, RayProfile::Full, &mismatched),
                RayProfile::Full
            );
        }
    }

    #[test]
    fn automatic_profile_is_immutable_after_initialization() {
        let initial = automatic_bvh_profile();
        let _ = set_auto_bvh_profile(initial);
        let replacement = if initial == RayProfile::StaticMiss {
            RayProfile::Limited(9)
        } else {
            RayProfile::StaticMiss
        };
        assert_eq!(set_auto_bvh_profile(replacement), Err(replacement));
        assert_eq!(automatic_bvh_profile(), initial);
    }

    #[test]
    fn cache_encoding_is_unambiguous_and_carries_the_complete_budget() {
        let profiles = [
            RayProfile::Full,
            RayProfile::StaticMiss,
            RayProfile::Limited(1),
            RayProfile::Limited(2),
            RayProfile::Limited(17),
            RayProfile::Limited(u32::MAX),
        ];
        let keys: HashSet<String> = profiles.iter().map(|p| p.cache_key()).collect();
        let encoded: HashSet<u64> = profiles.iter().map(|p| p.cache_encoding()).collect();
        assert_eq!(keys.len(), profiles.len());
        assert_eq!(encoded.len(), profiles.len());
        assert_eq!(RayProfile::Full.cache_key(), "full");
        assert_eq!(RayProfile::StaticMiss.cache_key(), "static-miss");
        assert_eq!(RayProfile::Limited(17).cache_key(), "limited:17");
        assert_eq!(
            RayProfile::Limited(0x1234_5678).cache_encoding(),
            2 | 0x1234_5678_u64 << 2
        );
        for profile in profiles {
            assert_eq!(
                parse_bvh_profile(Some(&profile.cache_key()), None),
                explicit(profile)
            );
            assert_eq!(
                profile.budget(),
                match profile {
                    RayProfile::Limited(budget) => Some(budget),
                    _ => None,
                }
            );
        }
    }
}
