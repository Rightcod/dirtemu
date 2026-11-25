use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShaderProfileRequest {
    Auto,
    Sm65,
    Sm66,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ShaderProfile {
    Sm65 = 0x65,
    Sm66 = 0x66,
}

impl ShaderProfile {
    pub fn cache_key(self) -> &'static str {
        match self {
            Self::Sm65 => "sm65",
            Self::Sm66 => "sm66",
        }
    }

    pub fn cache_encoding(self) -> u8 {
        self as u8
    }

    pub fn shader_model(self) -> (u8, u8) {
        match self {
            Self::Sm65 => (6, 5),
            Self::Sm66 => (6, 6),
        }
    }

    pub(crate) fn dxil_version(self) -> u32 {
        match self {
            Self::Sm65 => 0x105,
            Self::Sm66 => 0x106,
        }
    }

    pub(crate) fn validator_version(self) -> u32 {
        match self {
            Self::Sm65 => 5,
            Self::Sm66 => 6,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ResolvedShaderProfile {
    pub request: ShaderProfileRequest,
    pub profile: ShaderProfile,
    pub highest_shader_model: Option<u32>,
}

impl ResolvedShaderProfile {
    pub fn resolve(
        request: ShaderProfileRequest,
        highest_shader_model: Option<u32>,
    ) -> Result<Self, String> {
        let profile = match (request, highest_shader_model) {
            (ShaderProfileRequest::Auto, None) | (ShaderProfileRequest::Sm66, None) => {
                ShaderProfile::Sm66
            }
            (ShaderProfileRequest::Sm65, None) => ShaderProfile::Sm65,
            (ShaderProfileRequest::Auto, Some(highest)) if highest >= 0x66 => ShaderProfile::Sm66,
            (_, Some(highest)) if highest < 0x65 => {
                return Err(format!(
                    "shader profile {} requires SM 6.5 or later; the device reports HighestShaderModel={highest:#x}",
                    request.cache_key()
                ));
            }
            (ShaderProfileRequest::Sm66, Some(highest)) if highest < 0x66 => {
                return Err(format!(
                    "forced shader profile sm66 is unavailable: the device reports HighestShaderModel={highest:#x}"
                ));
            }
            (ShaderProfileRequest::Sm65, Some(_)) => ShaderProfile::Sm65,
            (ShaderProfileRequest::Auto, Some(_)) => ShaderProfile::Sm65,
            (ShaderProfileRequest::Sm66, Some(_)) => ShaderProfile::Sm66,
        };
        Ok(Self {
            request,
            profile,
            highest_shader_model,
        })
    }

    pub fn cache_key(self) -> &'static str {
        self.profile.cache_key()
    }
}

impl ShaderProfileRequest {
    pub fn cache_key(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Sm65 => "sm65",
            Self::Sm66 => "sm66",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "sm65" => Ok(Self::Sm65),
            "sm66" => Ok(Self::Sm66),
            other => Err(format!(
                "VICTORY_SHADER_PROFILE={other:?} is invalid; expected auto, sm65 or sm66"
            )),
        }
    }

    pub fn from_env() -> Result<Self, String> {
        match std::env::var("VICTORY_SHADER_PROFILE") {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Auto),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err("VICTORY_SHADER_PROFILE is not valid Unicode".into())
            }
        }
    }
}

static HOST_SHADER_MODEL: AtomicU32 = AtomicU32::new(0);
static REQUEST_OVERRIDE: AtomicU8 = AtomicU8::new(u8::MAX);

fn request_code(request: ShaderProfileRequest) -> u8 {
    match request {
        ShaderProfileRequest::Auto => 0,
        ShaderProfileRequest::Sm65 => 1,
        ShaderProfileRequest::Sm66 => 2,
    }
}

fn override_request() -> Option<ShaderProfileRequest> {
    match REQUEST_OVERRIDE.load(Ordering::Relaxed) {
        0 => Some(ShaderProfileRequest::Auto),
        1 => Some(ShaderProfileRequest::Sm65),
        2 => Some(ShaderProfileRequest::Sm66),
        _ => None,
    }
}

pub(crate) fn current_request() -> Result<ShaderProfileRequest, String> {
    match override_request() {
        Some(request) => Ok(request),
        None => ShaderProfileRequest::from_env(),
    }
}

pub fn configure_shader_profile(
    request: ShaderProfileRequest,
    highest_shader_model: Option<u32>,
) -> Result<ResolvedShaderProfile, String> {
    let resolved = ResolvedShaderProfile::resolve(request, highest_shader_model)?;
    REQUEST_OVERRIDE.store(request_code(request), Ordering::Relaxed);
    HOST_SHADER_MODEL.store(highest_shader_model.unwrap_or(0), Ordering::Relaxed);
    Ok(resolved)
}

pub fn set_host_shader_model(highest_shader_model: u32) -> Result<ResolvedShaderProfile, String> {
    let request = current_request()?;
    let resolved = ResolvedShaderProfile::resolve(request, Some(highest_shader_model))?;
    HOST_SHADER_MODEL.store(highest_shader_model, Ordering::Relaxed);
    Ok(resolved)
}

pub fn resolved_shader_profile() -> Result<ResolvedShaderProfile, String> {
    let request = current_request()?;
    let highest = match HOST_SHADER_MODEL.load(Ordering::Relaxed) {
        0 => None,
        value => Some(value),
    };
    ResolvedShaderProfile::resolve(request, highest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_resolution_keeps_offline_auto_and_checks_the_host() {
        for (request, highest, expected) in [
            (ShaderProfileRequest::Auto, None, ShaderProfile::Sm66),
            (ShaderProfileRequest::Sm65, None, ShaderProfile::Sm65),
            (ShaderProfileRequest::Sm66, None, ShaderProfile::Sm66),
            (ShaderProfileRequest::Auto, Some(0x65), ShaderProfile::Sm65),
            (ShaderProfileRequest::Auto, Some(0x66), ShaderProfile::Sm66),
            (ShaderProfileRequest::Auto, Some(0x67), ShaderProfile::Sm66),
            (ShaderProfileRequest::Sm65, Some(0x65), ShaderProfile::Sm65),
            (ShaderProfileRequest::Sm66, Some(0x66), ShaderProfile::Sm66),
        ] {
            let resolved = ResolvedShaderProfile::resolve(request, highest).unwrap();
            assert_eq!(resolved.profile, expected);
        }
        let error =
            ResolvedShaderProfile::resolve(ShaderProfileRequest::Sm66, Some(0x65)).unwrap_err();
        assert!(error.contains("forced shader profile sm66"), "{error}");
        assert!(error.contains("0x65"), "{error}");
        assert!(ResolvedShaderProfile::resolve(ShaderProfileRequest::Auto, Some(0x62)).is_err());
    }

    #[test]
    fn profile_environment_values_and_cache_keys_are_explicit() {
        for (text, request) in [
            ("auto", ShaderProfileRequest::Auto),
            (" SM65 ", ShaderProfileRequest::Sm65),
            ("Sm66", ShaderProfileRequest::Sm66),
        ] {
            assert_eq!(ShaderProfileRequest::parse(text).unwrap(), request);
        }
        assert!(ShaderProfileRequest::parse("6.5").is_err());
        assert_ne!(
            ShaderProfile::Sm65.cache_key(),
            ShaderProfile::Sm66.cache_key()
        );
        assert_ne!(
            ShaderProfile::Sm65.cache_encoding(),
            ShaderProfile::Sm66.cache_encoding()
        );
    }
}
