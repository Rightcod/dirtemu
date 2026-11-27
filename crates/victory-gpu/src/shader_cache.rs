//! Translated shaders kept on disk (decision 0003): under the configured
//! cache folder, keyed by the container's content, translator sources and
//! settings. Entries go through
//! `victory-pipeline`'s content-addressed cache, which writes them
//! atomically; an entry that does not read back is translated again.
//!
//! The entry is our own encoding of [`HostShader`]: a field added to it
//! must be added to `encode` and `decode` too (entries of older builds
//! are ignored anyway).

use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use victory_pipeline::{Cache, Converter};
use victory_shader::emit::{
    DescriptorAlias, DescriptorClass, HostShader, RayProfile, ShaderProfile, StaticSampler,
    UserData,
};
use victory_shader::resource::RootIndexedView;
use victory_shader::{HwStage, RootType};

static CACHE: OnceLock<Cache> = OnceLock::new();
static HITS: AtomicU64 = AtomicU64::new(0);
static MISSES: AtomicU64 = AtomicU64::new(0);

const DESCRIPTOR_ALIAS_ABI: u32 = 1;
const DIRECT_HEAP_POLICY: u32 = 1;

#[derive(Clone, Copy, PartialEq, Eq)]
struct Key {
    mesh_fallback: bool,
    profile: Option<ShaderProfile>,
    ray_profile: RayProfile,
    descriptor_alias_abi: u32,
    direct_heap_policy: u32,
    target: [u8; 12],
}

const KEY_BYTES: usize = 32 + 1 + 1 + 8 + 4 + 4 + 12;

/// Keeps translated shaders in `dir` from now on.
pub fn use_shader_cache(dir: PathBuf) {
    match Cache::open(dir) {
        Ok(cache) => {
            let _ = CACHE.set(cache);
        }
        Err(e) => tracing::warn!(error = %e, "no shader cache"),
    }
}

/// Shaders found in the cache and translated since the start.
pub(crate) fn counts() -> (u64, u64) {
    (HITS.load(Ordering::Relaxed), MISSES.load(Ordering::Relaxed))
}

/// Translator source and settings, independent of unrelated host changes.
pub(crate) fn build() -> &'static [u8; 32] {
    static BUILD: OnceLock<[u8; 32]> = OnceLock::new();
    BUILD.get_or_init(|| {
        let mut hash = blake3::Hasher::new();
        hash.update(env!("VICTORY_TRANSLATOR_SOURCE").as_bytes());
        let limit = victory_shader::emit::loop_limit();
        if limit != victory_shader::emit::DEFAULT_LOOP_LIMIT {
            tracing::warn!(
                limit,
                "VICTORY_SHADER_LOOP_LIMIT: translated loops stop sooner"
            );
        }
        hash.update(&limit.to_le_bytes());
        hash.update(&[victory_shader::emit::group_sync()]);
        for name in env!("VICTORY_TRANSLATOR_SWITCHES").split(',') {
            hash.update(&(name.len() as u64).to_le_bytes());
            hash.update(name.as_bytes());
            match std::env::var_os(name) {
                Some(value) => {
                    let bytes = value.as_encoded_bytes();
                    hash.update(&[1]);
                    hash.update(&(bytes.len() as u64).to_le_bytes());
                    hash.update(bytes);
                }
                None => {
                    hash.update(&[0]);
                }
            }
        }
        hash.update(&victory_shader::emit::shader_opt().to_le_bytes());
        *hash.finalize().as_bytes()
    })
}

/// The translation of a console shader container, from the cache when
/// there is one.
pub(crate) fn host_shader(container: &[u8], mesh_fallback: bool) -> Result<HostShader, String> {
    let mode = if mesh_fallback {
        victory_shader::emit::MeshTranslation::ComputeFallback
    } else {
        victory_shader::emit::MeshTranslation::Hardware
    };
    let translate =
        move |c: &[u8]| victory_shader::emit::translate_mesh(c, mode).map_err(|e| e.to_string());
    let key = Key {
        mesh_fallback,
        profile: victory_shader::emit::resolved_shader_profile()
            .ok()
            .map(|resolved| resolved.profile),
        ray_profile: victory_shader::emit::bvh_profile(),
        descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI,
        direct_heap_policy: DIRECT_HEAP_POLICY,
        target: victory_shader::emit::shader_target_key(),
    };
    match CACHE.get() {
        Some(cache) => cached_variant(cache, build(), container, key, translate),
        None => translate(container),
    }
}

