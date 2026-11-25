//! Shader-model profiles: what each one puts in the container, which
//! handle opcodes it uses, and the PSV0 layout the validator expects.

use super::bitcode::{Attr, Term};
use super::shader::{BindingKey, DescriptorClass, DescriptorTable, Shader};
use super::{ResolvedShaderProfile, ShaderProfile, ShaderProfileRequest, resolved_shader_profile};
use crate::dxbc::{Container, FourCC};

fn word(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(data[at..at + 4].try_into().unwrap())
}

fn part(dxil: &[u8], fourcc: FourCC) -> &[u8] {
    let c = Container::parse(dxil).unwrap();
    c.part(fourcc).unwrap().data
}

/// A compute shader that reads one user-data CBV, one raw UAV from the
/// dynamic heap and one sampler from the dynamic sampler heap, and stores
/// through the UAV (so the flags match what the code uses).
fn alias_shader(profile: ShaderProfile) -> (Vec<u8>, Vec<u32>, Vec<u32>) {
    let mut s = Shader::new(false, profile);
    s.threads = Some([1, 1, 1]);
    s.wave_size = Some(64);
    s.user_data = 4;
    let _ = s.user_data_handle();
    let b = s.entry;
    let i = s.m.i32(0);
    let uav = s.heap_handle(b, i, false, false, (11 | 0x1000, 0));
    let _ = s.heap_handle(b, i, true, false, (14, 0));
    let undef = s.m.undef(s.t.i32);
    let value = s.m.i32(7);
    let args = [
        uav,
        i,
        undef,
        value,
        undef,
        undef,
        undef,
        s.m.i8(1),
        s.m.i32(4),
    ];
    s.op(
        b,
        "rawBufferStore.i32",
        Attr::NoUnwind,
        s.t.void,
        140,
        &args,
    );
    s.m.terminate(b, Term::Ret);
    let opcodes = s.opcodes.clone();
    let (dxil, tags) = s.finish_with_tags();
    (dxil, opcodes, tags)
}

#[test]
fn sm65_containers_declare_dxil_1_5_and_psv_version_1() {
    let (dxil, opcodes, tags) = alias_shader(ShaderProfile::Sm65);
    let program = part(&dxil, FourCC::DXIL);
    assert_eq!(word(program, 0), 5 << 16 | 0x65, "cs_6_5");
    assert_eq!(word(program, 12), 0x105, "DXIL 1.5");
    let psv = part(&dxil, FourCC::PSV0);
    assert_eq!(word(psv, 0), 36, "PSVRuntimeInfo1");
    let count = word(psv, 4 + 36) as usize;
    let stride = word(psv, 8 + 36) as usize;
    assert_eq!(stride, 16, "PSVResourceBindInfo0");
    assert_eq!(count, 3, "user data, sampler alias, UAV alias");
    let record = |n: usize| 4 + 36 + 8 + n * stride;
    let kinds: Vec<u32> = (0..count).map(|n| word(psv, record(n))).collect();
    let spaces: Vec<u32> = (0..count).map(|n| word(psv, record(n) + 4)).collect();
    let uppers: Vec<u32> = (0..count).map(|n| word(psv, record(n) + 12)).collect();
    assert_eq!(kinds, [2, 1, 7]);
    assert_eq!(spaces[0], super::USER_DATA_SPACE);
    assert_eq!(spaces[2], super::shader::DYNAMIC_SPACE_BASE, "UAV alias");
    assert_eq!(
        spaces[1],
        super::shader::DYNAMIC_SPACE_BASE + 1,
        "sampler alias"
    );
    assert_eq!(uppers[2], u32::MAX, "an unbounded alias");
    assert_eq!(uppers[1], u32::MAX);
    let sfi = u64::from_le_bytes(part(&dxil, FourCC::SFI0).try_into().unwrap());
    assert_eq!(sfi & 0x1640_0000, 0, "no SM 6.6 feature bits");
    assert!(opcodes.contains(&57), "createHandle");
    for op in [216, 217, 218] {
        assert!(!opcodes.contains(&op), "SM 6.6 handle opcode {op}");
    }
    assert!(!tags.contains(&11), "no WaveSize property at SM 6.5");
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut dxil.clone())
            .unwrap_or_else(|e| panic!("sm65 container: {e}"));
    }
}

