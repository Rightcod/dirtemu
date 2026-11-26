use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};

use victory_shader::{ResourceType, RootType, UsedResource};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

use super::Root;
use crate::descriptor::View;
use crate::host::HostList;
use crate::object::This;
use crate::pipeline::host::HostPipeline;

const RESOLVE_PROGRAMS: [(&str, &str); 6] = [
    (
        "CS_ScreenProbes_TemporalResolve_a911b49c",
        "2fc5d11ece80c884",
    ),
    (
        "CS_ScreenProbes_TemporalResolve_ReflectiveCaustics_a911b49c",
        "34fb51b8a0312e82",
    ),
    (
        "CS_ScreenProbes_TemporalResolve_757be8ff",
        "5053352ac4370ca1",
    ),
    (
        "CS_ScreenProbes_TemporalResolve_ReflectiveCaustics_757be8ff",
        "2bed88a03a2f5558",
    ),
    (
        "CS_ScreenProbes_TemporalResolve_CB_a1c765aa",
        "cf76630573822d04",
    ),
    (
        "CS_ScreenProbes_TemporalResolve_CB_ReflectiveCaustics_a1c765aa",
        "3b4f6eb5edb79848",
    ),
];
const LIGHT_SLOT: u8 = 1;
const LIGHT_DWORD: u32 = 176;
const DEFAULT_LEVEL: f32 = 0.2;
const TINT: [f32; 3] = [1.0, 0.98, 0.94];
const R32_TYPELESS: u32 = 39;
const R32_UINT: u32 = 42;
const R16G16B16A16_TYPELESS: u32 = 9;
const R16G16B16A16_FLOAT: u32 = 10;
const TEXTURE2D: u32 = 4;
const UNORDERED_ACCESS: u32 = 0x8;
const SLOTS: u32 = 1024;
const POLARIS_VENDOR: u32 = 0x1002;
const POLARIS_DEVICE: u32 = 0x67df;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Mode {
    Off,
    Level(f32),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ClearKind {
    Uint([u32; 4]),
    Float([f32; 4]),
}

struct Pass {
    gpu: ID3D12DescriptorHeap,
    cpu: ID3D12DescriptorHeap,
    step: u32,
    next: AtomicU32,
}

unsafe impl Send for Pass {}
unsafe impl Sync for Pass {}

fn is_polaris(vendor: u32, device: u32) -> bool {
    vendor == POLARIS_VENDOR && device == POLARIS_DEVICE
}

fn is_rx570() -> bool {
    static YES: OnceLock<bool> = OnceLock::new();
    *YES.get_or_init(|| {
        let Some(host) = crate::host::host() else {
            return false;
        };
        let factory: IDXGIFactory4 =
            match unsafe { CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)) } {
                Ok(factory) => factory,
                Err(_) => return false,
            };
        let adapter: IDXGIAdapter1 = match unsafe {
            factory.EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid())
        } {
            Ok(adapter) => adapter,
            Err(_) => return false,
        };
        let desc = match unsafe { adapter.GetDesc1() } {
            Ok(desc) => desc,
            Err(_) => return false,
        };
        is_polaris(desc.VendorId, desc.DeviceId)
    })
}

fn parse_mode(text: &str, rx570: bool) -> Result<Mode, String> {
    let text = text.trim();
    if text.eq_ignore_ascii_case("off") {
        return Ok(Mode::Off);
    }
    if text.eq_ignore_ascii_case("auto") {
        return Ok(if rx570 {
            Mode::Level(DEFAULT_LEVEL)
        } else {
            Mode::Off
        });
    }
    match text.parse::<f32>() {
        Ok(level) if level.is_finite() && (0.0..=1.0).contains(&level) => Ok(Mode::Level(level)),
        _ => Err(format!(
            "VICTORY_AMBIENT_FALLBACK={text:?} is neither off, auto nor a level in 0.0..=1.0"
        )),
    }
}

fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        let text = std::env::var("VICTORY_AMBIENT_FALLBACK").ok();
        let automatic = text
            .as_deref()
            .is_none_or(|text| text.trim().eq_ignore_ascii_case("auto"));
        let rx570 = automatic && is_rx570();
        parse_mode(text.as_deref().unwrap_or("auto"), rx570).unwrap_or_else(|error| {
            tracing::error!("{error}");
            Mode::Off
        })
    })
}