pub fn precompile_setup() -> Result<blake3::Hash, String> {
    crate::host::host().ok_or("no D3D12 device for shader precompilation")?;
    crate::descriptor::host::probe_view_limit();
    let profile = victory_shader::emit::resolved_shader_profile()?.profile;
    let mut key = blake3::Hasher::new();
    key.update(build());
    key.update(&[
        profile.cache_encoding(),
        u8::from(crate::mesh_fallback::enabled()),
    ]);
    key.update(
        &victory_shader::emit::bvh_profile()
            .cache_encoding()
            .to_le_bytes(),
    );
    key.update(&victory_shader::emit::shader_target_key());
    Ok(key.finalize())
}

pub fn precompile_shader(container: &[u8]) -> Result<(), String> {
    let parsed = victory_shader::Container::parse(container).map_err(|e| e.to_string())?;
    let programs = victory_shader::container_programs(&parsed).map_err(|e| e.to_string())?;
    let mesh = programs.iter().any(|p| p.stage == HwStage::Ms);
    host_shader(container, mesh && crate::mesh_fallback::enabled()).map(|_| ())
}

/// The cache entry's converter: the build and the container in, the
/// encoded translation out. `ran` tells a miss from a hit.
struct Translate<F> {
    translate: F,
    ran: AtomicBool,
    container: usize,
}

impl<F: Fn(&[u8]) -> Result<HostShader, String> + Send + Sync> Converter for Translate<F> {
    fn id(&self) -> &'static str {
        "shader.dxil"
    }
    fn version(&self) -> u32 {
        // Bump with the entry encoding and key layout.
        6
    }
    fn convert(&self, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        self.ran.store(true, Ordering::Relaxed);
        let shader = (self.translate)(&input[self.container..]).map_err(anyhow::Error::msg)?;
        Ok(encode(&shader))
    }
}

#[cfg(test)]
fn cached(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    cached_variant(
        cache,
        build,
        container,
        Key {
            mesh_fallback: false,
            profile: None,
            ray_profile: RayProfile::Full,
            descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI,
            direct_heap_policy: DIRECT_HEAP_POLICY,
            target: [0; 12],
        },
        translate,
    )
}

fn cached_variant(
    cache: &Cache,
    build: &[u8; 32],
    container: &[u8],
    key: Key,
    translate: impl Fn(&[u8]) -> Result<HostShader, String> + Send + Sync,
) -> Result<HostShader, String> {
    let mut input = Vec::with_capacity(KEY_BYTES + container.len());
    input.extend_from_slice(build);
    input.push(u8::from(key.mesh_fallback));
    input.push(key.profile.map_or(0, ShaderProfile::cache_encoding));
    input.extend_from_slice(&key.ray_profile.cache_encoding().to_le_bytes());
    input.extend_from_slice(&key.descriptor_alias_abi.to_le_bytes());
    input.extend_from_slice(&key.direct_heap_policy.to_le_bytes());
    input.extend_from_slice(&key.target);
    input.extend_from_slice(container);
    let converter = Translate {
        translate,
        ran: AtomicBool::new(false),
        container: KEY_BYTES,
    };
    for _ in 0..2 {
        let path = cache
            .get_or_convert(&converter, &input)
            .map_err(|e| format!("{e:#}"))?;
        if let Some(shader) = std::fs::read(&path).ok().and_then(|b| decode(&b)) {
            let counter = if converter.ran.load(Ordering::Relaxed) {
                &MISSES
            } else {
                &HITS
            };
            counter.fetch_add(1, Ordering::Relaxed);
            return Ok(shader);
        }
        let _ = std::fs::remove_file(&path);
    }
    MISSES.fetch_add(1, Ordering::Relaxed);
    (converter.translate)(container)
}