#[test]
fn sm66_containers_keep_their_dxil_1_6_and_psv_version_2() {
    let (dxil, opcodes, tags) = alias_shader(ShaderProfile::Sm66);
    let program = part(&dxil, FourCC::DXIL);
    assert_eq!(word(program, 0), 5 << 16 | 0x66, "cs_6_6");
    assert_eq!(word(program, 12), 0x106, "DXIL 1.6");
    let psv = part(&dxil, FourCC::PSV0);
    assert_eq!(word(psv, 0), 48, "PSVRuntimeInfo2");
    let count = word(psv, 4 + 48) as usize;
    let stride = word(psv, 8 + 48) as usize;
    assert_eq!(stride, 24, "PSVResourceBindInfo1");
    assert_eq!(count, 1, "only the user-data CBV is a declared range");
    let sfi = u64::from_le_bytes(part(&dxil, FourCC::SFI0).try_into().unwrap());
    assert_eq!(
        sfi & 0x1640_0000,
        0x200_0000,
        "the resource heap (the sampler heap's last use was optimised away)"
    );
    for op in [216, 217, 218] {
        assert!(opcodes.contains(&op), "SM 6.6 handle opcode {op}");
    }
    assert!(!opcodes.contains(&57), "createHandle is for SM 6.5");
    assert!(tags.contains(&11), "WaveSize is declared at SM 6.6");
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut dxil.clone())
            .unwrap_or_else(|e| panic!("sm66 container: {e}"));
    }
}

#[test]
fn sm65_structured_alias_declares_its_stride() {
    let mut s = Shader::new(false, ShaderProfile::Sm65);
    s.threads = Some([1, 1, 1]);
    let b = s.entry;
    let zero = s.m.i32(0);
    let uav = s.heap_handle(b, zero, false, false, (12 | 0x1000, 16));
    let undef = s.m.undef(s.t.i32);
    let value = s.m.i32(7);
    let args = [
        uav,
        zero,
        zero,
        value,
        undef,
        undef,
        undef,
        s.m.i8(1),
        s.m.i32(4),
    ];
    s.op(
        b,
        "rawBufferStore.i32",
        Attr::NoUnwind,
        s.t.void,
        140,
        &args,
    );
    s.m.terminate(b, Term::Ret);
    let mut dxil = s.finish();
    let psv = part(&dxil, FourCC::PSV0);
    let count = word(psv, 4 + 36) as usize;
    assert_eq!(count, 1);
    assert_eq!(word(psv, 4 + 36 + 8), 8, "structured UAV PSV type");
    assert_eq!(
        word(psv, 4 + 36 + 12),
        super::shader::DYNAMIC_SPACE_BASE,
        "alias space"
    );
    assert_eq!(word(psv, 4 + 36 + 20), u32::MAX, "unbounded upper bound");
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        v.validate(&mut dxil)
            .unwrap_or_else(|e| panic!("sm65 structured alias: {e}"));
    }
}

fn typed_uav_shader(profile: ShaderProfile) -> Vec<u8> {
    let mut s = Shader::new(false, profile);
    s.threads = Some([1, 1, 1]);
    s.user_data = 4;
    let _ = s.user_data_handle();
    s.samplers = 1;
    let b = s.entry;
    let i = s.m.i32(0);
    if profile == ShaderProfile::Sm65 {
        let _ = s.heap_handle(b, i, false, false, (2, 0x409));
        let _ = s.heap_handle(b, i, false, false, (12 | 0x1000, 16));
        let _ = s.heap_handle(b, i, false, false, (6 | 0x1000, 0x409));
    } else {
        s.flags |= super::shader::flags::RAW_BUFFERS;
        for (class, kind, stride, component, space, range_id, upper) in [
            (DescriptorClass::Srv, 2, 0, 0x409, 2, 0, u32::MAX),
            (DescriptorClass::Uav, 12, 16, 0, 3, 0, 0),
            (DescriptorClass::Uav, 6, 0, 0x409, 4, 1, 0),
        ] {
            s.descriptor_tables.push(DescriptorTable {
                key: BindingKey {
                    class,
                    kind,
                    stride,
                    component,
                    counter: false,
                },
                space,
                range_id,
                upper,
            });
        }
    }
    let _ = s.sampler_handle(b, 0);
    s.m.terminate(b, Term::Ret);
    s.finish()
}