fn is_resolve(pipeline: &HostPipeline) -> bool {
    pipeline.stages.first().is_some_and(|stage| {
        RESOLVE_PROGRAMS
            .iter()
            .any(|(_, hash)| *hash == stage.shader.hash.as_str())
    })
}

fn is_light(resource: &UsedResource) -> bool {
    resource.kind == ResourceType::Uav
        && resource.root == RootType::Table
        && resource.slot == LIGHT_SLOT
        && resource.table_offset_dwords == LIGHT_DWORD
}

fn binds_light(pipeline: &HostPipeline) -> bool {
    pipeline
        .stages
        .first()
        .is_some_and(|stage| stage.translated().used.iter().any(is_light))
}

fn pack_r9g9b9e5(r: f32, g: f32, b: f32) -> u32 {
    let c = [
        r.clamp(0.0, 65408.0),
        g.clamp(0.0, 65408.0),
        b.clamp(0.0, 65408.0),
    ];
    let most = c[0].max(c[1]).max(c[2]);
    if most <= 0.0 {
        return 0;
    }
    let mut exponent = (most.log2().floor() as i32 + 16).clamp(0, 31);
    let mut scale = 2.0f32.powi(24 - exponent);
    if most * scale >= 511.5 {
        exponent = (exponent + 1).min(31);
        scale = 2.0f32.powi(24 - exponent);
    }
    let mantissa = c.map(|value| (value * scale + 0.5).floor().min(511.0) as u32);
    mantissa[0] | (mantissa[1] << 9) | (mantissa[2] << 18) | (exponent as u32) << 27
}

#[cfg(test)]
fn unpack_r9g9b9e5(value: u32) -> [f32; 3] {
    const BITS: u32 = 0x1ff;
    let scale = 2.0f32.powi((value >> 27) as i32 - 24);
    [
        (value & BITS) as f32 * scale,
        ((value >> 9) & BITS) as f32 * scale,
        ((value >> 18) & BITS) as f32 * scale,
    ]
}

fn clear_kind(
    level: f32,
    view_format: u32,
    resource_format: u32,
    dimension: u32,
) -> Result<ClearKind, String> {
    if !level.is_finite() || !(0.0..=1.0).contains(&level) {
        return Err(format!("ambient level {level} is outside 0.0..=1.0"));
    }
    if dimension != TEXTURE2D {
        return Err(format!(
            "the light UAV is not a 2D texture (dimension {dimension})"
        ));
    }
    let compatible = view_format == resource_format
        || (resource_format == R32_TYPELESS && view_format == R32_UINT)
        || (resource_format == R16G16B16A16_TYPELESS && view_format == R16G16B16A16_FLOAT);
    if !compatible {
        return Err(format!(
            "the light UAV view format {view_format} does not match its resource format {resource_format}"
        ));
    }
    match view_format {
        R32_UINT => Ok(ClearKind::Uint([
            pack_r9g9b9e5(TINT[0] * level, TINT[1] * level, TINT[2] * level),
            0,
            0,
            0,
        ])),
        R16G16B16A16_FLOAT => Ok(ClearKind::Float([
            TINT[0] * level,
            TINT[1] * level,
            TINT[2] * level,
            1.0,
        ])),
        _ => Err(format!(
            "the light UAV format {view_format} is neither R32_UINT nor R16G16B16A16_FLOAT"
        )),
    }
}

fn fail(message: String) {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|error| error.into_inner());
    if seen.iter().any(|entry| entry == &message) {
        return;
    }
    seen.push(message.clone());
    tracing::error!("{message}");
}

fn pass() -> Option<&'static Pass> {
    static PASS: OnceLock<Option<Pass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = crate::host::host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        let make = |flags: D3D12_DESCRIPTOR_HEAP_FLAGS| unsafe {
            host.device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: SLOTS,
                    Flags: flags,
                    NodeMask: 0,
                })
                .ok()
        };
        let gpu = make(D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE)?;
        let cpu = make(D3D12_DESCRIPTOR_HEAP_FLAG_NONE)?;
        let step = unsafe { host.device.GetDescriptorHandleIncrementSize(kind) };
        Some(Pass {
            gpu,
            cpu,
            step,
            next: AtomicU32::new(0),
        })
    })
    .as_ref()
}

