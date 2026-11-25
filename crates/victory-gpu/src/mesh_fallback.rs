//! Selection and scheduling limits for mesh emulation without MeshShaderTier.

use std::sync::OnceLock;

pub(crate) const INDIRECT_GROUP_LIMIT: u32 = 1024;
pub(crate) const MAX_PRIMITIVES: u32 = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Override {
    Auto,
    Hardware,
    Compute,
}

fn parse_override(value: Option<&str>) -> Override {
    match value.map(str::trim) {
        Some("1" | "on" | "compute" | "fallback") => Override::Compute,
        Some("0" | "off" | "hardware") => Override::Hardware,
        _ => Override::Auto,
    }
}

pub(crate) fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        let selected = parse_override(std::env::var("VICTORY_MESH_FALLBACK").ok().as_deref());
        let supported = crate::host::host().is_some_and(|host| host.mesh_shaders());
        let on = match selected {
            Override::Auto => !supported,
            Override::Hardware => false,
            Override::Compute => true,
        };
        tracing::info!(
            hardware_mesh = supported,
            compute_fallback = on,
            "mesh translation selected (VICTORY_MESH_FALLBACK=auto|off|on)"
        );
        on
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_parsing_is_explicit_and_auto_by_default() {
        assert_eq!(parse_override(None), Override::Auto);
        assert_eq!(parse_override(Some("auto")), Override::Auto);
        assert_eq!(parse_override(Some("unexpected")), Override::Auto);
        for value in ["1", "on", "compute", "fallback", " on "] {
            assert_eq!(parse_override(Some(value)), Override::Compute);
        }
        for value in ["0", "off", "hardware"] {
            assert_eq!(parse_override(Some(value)), Override::Hardware);
        }
    }
}