const MAGIC: &[u8; 4] = b"VSH3";
const STAGES: [HwStage; 8] = [
    HwStage::Ls,
    HwStage::Hs,
    HwStage::Es,
    HwStage::Gs,
    HwStage::Vs,
    HwStage::Ps,
    HwStage::Ms,
    HwStage::Cs,
];

fn root_code(r: RootType) -> u32 {
    match r {
        RootType::Constants => 0,
        RootType::Descriptor => 1,
        RootType::Table => 2,
        RootType::ImmediateSampler => 3,
        RootType::ResourceHeap => 4,
        RootType::SamplerHeap => 5,
        RootType::Other(x) => 6 + x,
    }
}

fn root_type(code: u32) -> RootType {
    match code {
        0 => RootType::Constants,
        1 => RootType::Descriptor,
        2 => RootType::Table,
        3 => RootType::ImmediateSampler,
        4 => RootType::ResourceHeap,
        5 => RootType::SamplerHeap,
        x => RootType::Other(x - 6),
    }
}

/// Little-endian words and length-prefixed lists.
fn encode(s: &HostShader) -> Vec<u8> {
    let mut out = MAGIC.to_vec();
    let word = |out: &mut Vec<u8>, v: u32| out.extend(v.to_le_bytes());
    let stage = STAGES.iter().position(|&x| x == s.stage).unwrap_or(0) as u32;
    word(&mut out, stage);
    word(&mut out, u32::from(s.profile.cache_encoding()));
    word(&mut out, s.dxil.len() as u32);
    out.extend(&s.dxil);
    word(&mut out, s.descriptor_aliases.len() as u32);
    for alias in &s.descriptor_aliases {
        word(&mut out, u32::from(alias.class.code()));
        word(&mut out, alias.space);
    }
    word(&mut out, u32::from(s.direct_heap));
    word(&mut out, u32::from(s.mesh_fallback.is_some()));
    if let Some(mesh) = &s.mesh_fallback {
        word(&mut out, mesh.vertex_shader.len() as u32);
        out.extend(&mesh.vertex_shader);
        for value in [
            mesh.group_bytes,
            mesh.vertex_stride,
            mesh.primitive_stride,
            mesh.max_vertices,
            mesh.max_primitives,
        ] {
            word(&mut out, value);
        }
    }
    word(&mut out, s.user_data.len() as u32);
    for u in &s.user_data {
        for v in [u.index, u32::from(u.slot), root_code(u.root), u.dword] {
            word(&mut out, v);
        }
    }
    word(&mut out, s.samplers.len() as u32);
    for sampler in &s.samplers {
        for &v in sampler {
            word(&mut out, v);
        }
    }
    word(&mut out, s.static_samplers.len() as u32);
    for t in &s.static_samplers {
        let [u, v, w] = t.address;
        let words = [
            t.filter,
            u,
            v,
            w,
            t.mip_lod_bias.to_bits(),
            t.max_anisotropy,
            t.comparison,
            t.border,
            t.min_lod.to_bits(),
            t.max_lod.to_bits(),
        ];
        for v in words {
            word(&mut out, v);
        }
    }
    for slots in [&s.root_cbvs, &s.root_srvs, &s.root_uavs] {
        word(&mut out, slots.len() as u32);
        out.extend(slots);
    }
    word(&mut out, s.root_indexed_views.len() as u32);
    for view in &s.root_indexed_views {
        for value in [
            u32::from(view.table_slot),
            u32::from(view.constant_slot),
            view.constant_dword,
            view.byte_offset,
            u32::from(view.shift),
        ] {
            word(&mut out, value);
        }
    }
    out
}

fn push_alias(shader: &mut HostShader, class: u32, space: u32) -> Option<()> {
    let class = match class {
        0 => DescriptorClass::Srv,
        1 => DescriptorClass::Uav,
        2 => DescriptorClass::Cbv,
        3 => DescriptorClass::Sampler,
        _ => return None,
    };
    shader
        .descriptor_aliases
        .push(DescriptorAlias { class, space });
    Some(())
}