fn uav_desc(words: &[u32; 10]) -> D3D12_UNORDERED_ACCESS_VIEW_DESC {
    let mut words = *words;
    words[0] = crate::resource::pc_format(words[0]);
    unsafe {
        std::ptr::from_ref(&words)
            .cast::<D3D12_UNORDERED_ACCESS_VIEW_DESC>()
            .read_unaligned()
    }
}

fn title_heaps(heaps: &[usize; 2]) -> Vec<Option<ID3D12DescriptorHeap>> {
    heaps
        .iter()
        .filter(|&&heap| heap != 0)
        .filter_map(|&heap| crate::descriptor::heap(heap as This).host())
        .map(|heap| Some(heap.heap.clone()))
        .collect()
}

fn uav_barrier(list: &HostList) {
    let barrier = D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
        ..Default::default()
    };
    unsafe { list.list.ResourceBarrier(&[barrier]) };
}

fn clear(
    list: &HostList,
    resource: This,
    host: &ID3D12Resource,
    desc: &[u32; 10],
    kind: ClearKind,
    heaps: &[usize; 2],
) -> bool {
    let Some(pass) = pass() else {
        fail("ambient fallback: no descriptor heap for the light UAV".to_owned());
        return false;
    };
    let Some(device) = crate::host::host().map(|host| &host.device) else {
        fail("ambient fallback: no host device".to_owned());
        return false;
    };
    let slot = pass.next.fetch_add(1, Ordering::Relaxed) % SLOTS;
    let view = uav_desc(desc);
    let cpu = unsafe { pass.cpu.GetCPUDescriptorHandleForHeapStart() };
    let gpu = unsafe { pass.gpu.GetGPUDescriptorHandleForHeapStart() };
    let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
        ptr: cpu.ptr + (slot * pass.step) as usize,
    };
    let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
        ptr: gpu.ptr + u64::from(slot * pass.step),
    };
    crate::list::ensure_state(list, resource, UNORDERED_ACCESS);
    unsafe {
        device.CreateUnorderedAccessView(host, None::<&ID3D12Resource>, Some(&view), cpu);
        let visible = pass.gpu.GetCPUDescriptorHandleForHeapStart();
        device.CopyDescriptorsSimple(
            1,
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: visible.ptr + (slot * pass.step) as usize,
            },
            cpu,
            D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        );
        list.list.SetDescriptorHeaps(&[Some(pass.gpu.clone())]);
        uav_barrier(list);
        match kind {
            ClearKind::Uint(values) => {
                list.list
                    .ClearUnorderedAccessViewUint(gpu, cpu, Some(host), &values, &[])
            }
            ClearKind::Float(values) => {
                list.list
                    .ClearUnorderedAccessViewFloat(gpu, cpu, Some(host), &values, &[])
            }
        }
        uav_barrier(list);
        list.list.SetDescriptorHeaps(&title_heaps(heaps));
    }
    true
}