#[test]
fn typed_uav_is_ordered_with_the_uavs_in_psv0_at_both_profiles() {
    for profile in [ShaderProfile::Sm65, ShaderProfile::Sm66] {
        let dxil = typed_uav_shader(profile);
        let psv = part(&dxil, FourCC::PSV0);
        let info = word(psv, 0) as usize;
        let count = word(psv, 4 + info) as usize;
        let stride = word(psv, 8 + info) as usize;
        let record = |n: usize| 12 + info + n * stride;
        let field = |n: usize, at: usize| word(psv, record(n) + at);
        let order: Vec<(u32, u32)> = (0..count).map(|n| (field(n, 0), field(n, 4))).collect();
        assert_eq!(
            order,
            [(2, 0), (1, 0), (3, 2), (8, 3), (6, 4)],
            "{profile:?}: CBV, sampler, SRV, structured UAV, typed UAV"
        );
        assert_eq!(field(2, 12), u32::MAX, "{profile:?}: SRV is unbounded");
        let uav_upper = if profile == ShaderProfile::Sm65 {
            u32::MAX
        } else {
            0
        };
        assert_eq!(field(3, 12), uav_upper, "{profile:?}: structured UAV");
        assert_eq!(field(4, 12), uav_upper, "{profile:?}: typed UAV");
        if profile == ShaderProfile::Sm66 {
            assert_eq!(stride, 24, "{profile:?}: PSVResourceBindInfo1");
            assert_eq!(field(4, 16), 6, "{profile:?}: typed UAV DXIL kind");
        } else {
            assert_eq!(stride, 16, "{profile:?}: PSVResourceBindInfo0");
        }
        if let Ok(mut v) = super::validator::Validator::load(
            super::validator::Validator::default_path().as_deref(),
        ) {
            v.validate(&mut dxil.clone())
                .unwrap_or_else(|e| panic!("{profile:?}: {e}"));
        }
    }
}

#[test]
fn wide_image_atomics_are_rejected_only_at_sm65() {
    assert!(super::lower::check_wide_atomic(ShaderProfile::Sm65, true).is_err());
    assert!(super::lower::check_wide_atomic(ShaderProfile::Sm65, false).is_ok());
    assert!(super::lower::check_wide_atomic(ShaderProfile::Sm66, true).is_ok());
}

#[test]
fn hardware_mesh_stays_on_ms_6_6() {
    assert_eq!(super::lower::mesh_profile(), ShaderProfile::Sm66);
}

#[test]
fn auto_takes_sm65_from_a_highest_shader_model_of_6_5() {
    let resolved = ResolvedShaderProfile::resolve(ShaderProfileRequest::Auto, Some(0x65)).unwrap();
    assert_eq!(resolved.request, ShaderProfileRequest::Auto);
    assert_eq!(resolved.profile, ShaderProfile::Sm65);
    assert_eq!(resolved.highest_shader_model, Some(0x65));
}

#[test]
fn forced_sm66_is_refused_by_a_shader_model_6_5_device() {
    let error = ResolvedShaderProfile::resolve(ShaderProfileRequest::Sm66, Some(0x65)).unwrap_err();
    assert!(error.contains("forced shader profile sm66"), "{error}");
    assert!(error.contains("0x65"), "{error}");
}

#[test]
fn forced_sm66_keeps_the_sm66_hardware_path() {
    let resolved = ResolvedShaderProfile::resolve(ShaderProfileRequest::Sm66, Some(0x66)).unwrap();
    assert_eq!(resolved.profile, ShaderProfile::Sm66);
    assert_eq!(super::lower::mesh_profile(), ShaderProfile::Sm66);
}

#[test]
fn the_resolved_profile_is_what_the_translation_uses() {
    let resolved = resolved_shader_profile().unwrap();
    assert_eq!(
        resolved.profile.shader_model(),
        match resolved.profile {
            ShaderProfile::Sm65 => (6, 5),
            ShaderProfile::Sm66 => (6, 6),
        }
    );
}