fn decode(bytes: &[u8]) -> Option<HostShader> {
    struct Reader<'a>(&'a [u8]);
    impl<'a> Reader<'a> {
        fn take(&mut self, n: usize) -> Option<&'a [u8]> {
            let (head, rest) = self.0.split_at_checked(n)?;
            self.0 = rest;
            Some(head)
        }
        fn word(&mut self) -> Option<u32> {
            Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
        }
        fn list(&mut self) -> Option<Vec<u8>> {
            let n = self.word()? as usize;
            Some(self.take(n)?.to_vec())
        }
    }
    let mut r = Reader(bytes);
    if r.take(4)? != MAGIC {
        return None;
    }
    let stage = *STAGES.get(r.word()? as usize)?;
    let profile = match r.word()? {
        0x65 => ShaderProfile::Sm65,
        0x66 => ShaderProfile::Sm66,
        _ => return None,
    };
    let dxil = r.list()?;
    let n = r.word()? as usize;
    let mut descriptor_aliases = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        descriptor_aliases.push((r.word()?, r.word()?));
    }
    let direct_heap = match r.word()? {
        0 => false,
        1 => true,
        _ => return None,
    };
    let mesh_fallback = match r.word()? {
        0 => None,
        1 => {
            let vertex_shader = r.list()?;
            Some(victory_shader::emit::MeshFallback {
                vertex_shader,
                group_bytes: r.word()?,
                vertex_stride: r.word()?,
                primitive_stride: r.word()?,
                max_vertices: r.word()?,
                max_primitives: r.word()?,
            })
        }
        _ => return None,
    };
    let n = r.word()? as usize;
    let mut user_data = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        user_data.push(UserData {
            index: r.word()?,
            slot: u8::try_from(r.word()?).ok()?,
            root: root_type(r.word()?),
            dword: r.word()?,
        });
    }
    let n = r.word()? as usize;
    let mut samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        samplers.push([r.word()?, r.word()?, r.word()?, r.word()?]);
    }
    let n = r.word()? as usize;
    let mut static_samplers = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        static_samplers.push(StaticSampler {
            filter: r.word()?,
            address: [r.word()?, r.word()?, r.word()?],
            mip_lod_bias: f32::from_bits(r.word()?),
            max_anisotropy: r.word()?,
            comparison: r.word()?,
            border: r.word()?,
            min_lod: f32::from_bits(r.word()?),
            max_lod: f32::from_bits(r.word()?),
        });
    }
    let root_cbvs = r.list()?;
    let root_srvs = r.list()?;
    let root_uavs = r.list()?;
    let n = r.word()? as usize;
    let mut root_indexed_views = Vec::with_capacity(n.min(256));
    for _ in 0..n {
        root_indexed_views.push(RootIndexedView {
            table_slot: u8::try_from(r.word()?).ok()?,
            constant_slot: u8::try_from(r.word()?).ok()?,
            constant_dword: r.word()?,
            byte_offset: r.word()?,
            shift: u8::try_from(r.word()?).ok().filter(|&s| s < 32)?,
        });
    }
    if !r.0.is_empty() {
        return None;
    }
    let mut shader = HostShader {
        stage,
        profile,
        dxil,
        descriptor_aliases: Vec::new(),
        direct_heap,
        mesh_fallback,
        user_data,
        samplers,
        static_samplers,
        root_cbvs,
        root_srvs,
        root_uavs,
        root_indexed_views,
    };
    for (class, space) in descriptor_aliases {
        push_alias(&mut shader, class, space)?;
    }
    Some(shader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shader() -> HostShader {
        shader_for(ShaderProfile::Sm65)
    }

    fn shader_for(profile: ShaderProfile) -> HostShader {
        let mut shader = HostShader {
            stage: HwStage::Ps,
            profile,
            dxil: b"DXBC and the rest".to_vec(),
            descriptor_aliases: Vec::new(),
            direct_heap: true,
            mesh_fallback: Some(victory_shader::emit::MeshFallback {
                vertex_shader: b"vertex DXBC".to_vec(),
                group_bytes: 256,
                vertex_stride: 32,
                primitive_stride: 16,
                max_vertices: 8,
                max_primitives: 4,
            }),
            user_data: vec![UserData {
                index: 8,
                slot: 1,
                root: RootType::Table,
                dword: 0,
            }],
            samplers: vec![[0x400, 0xc0_0000, 0x18f0_0000, 0]],
            static_samplers: vec![StaticSampler::from_words([
                0x400,
                0xc0_0000,
                0x18f0_0000,
                0,
            ])],
            root_cbvs: vec![2],
            root_srvs: vec![0, 3],
            root_uavs: vec![1],
            root_indexed_views: vec![RootIndexedView {
                table_slot: 1,
                constant_slot: 2,
                constant_dword: 3,
                byte_offset: 32,
                shift: 5,
            }],
        };
        push_alias(&mut shader, 2, 7).unwrap();
        push_alias(&mut shader, 3, 2).unwrap();
        shader
    }

    #[test]
    fn translations_come_back_from_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [7u8; 32];
        let container = b"a console container".as_slice();
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 1, "the second came from disk");

        // Another build translates again.
        assert_eq!(cached(&cache, &[8; 32], container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 2);

        // A damaged entry is a miss, translated and stored again.
        for e in walk(dir.path()) {
            std::fs::write(e, b"VSH3 broken").unwrap();
        }
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        assert_eq!(cached(&cache, &build, container, translate), Ok(shader()));
        assert_eq!(runs.load(Ordering::Relaxed), 3);

        // Failures are not stored.
        let fail = |_: &[u8]| Err::<HostShader, _>("unsupported".to_owned());
        assert!(cached(&cache, &build, b"other", fail).is_err());
    }

    #[test]
    fn mesh_translation_variants_have_distinct_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [3u8; 32];
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        let hardware_65 = Key {
            target: [0; 12],
            mesh_fallback: false,
            profile: Some(ShaderProfile::Sm65),
            ray_profile: RayProfile::Full,
            descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI,
            direct_heap_policy: DIRECT_HEAP_POLICY,
        };
        let fallback_65 = Key {
            mesh_fallback: true,
            ..hardware_65
        };
        let hardware_66 = Key {
            profile: Some(ShaderProfile::Sm66),
            ..hardware_65
        };
        let fallback_66 = Key {
            mesh_fallback: true,
            ..hardware_66
        };
        cached_variant(&cache, &build, b"mesh", hardware_65, translate).unwrap();
        cached_variant(&cache, &build, b"mesh", hardware_65, translate).unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 1);
        cached_variant(&cache, &build, b"mesh", fallback_65, translate).unwrap();
        cached_variant(&cache, &build, b"mesh", fallback_65, translate).unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        cached_variant(&cache, &build, b"mesh", hardware_66, translate).unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 3);
        cached_variant(&cache, &build, b"mesh", fallback_66, translate).unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 4);
    }

    #[test]
    fn ray_profiles_and_budgets_have_distinct_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [9u8; 32];
        let runs = AtomicU64::new(0);
        let base = Key {
            target: [0; 12],
            mesh_fallback: false,
            profile: Some(ShaderProfile::Sm65),
            ray_profile: RayProfile::Full,
            descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI,
            direct_heap_policy: DIRECT_HEAP_POLICY,
        };
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        for (n, profile) in [
            RayProfile::Full,
            RayProfile::StaticMiss,
            RayProfile::Limited(1),
            RayProfile::Limited(2),
            RayProfile::Limited(17),
        ]
        .into_iter()
        .enumerate()
        {
            let key = Key {
                ray_profile: profile,
                ..base
            };
            cached_variant(&cache, &build, b"ray", key, translate).unwrap();
            assert_eq!(runs.load(Ordering::Relaxed), n as u64 + 1, "{profile:?}");
            cached_variant(&cache, &build, b"ray", key, translate).unwrap();
            assert_eq!(runs.load(Ordering::Relaxed), n as u64 + 1, "{profile:?}");
        }
    }

    #[test]
    fn alias_abi_and_direct_heap_policy_have_distinct_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [5u8; 32];
        let runs = AtomicU64::new(0);
        let translate = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader())
        };
        let base = Key {
            target: [0; 12],
            mesh_fallback: false,
            profile: Some(ShaderProfile::Sm65),
            ray_profile: RayProfile::Full,
            descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI,
            direct_heap_policy: DIRECT_HEAP_POLICY,
        };
        cached_variant(&cache, &build, b"abi", base, translate).unwrap();
        cached_variant(
            &cache,
            &build,
            b"abi",
            Key {
                descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI + 1,
                ..base
            },
            translate,
        )
        .unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        cached_variant(
            &cache,
            &build,
            b"abi",
            Key {
                direct_heap_policy: DIRECT_HEAP_POLICY + 1,
                ..base
            },
            translate,
        )
        .unwrap();
        assert_eq!(runs.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn profiles_do_not_share_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::open(dir.path()).unwrap();
        let build = [6u8; 32];
        let runs = AtomicU64::new(0);
        let key = |profile| Key {
            target: [0; 12],
            mesh_fallback: false,
            profile: Some(profile),
            ray_profile: RayProfile::Full,
            descriptor_alias_abi: DESCRIPTOR_ALIAS_ABI,
            direct_heap_policy: DIRECT_HEAP_POLICY,
        };
        let translate_65 = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader_for(ShaderProfile::Sm65))
        };
        let translate_66 = |_: &[u8]| {
            runs.fetch_add(1, Ordering::Relaxed);
            Ok(shader_for(ShaderProfile::Sm66))
        };
        assert_eq!(
            cached_variant(
                &cache,
                &build,
                b"profile",
                key(ShaderProfile::Sm65),
                translate_65
            )
            .unwrap()
            .profile,
            ShaderProfile::Sm65
        );
        assert_eq!(
            cached_variant(
                &cache,
                &build,
                b"profile",
                key(ShaderProfile::Sm66),
                translate_66
            )
            .unwrap()
            .profile,
            ShaderProfile::Sm66
        );
        assert_eq!(runs.load(Ordering::Relaxed), 2);
        assert_eq!(
            cached_variant(
                &cache,
                &build,
                b"profile",
                key(ShaderProfile::Sm65),
                translate_65
            )
            .unwrap()
            .profile,
            ShaderProfile::Sm65
        );
        assert_eq!(runs.load(Ordering::Relaxed), 2);
    }

    fn walk(dir: &std::path::Path) -> Vec<PathBuf> {
        let mut files = Vec::new();
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            if e.path().is_dir() {
                files.extend(walk(&e.path()));
            } else {
                files.push(e.path());
            }
        }
        files
    }

    #[test]
    fn entries_round_trip_and_reject_damage() {
        let bytes = encode(&shader());
        assert_eq!(decode(&bytes), Some(shader()));
        let decoded = decode(&bytes).unwrap();
        assert_eq!(decoded.profile, ShaderProfile::Sm65);
        assert!(decoded.direct_heap);
        assert_eq!(decoded.descriptor_aliases.len(), 2);
        assert_eq!(decoded.descriptor_aliases[0].class.code(), 2);
        assert_eq!(decoded.descriptor_aliases[0].space, 7);
        assert_eq!(decoded.descriptor_aliases[1].class.code(), 3);
        assert_eq!(decoded.descriptor_aliases[1].space, 2);
        let mut sm66 = shader_for(ShaderProfile::Sm66);
        sm66.direct_heap = false;
        assert_eq!(decode(&encode(&sm66)), Some(sm66));
        assert_eq!(decode(&bytes[..bytes.len() - 1]), None);
        assert_eq!(decode(&[bytes.as_slice(), &[0]].concat()), None);
        let mut wrong_profile = bytes.clone();
        wrong_profile[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(decode(&wrong_profile), None);
        assert_eq!(decode(b"VSH0"), None);
        assert_eq!(decode(b"VSH1"), None);
        assert_eq!(decode(b"VSH2"), None);
    }
}