pub(super) fn after_dispatch(
    list: &HostList,
    pipeline: &HostPipeline,
    roots: &[Root],
    heaps: &[usize; 2],
) {
    if !is_resolve(pipeline) {
        return;
    }
    let Mode::Level(level) = mode() else {
        return;
    };
    if !binds_light(pipeline) {
        fail(
            "ambient fallback: the resolve program does not bind a UAV at table 1 offset 176"
                .to_owned(),
        );
        return;
    }
    let Some(Root::Table(table)) = roots.get(usize::from(LIGHT_SLOT)) else {
        fail("ambient fallback: the resolve program did not set its light table".to_owned());
        return;
    };
    let handle = *table + u64::from(LIGHT_DWORD) * 4;
    let (resource, desc) = match crate::descriptor::view(handle) {
        Some(View::Unordered(resource, _, Some(desc))) => (resource, desc),
        Some(View::Unordered(_, _, None)) => {
            fail("ambient fallback: the light UAV has no view description".to_owned());
            return;
        }
        Some(View::Placed(..)) => {
            fail(
                "ambient fallback: the light UAV is a placed view, which is not mirrored"
                    .to_owned(),
            );
            return;
        }
        _ => {
            fail(
                "ambient fallback: the light table entry is not an unordered access view"
                    .to_owned(),
            );
            return;
        }
    };
    let Some(host) = crate::resource::host_resource(resource as This) else {
        fail("ambient fallback: the light UAV has no host resource".to_owned());
        return;
    };
    let description = unsafe { host.GetDesc() };
    if description.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D {
        fail(format!(
            "ambient fallback: the light resource dimension is {:?}",
            description.Dimension
        ));
        return;
    }
    let kind = match clear_kind(
        level,
        crate::resource::pc_format(desc[0]),
        description.Format.0 as u32,
        desc[1],
    ) {
        Ok(kind) => kind,
        Err(error) => {
            fail(error);
            return;
        }
    };
    if !clear(list, resource as This, host, &desc, kind, heaps) {
        return;
    }
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing::info!(
            level,
            width = description.Width,
            height = description.Height,
            format = description.Format.0,
            "ambient fallback after the screen-probe resolve"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn used(kind: ResourceType, slot: u8, offset: u32) -> UsedResource {
        UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: offset,
            lower_bound: 36,
            stride: 4,
            dwords: 8,
            user_data: 5,
            slot,
        }
    }

    #[cfg(feature = "diagnostics")]
    #[test]
    fn clears_light_uav_and_reads_pixels_on_gpu() {
        use windows::Win32::Graphics::Dxgi::Common::*;
        let Some(host) = crate::host::host() else {
            eprintln!("skipping light clear regression: no D3D12 device");
            return;
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 4,
            Height: 2,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R32_UINT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut texture: Option<ID3D12Resource> = None;
        unsafe {
            host.device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    None,
                    &mut texture,
                )
                .unwrap();
        }
        let texture = texture.unwrap();
        let list = host.new_list().unwrap();
        let expected = pack_r9g9b9e5(0.2, 0.196, 0.188);
        assert!(clear(
            &list,
            std::ptr::null_mut(),
            &texture,
            &[R32_UINT, TEXTURE2D, 0, 0, 0, 0, 0, 0, 0, 0],
            ClearKind::Uint([expected, 0, 0, 0]),
            &[0, 0],
        ));
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &texture,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
        }
        let (back, footprint) =
            crate::capture::read_back(&host.device, &list.list, &texture).unwrap();
        assert!(host.log_messages().is_none());
        list.close();
        host.execute(&[&list]);
        unsafe {
            host.device.GetDeviceRemovedReason().unwrap();
            let mut mapped = std::ptr::null_mut();
            back.Map(0, None, Some(&mut mapped)).unwrap();
            for row in 0..2usize {
                let start = mapped
                    .cast::<u8>()
                    .add(footprint.Offset as usize + row * footprint.Footprint.RowPitch as usize);
                for column in 0..4usize {
                    assert_eq!(
                        start.add(column * 4).cast::<u32>().read_unaligned(),
                        expected
                    );
                }
            }
            back.Unmap(0, None);
        }
    }

    #[test]
    fn known_resolve_hashes_only() {
        for (name, hash) in RESOLVE_PROGRAMS {
            assert!(name.starts_with("CS_ScreenProbes_TemporalResolve"));
            assert_eq!(hash.len(), 16);
        }
        assert!(
            RESOLVE_PROGRAMS
                .iter()
                .any(|(_, hash)| *hash == "2fc5d11ece80c884")
        );
        assert!(
            RESOLVE_PROGRAMS
                .iter()
                .any(|(_, hash)| *hash == "3b4f6eb5edb79848")
        );
        assert!(
            !RESOLVE_PROGRAMS
                .iter()
                .any(|(_, hash)| *hash == "0000000000000000")
        );
    }

    #[test]
    fn light_binding_is_table_one_offset_176() {
        assert!(is_light(&used(ResourceType::Uav, 1, 176)));
        assert!(!is_light(&used(ResourceType::Srv, 1, 176)));
        assert!(!is_light(&used(ResourceType::Uav, 0, 176)));
        assert!(!is_light(&used(ResourceType::Uav, 1, 168)));
        assert!(!is_light(&used(ResourceType::Uav, 1, 184)));
    }

    #[test]
    fn packed_light_round_trips() {
        for level in [0.0, 0.05, 0.2, 0.6, 1.0] {
            let packed = pack_r9g9b9e5(level, level * 0.98, level * 0.94);
            let [r, g, b] = unpack_r9g9b9e5(packed);
            assert!((r - level).abs() < 0.002, "{level} -> {r}");
            assert!((g - level * 0.98).abs() < 0.002, "{level} -> {g}");
            assert!((b - level * 0.94).abs() < 0.002, "{level} -> {b}");
        }
        assert_eq!(pack_r9g9b9e5(0.0, 0.0, 0.0), 0);
    }

    #[test]
    fn clear_kind_accepts_only_known_light_formats() {
        assert_eq!(
            clear_kind(0.2, R32_UINT, R32_UINT, TEXTURE2D),
            Ok(ClearKind::Uint([pack_r9g9b9e5(0.2, 0.196, 0.188), 0, 0, 0]))
        );
        assert_eq!(
            clear_kind(0.2, R32_UINT, R32_TYPELESS, TEXTURE2D),
            Ok(ClearKind::Uint([pack_r9g9b9e5(0.2, 0.196, 0.188), 0, 0, 0]))
        );
        match clear_kind(0.2, R16G16B16A16_FLOAT, R16G16B16A16_FLOAT, TEXTURE2D) {
            Ok(ClearKind::Float([r, g, b, a])) => {
                assert!((r - 0.2).abs() < 1e-6);
                assert!((g - 0.196).abs() < 1e-6);
                assert!((b - 0.188).abs() < 1e-6);
                assert_eq!(a, 1.0);
            }
            other => panic!("{other:?}"),
        }
        assert!(clear_kind(0.2, 41, 41, TEXTURE2D).is_err());
        assert!(clear_kind(0.2, 67, 67, TEXTURE2D).is_err());
        assert!(clear_kind(0.2, 28, 28, TEXTURE2D).is_err());
        assert!(clear_kind(0.2, R32_UINT, R16G16B16A16_FLOAT, TEXTURE2D).is_err());
        assert!(clear_kind(0.2, R32_UINT, R32_UINT, 5).is_err());
        assert!(clear_kind(1.1, R32_UINT, R32_UINT, TEXTURE2D).is_err());
        assert!(clear_kind(f32::NAN, R32_UINT, R32_UINT, TEXTURE2D).is_err());
    }

    #[test]
    fn clear_view_keeps_format_dimension_and_mip() {
        let view = uav_desc(&[R32_UINT, TEXTURE2D, 2, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(view.Format.0 as u32, R32_UINT);
        assert_eq!(view.ViewDimension, D3D12_UAV_DIMENSION_TEXTURE2D);
        assert_eq!(unsafe { view.Anonymous.Texture2D.MipSlice }, 2);
    }

    #[test]
    fn mode_parsing_is_bounded() {
        assert_eq!(parse_mode("off", true).unwrap(), Mode::Off);
        assert_eq!(
            parse_mode("auto", true).unwrap(),
            Mode::Level(DEFAULT_LEVEL)
        );
        assert_eq!(parse_mode("auto", false).unwrap(), Mode::Off);
        assert_eq!(parse_mode("0.35", false).unwrap(), Mode::Level(0.35));
        assert_eq!(parse_mode("0", true).unwrap(), Mode::Level(0.0));
        assert!(parse_mode("-0.1", true).is_err());
        assert!(parse_mode("1.1", true).is_err());
        assert!(parse_mode("nan", true).is_err());
        assert!(parse_mode("on", true).is_err());
    }

    #[test]
    fn polaris_rx570_profile() {
        assert!(is_polaris(0x1002, 0x67df));
        assert!(!is_polaris(0x10de, 0x67df));
        assert!(!is_polaris(0x1002, 0x73ff));
    }
}
