//! Host pipelines for title pipelines (decision 0007, step 3): the
//! title's graphics state with the translated shaders, under a root
//! signature of our own that the translated shaders' bindings define
//! (`victory_shader::emit`): per stage one block of root constants for the
//! user data, root views for the title's root views, static samplers for
//! the immediate samplers, and either the SM 6.5 descriptor-table aliases
//! of the dynamic heap or the SM 6.6 directly indexed heaps.

use std::ffi::{CStr, CString};
use std::sync::Arc;

use victory_shader::emit::{
    DescriptorAlias, DescriptorClass, HostShader, ROOT_CBV_SPACE, SAMPLER_SPACE, USER_DATA_SPACE,
};
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::PCSTR;

use super::{Shader, Translated};
use crate::host::host;

/// A stream's graphics state, owned (the title's stream lives only for the
/// creation call). Subobjects the stream leaves out keep D3D12's defaults.
pub(crate) struct GraphicsState {
    blend: D3D12_BLEND_DESC,
    sample_mask: u32,
    rasterizer: D3D12_RASTERIZER_DESC,
    depth: D3D12_DEPTH_STENCIL_DESC,
    /// Input elements, their names pointing into `names`.
    inputs: Vec<D3D12_INPUT_ELEMENT_DESC>,
    names: Vec<CString>,
    strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE,
    pub(crate) topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE,
    pub(crate) targets: D3D12_RT_FORMAT_ARRAY,
    depth_format: DXGI_FORMAT,
    samples: DXGI_SAMPLE_DESC,
}

impl Default for GraphicsState {
    fn default() -> Self {
        let target = D3D12_RENDER_TARGET_BLEND_DESC {
            SrcBlend: D3D12_BLEND_ONE,
            DestBlend: D3D12_BLEND_ZERO,
            BlendOp: D3D12_BLEND_OP_ADD,
            SrcBlendAlpha: D3D12_BLEND_ONE,
            DestBlendAlpha: D3D12_BLEND_ZERO,
            BlendOpAlpha: D3D12_BLEND_OP_ADD,
            LogicOp: D3D12_LOGIC_OP_NOOP,
            RenderTargetWriteMask: D3D12_COLOR_WRITE_ENABLE_ALL.0 as u8,
            ..Default::default()
        };
        let keep = D3D12_DEPTH_STENCILOP_DESC {
            StencilFailOp: D3D12_STENCIL_OP_KEEP,
            StencilDepthFailOp: D3D12_STENCIL_OP_KEEP,
            StencilPassOp: D3D12_STENCIL_OP_KEEP,
            StencilFunc: D3D12_COMPARISON_FUNC_ALWAYS,
        };
        GraphicsState {
            blend: D3D12_BLEND_DESC {
                RenderTarget: [target; 8],
                ..Default::default()
            },
            sample_mask: u32::MAX,
            rasterizer: D3D12_RASTERIZER_DESC {
                FillMode: D3D12_FILL_MODE_SOLID,
                CullMode: D3D12_CULL_MODE_BACK,
                DepthClipEnable: true.into(),
                ..Default::default()
            },
            depth: D3D12_DEPTH_STENCIL_DESC {
                DepthEnable: true.into(),
                DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                DepthFunc: D3D12_COMPARISON_FUNC_LESS,
                StencilReadMask: 0xff,
                StencilWriteMask: 0xff,
                FrontFace: keep,
                BackFace: keep,
                ..Default::default()
            },
            inputs: Vec::new(),
            names: Vec::new(),
            strip_cut: D3D12_INDEX_BUFFER_STRIP_CUT_VALUE_DISABLED,
            topology: D3D12_PRIMITIVE_TOPOLOGY_TYPE_UNDEFINED,
            targets: D3D12_RT_FORMAT_ARRAY::default(),
            depth_format: DXGI_FORMAT_UNKNOWN,
            samples: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
        }
    }
}

impl GraphicsState {
    fn uses_depth_stencil(&self) -> bool {
        self.depth.DepthEnable.as_bool() || self.depth.StencilEnable.as_bool()
    }

    /// Takes in one subobject of a stream: its type, then its data at +4
    /// (+8 for those holding pointers).
    ///
    /// # Safety
    /// `at` points at a whole subobject of type `kind`.
    pub(crate) unsafe fn read(&mut self, kind: u32, at: *const u8) {
        // SAFETY: the caller passes a whole subobject.
        unsafe {
            let data = at.add(4);
            match kind {
                8 => self.blend = data.cast::<D3D12_BLEND_DESC>().read_unaligned(),
                9 => self.sample_mask = data.cast::<u32>().read_unaligned(),
                10 => self.rasterizer = data.cast::<D3D12_RASTERIZER_DESC>().read_unaligned(),
                11 => self.depth = data.cast::<D3D12_DEPTH_STENCIL_DESC>().read_unaligned(),
                21 => {
                    // shortcut: the depth bounds test is dropped (the host
                    // pipeline uses the plain description); bring it back
                    // with a stream if a title enables it.
                    let d = data.cast::<D3D12_DEPTH_STENCIL_DESC1>().read_unaligned();
                    if d.DepthBoundsTestEnable.as_bool() {
                        static SEEN: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(0);
                        if SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 4 {
                            tracing::warn!(
                                "a pipeline enables the depth bounds test, which its host pipeline leaves out: its draws pass at every depth"
                            );
                        }
                    }
                    self.depth = D3D12_DEPTH_STENCIL_DESC {
                        DepthEnable: d.DepthEnable,
                        DepthWriteMask: d.DepthWriteMask,
                        DepthFunc: d.DepthFunc,
                        StencilEnable: d.StencilEnable,
                        StencilReadMask: d.StencilReadMask,
                        StencilWriteMask: d.StencilWriteMask,
                        FrontFace: d.FrontFace,
                        BackFace: d.BackFace,
                    };
                }
                12 => {
                    let layout = at.add(8).cast::<D3D12_INPUT_LAYOUT_DESC>().read_unaligned();
                    if layout.pInputElementDescs.is_null() {
                        return;
                    }
                    let elements = std::slice::from_raw_parts(
                        layout.pInputElementDescs,
                        layout.NumElements as usize,
                    );
                    self.names = elements
                        .iter()
                        .map(|e| CStr::from_ptr(e.SemanticName.0.cast()).to_owned())
                        .collect();
                    self.inputs = elements
                        .iter()
                        .zip(&self.names)
                        .map(|(e, name)| D3D12_INPUT_ELEMENT_DESC {
                            SemanticName: PCSTR(name.as_ptr().cast()),
                            Format: DXGI_FORMAT(
                                crate::resource::pc_format(e.Format.0 as u32) as i32
                            ),
                            ..*e
                        })
                        .collect();
                }
                13 => {
                    self.strip_cut =
                        D3D12_INDEX_BUFFER_STRIP_CUT_VALUE(data.cast::<i32>().read_unaligned())
                }
                14 => {
                    self.topology =
                        D3D12_PRIMITIVE_TOPOLOGY_TYPE(data.cast::<i32>().read_unaligned())
                }
                15 => {
                    self.targets = data.cast::<D3D12_RT_FORMAT_ARRAY>().read_unaligned();
                    for f in &mut self.targets.RTFormats {
                        f.0 = crate::resource::target_format(crate::resource::pc_format(f.0 as u32))
                            as i32;
                    }
                }
                16 => {
                    let f = data.cast::<u32>().read_unaligned();
                    self.depth_format = DXGI_FORMAT(crate::resource::pc_format(f) as i32);
                }
                17 => self.samples = data.cast::<DXGI_SAMPLE_DESC>().read_unaligned(),
                _ => {}
            }
        }
    }

    /// Everything the state holds, as text: part of a pipeline's name in
    /// the library and of the key a title pipeline is started early by.
    pub(crate) fn describe(&self) -> String {
        let inputs: Vec<String> = self
            .inputs
            .iter()
            .zip(&self.names)
            .map(|(e, name)| {
                format!(
                    "{name:?} {} {} {} {} {} {}",
                    e.SemanticIndex,
                    e.Format.0,
                    e.InputSlot,
                    e.AlignedByteOffset,
                    e.InputSlotClass.0,
                    e.InstanceDataStepRate
                )
            })
            .collect();
        format!(
            "{:?} {} {:?} {:?} {inputs:?} {} {} {:?} {} {:?}",
            self.blend,
            self.sample_mask,
            self.rasterizer,
            self.depth,
            self.strip_cut.0,
            self.topology.0,
            self.targets,
            self.depth_format.0,
            self.samples
        )
    }

    /// The vertex buffer slots the input layout reads.
    fn slots(&self) -> u32 {
        self.inputs
            .iter()
            .fold(0, |m, e| m | 1 << e.InputSlot.min(31))
    }

    /// The input elements with their byte offsets worked out.
    fn elements(&self) -> Vec<Input> {
        let mut next = [0u32; 32];
        self.inputs
            .iter()
            .map(|e| {
                let slot = e.InputSlot.min(31) as usize;
                let format = e.Format.0 as u32;
                let offset = if e.AlignedByteOffset == D3D12_APPEND_ALIGNED_ELEMENT {
                    next[slot]
                } else {
                    e.AlignedByteOffset
                };
                let bytes = crate::resource::element(format).map_or(0, |(b, _)| b as u32);
                next[slot] = offset + bytes;
                Input {
                    slot: slot as u32,
                    offset,
                    format,
                    per_instance: e.InputSlotClass == D3D12_INPUT_CLASSIFICATION_PER_INSTANCE_DATA,
                }
            })
            .collect()
    }
}

/// One input element: vertex buffer slot, byte offset, format.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Input {
    pub(crate) slot: u32,
    pub(crate) offset: u32,
    pub(crate) format: u32,
    pub(crate) per_instance: bool,
}

/// The console's own primitive topology types.
const RECTANGLES: i32 = 19;
const QUADS: i32 = 32;

/// Whether the title's root SRVs are bound as one-view tables of the
/// buffer's host copy, whose reads past its end return zeros, rather than
/// as root descriptors, which have no size: a mesh shader's vertex fetch
/// has no record limit, and a read past the copy reaches whatever the host
/// placed after it (memory it has freed: the device is removed, upstream
/// PR #493). `VICTORY_ROOT_SRV_DIRECT=1` goes back to root descriptors.
pub(crate) fn bounded_root_srvs() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ROOT_SRV_DIRECT").is_none())
}

/// One stage of a host pipeline and where its bindings go.
pub(crate) struct Stage {
    pub(crate) shader: Arc<Shader>,
    /// The root parameter of its user data, and its size in dwords.
    pub(crate) user_data: Option<(u32, u32)>,
    /// Root parameters of its root views: (title root parameter, ours).
    pub(crate) root_cbvs: Vec<(u8, u32)>,
    pub(crate) root_srvs: Vec<(u8, u32)>,
    /// Root UAVs: one-view tables like the SRVs (or root descriptors with
    /// `VICTORY_ROOT_SRV_DIRECT=1`).
    pub(crate) root_uavs: Vec<(u8, u32)>,
    /// SM 6.5 descriptor aliases: the root parameter of each, and whether
    /// it is the sampler heap. Each is bound to the base of its heap.
    pub(crate) aliases: Vec<(u32, bool)>,
}

impl Stage {
    pub(crate) fn translated(&self) -> &Translated {
        self.shader
            .translated()
            .expect("host pipelines are made from translated shaders")
    }
}

pub(crate) struct MeshFallbackPipeline {
    pub(crate) compute: ID3D12PipelineState,
    pub(crate) compute_root: ID3D12RootSignature,
    pub(crate) output_uav: u32,
    pub(crate) output_srv: u32,
    pub(crate) group_bytes: u32,
    pub(crate) max_primitives: u32,
}

fn mesh_fallback_max_primitives(
    max_primitives: u32,
    max_vertices: u32,
    vertex_stride: u32,
    primitive_stride: u32,
    group_bytes: u32,
) -> Option<u32> {
    const DRAW_ARGUMENTS: u32 = 16;
    const INDEX_BYTES: u32 = 12;
    if max_primitives > crate::mesh_fallback::MAX_PRIMITIVES {
        return None;
    }
    let index_end = DRAW_ARGUMENTS.checked_add(INDEX_BYTES.checked_mul(max_primitives)?)?;
    let vertex_base = index_end.checked_add(15)? & !15;
    let vertex_end = vertex_base.checked_add(vertex_stride.checked_mul(max_vertices)?)?;
    let record_end = vertex_end.checked_add(primitive_stride.checked_mul(max_primitives)?)?;
    let least = record_end.checked_add(15)? & !15;
    (group_bytes >= least).then_some(max_primitives)
}

pub(crate) struct HostPipeline {
    /// A compute pipeline (else graphics).
    pub(crate) compute: bool,
    /// A mesh pipeline (graphics without vertex input).
    pub(crate) mesh: bool,
    pub(crate) depth_stencil: bool,
    pub(crate) mesh_fallback: Option<MeshFallbackPipeline>,
    pub(crate) pso: ID3D12PipelineState,
    pub(crate) root: ID3D12RootSignature,
    /// Vertex, then pixel stage, or the compute stage.
    pub(crate) stages: Vec<Stage>,
    /// The vertex buffer slots it reads (bit per slot).
    pub(crate) slots: u32,
    pub(crate) inputs: Vec<Input>,
    /// Has the geometry stage that completes the console's rectangles
    /// (`victory_shader::emit::rect_list_gs`): its rectangle draws go out
    /// as the title gives them, three vertices a rectangle.
    pub(crate) rects: bool,
    /// Its shaders, for reports of slow submissions.
    pub(crate) name: String,
    /// Its depth, stencil, cull and blend state in short, for the frame
    /// listing (empty for compute).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    pub(crate) state: String,
}

/// Logs a pipeline that gets no host pipeline, once per reason.
fn refuse(reason: &'static str, shaders: &[Arc<Shader>]) -> Option<HostPipeline> {
    static SEEN: std::sync::Mutex<Vec<&str>> = std::sync::Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if !seen.contains(&reason) {
        seen.push(reason);
        tracing::warn!(?shaders, "no host pipeline: {reason} (logged once)");
    }
    None
}

impl HostPipeline {
    /// A host pipeline for vertex and pixel shaders with the graphics
    /// state `g`, or for one compute shader.
    pub(crate) fn new(shaders: &[Arc<Shader>], g: Option<&GraphicsState>) -> Option<HostPipeline> {
        // For the report when the host device is removed: what this thread
        // has in the library from here on.
        super::library::label(format!("{shaders:?}"));
        let find = |stage| shaders.iter().find(|s| s.stage == stage);
        let compute = match shaders {
            [cs] if cs.stage == "compute" => Some(cs),
            _ => None,
        };
        if compute.is_none()
            && shaders
                .iter()
                .any(|s| !matches!(s.stage, "vertex" | "pixel" | "mesh"))
        {
            return refuse(
                "only vertex, mesh, pixel and compute stages are translated",
                shaders,
            );
        }
        let mesh = find("mesh");
        let fallback = mesh.is_some() && crate::mesh_fallback::enabled();
        let stages = if let Some(cs) = compute {
            vec![(cs, D3D12_SHADER_VISIBILITY_ALL)]
        } else {
            let first = match (mesh, find("vertex")) {
                (Some(ms), _) if fallback => (ms, D3D12_SHADER_VISIBILITY_ALL),
                (Some(ms), _) => (ms, D3D12_SHADER_VISIBILITY_MESH),
                (None, Some(vs)) => (vs, D3D12_SHADER_VISIBILITY_VERTEX),
                (None, None) => return refuse("no vertex or mesh shader", shaders),
            };
            let mut stages = vec![first];
            stages.extend(find("pixel").map(|ps| (ps, D3D12_SHADER_VISIBILITY_PIXEL)));
            stages
        };
        // The host device exists (and has fixed the shader profile) before
        // any shader is translated: a translation, and its disk-cache key,
        // must see the device's effective profile.
        let host = host()?;
        if mesh.is_some() && !fallback && !host.mesh_shaders() {
            return refuse(
                "the host GPU has no mesh shaders (or no shader model 6.6)",
                shaders,
            );
        }
        for (s, _) in &stages {
            s.translated()?;
        }

        let layout = |selected: &[(&Arc<Shader>, D3D12_SHADER_VISIBILITY)],
                      output: Option<(D3D12_ROOT_PARAMETER_TYPE, D3D12_SHADER_VISIBILITY)>,
                      compute: bool| {
            let mut translated: Vec<(&HostShader, D3D12_SHADER_VISIBILITY)> =
                Vec::with_capacity(selected.len());
            for &(shader, visibility) in selected {
                translated.push((&shader.translated()?.host, visibility));
            }
            match signature_layout(&host.device, &translated, output, compute, fallback) {
                Ok(signature) => {
                    let Signature {
                        root,
                        blob,
                        stages: layouts,
                        output,
                    } = signature;
                    let bound: Vec<Stage> = selected
                        .iter()
                        .zip(layouts)
                        .map(|(&(shader, _), layout)| Stage {
                            shader: shader.clone(),
                            user_data: layout.user_data,
                            root_cbvs: layout.root_cbvs,
                            root_srvs: layout.root_srvs,
                            root_uavs: layout.root_uavs,
                            aliases: layout.aliases,
                        })
                        .collect();
                    Some((root, blob, bound, output))
                }
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host root signature refused");
                    None
                }
            }
        };
        let (root, root_blob, compute_root, compute_root_blob, bound, mesh_params) = if fallback {
            let (compute_root, compute_root_blob, mesh_stage, output_uav) = layout(
                &stages[..1],
                Some((D3D12_ROOT_PARAMETER_TYPE_UAV, D3D12_SHADER_VISIBILITY_ALL)),
                true,
            )?;
            let (draw_root, draw_root_blob, pixel_stage, output_srv) = layout(
                &stages[1..],
                Some((
                    D3D12_ROOT_PARAMETER_TYPE_SRV,
                    D3D12_SHADER_VISIBILITY_VERTEX,
                )),
                false,
            )?;
            let mut bound = mesh_stage;
            bound.extend(pixel_stage);
            (
                draw_root,
                draw_root_blob,
                Some(compute_root),
                compute_root_blob,
                bound,
                output_uav.zip(output_srv),
            )
        } else {
            let (root, root_blob, bound, _) = layout(&stages, None, compute.is_some())?;
            (root, root_blob, None, Vec::new(), bound, None)
        };
        let code = |i: usize| {
            stages.get(i).and_then(|(s, _)| s.translated()).map_or(
                D3D12_SHADER_BYTECODE::default(),
                |t| D3D12_SHADER_BYTECODE {
                    pShaderBytecode: t.host.dxil.as_ptr().cast(),
                    BytecodeLength: t.host.dxil.len(),
                },
            )
        };
        if compute.is_some() {
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                // SAFETY: a borrowed pointer the description never releases.
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                CS: code(0),
                ..Default::default()
            };
            let name = super::library::name("compute", &root_blob, &[&code_bytes(&code(0))], "");
            let made = super::library::pipeline(
                &name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadComputePipeline(name, &desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateComputePipelineState(&desc) },
            );
            let pso = match made {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(
                ?shaders,
                profile = host.shader_profile.cache_key(),
                "host pipeline"
            );
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: true,
                mesh: false,
                depth_stencil: false,
                mesh_fallback: None,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                state: String::new(),
            });
        }
        let Some(g) = g else {
            return refuse("graphics state not kept (plain description)", shaders);
        };
        let state = g.describe();
        if fallback {
            let (output_uav, output_srv) = mesh_params?;
            let Some(emulated) = mesh
                .and_then(|ms| ms.translated())
                .and_then(|t| t.host.mesh_fallback.as_ref())
            else {
                return refuse("mesh compute fallback translation is missing", shaders);
            };
            let Some(max_primitives) = mesh_fallback_max_primitives(
                emulated.max_primitives,
                emulated.max_vertices,
                emulated.vertex_stride,
                emulated.primitive_stride,
                emulated.group_bytes,
            ) else {
                return refuse(
                    "mesh compute fallback record does not hold its declared primitive maximum",
                    shaders,
                );
            };
            let vs = D3D12_SHADER_BYTECODE {
                pShaderBytecode: emulated.vertex_shader.as_ptr().cast(),
                BytecodeLength: emulated.vertex_shader.len(),
            };
            let compute_desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: unsafe { std::mem::transmute_copy(compute_root.as_ref()?) },
                CS: code(0),
                ..Default::default()
            };
            let compute_name = super::library::name(
                "mesh compute fallback v1",
                &compute_root_blob,
                &[&code_bytes(&code(0))],
                "",
            );
            let compute_pso = match super::library::pipeline(
                &compute_name,
                |library, name| unsafe { library.LoadComputePipeline(name, &compute_desc) },
                || unsafe { host.device.CreateComputePipelineState(&compute_desc) },
            ) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "mesh fallback compute pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            let draw_desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: unsafe { std::mem::transmute_copy(&root) },
                VS: vs,
                PS: code(1),
                BlendState: g.blend,
                SampleMask: g.sample_mask,
                RasterizerState: g.rasterizer,
                DepthStencilState: g.depth,
                IBStripCutValue: g.strip_cut,
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                NumRenderTargets: g.targets.NumRenderTargets,
                RTVFormats: g.targets.RTFormats,
                DSVFormat: g.depth_format,
                SampleDesc: g.samples,
                ..Default::default()
            };
            let draw_name = super::library::name(
                "mesh draw fallback v1",
                &root_blob,
                &[&emulated.vertex_shader, &code_bytes(&code(1))],
                &state,
            );
            let pso = match super::library::pipeline(
                &draw_name,
                |library, name| unsafe { library.LoadGraphicsPipeline(name, &draw_desc) },
                || unsafe { host.device.CreateGraphicsPipelineState(&draw_desc) },
            ) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "mesh fallback draw pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(
                ?shaders,
                profile = host.shader_profile.cache_key(),
                "host compute-emulated mesh pipeline"
            );
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                depth_stencil: g.uses_depth_stencil(),
                mesh_fallback: Some(MeshFallbackPipeline {
                    compute: compute_pso,
                    compute_root: compute_root?,
                    output_uav,
                    output_srv,
                    group_bytes: emulated.group_bytes,
                    max_primitives,
                }),
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                state: String::new(),
            });
        }
        if mesh.is_some() {
            // "triangles": the stream names its topology since it did not
            // load from the library without one (see `mesh_pipeline`); a
            // new name keeps such entries, stored before, from being
            // looked up again.
            let name = super::library::name(
                "mesh triangles",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            );
            let pso = match mesh_pipeline(&host.device, &root, code(0), code(1), g, &name) {
                Ok(pso) => pso,
                Err(e) => {
                    tracing::warn!(error = %e, ?shaders, "host mesh pipeline refused");
                    host.log_messages();
                    return None;
                }
            };
            tracing::info!(
                ?shaders,
                profile = host.shader_profile.cache_key(),
                "host mesh pipeline"
            );
            crate::host::name(&pso, || format!("{shaders:?}"));
            return Some(HostPipeline {
                compute: false,
                mesh: true,
                depth_stencil: g.uses_depth_stencil(),
                mesh_fallback: None,
                pso,
                root,
                stages: bound,
                name: format!("{shaders:?}"),
                slots: 0,
                inputs: Vec::new(),
                rects: false,
                state: String::new(),
            });
        }

        // A pipeline the title makes for rectangles gets a geometry stage
        // that completes each one from the three vertices the vertex
        // shader made. Without it (refused here or by the host) the draws
        // fall back on what `draw` does for rectangles.
        let mut rect_gs = None;
        if g.topology.0 == RECTANGLES {
            let ps = stages.get(1).and_then(|(s, _)| s.translated());
            let vs = &stages[0].0.translated()?.host.dxil;
            match victory_shader::emit::rect_list_gs(vs, ps.map(|t| &t.host.dxil[..])) {
                Ok(gs) => rect_gs = Some(gs),
                Err(e) => tracing::warn!(error = %e, ?shaders, "no rectangle geometry stage"),
            }
        }
        let mut desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&root) },
            VS: code(0),
            PS: code(1),
            GS: rect_gs
                .as_ref()
                .map_or(D3D12_SHADER_BYTECODE::default(), |gs| {
                    D3D12_SHADER_BYTECODE {
                        pShaderBytecode: gs.as_ptr().cast(),
                        BytecodeLength: gs.len(),
                    }
                }),
            BlendState: g.blend,
            SampleMask: g.sample_mask,
            RasterizerState: g.rasterizer,
            DepthStencilState: g.depth,
            InputLayout: D3D12_INPUT_LAYOUT_DESC {
                pInputElementDescs: g.inputs.as_ptr(),
                NumElements: g.inputs.len() as u32,
            },
            IBStripCutValue: g.strip_cut,
            // The console's rectangle and quad types draw as triangles on
            // the host (see `draw`).
            PrimitiveTopologyType: match g.topology.0 {
                RECTANGLES | QUADS => D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                _ => g.topology,
            },
            NumRenderTargets: g.targets.NumRenderTargets,
            RTVFormats: g.targets.RTFormats,
            DSVFormat: g.depth_format,
            SampleDesc: g.samples,
            ..Default::default()
        };
        // A pipeline with the geometry stage has a name of its own in the
        // library; the others keep theirs.
        let name = |gs: Option<&Vec<u8>>| match gs {
            Some(gs) => super::library::name(
                "graphics rectangles",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1)), &gs[..]],
                &state,
            ),
            None => super::library::name(
                "graphics",
                &root_blob,
                &[&code_bytes(&code(0)), &code_bytes(&code(1))],
                &state,
            ),
        };
        let make = |desc: &D3D12_GRAPHICS_PIPELINE_STATE_DESC, name: &str| {
            super::library::pipeline(
                name,
                // SAFETY: the description points at live data.
                |library, name| unsafe { library.LoadGraphicsPipeline(name, desc) },
                // SAFETY: as above.
                || unsafe { host.device.CreateGraphicsPipelineState(desc) },
            )
        };
        let mut made = make(&desc, &name(rect_gs.as_ref()));
        if made.is_err() && rect_gs.is_some() {
            // The host refused the geometry stage: once more without it.
            tracing::warn!(
                ?shaders,
                "host pipeline refused with the rectangle geometry stage"
            );
            host.log_messages();
            desc.GS = D3D12_SHADER_BYTECODE::default();
            rect_gs = None;
            made = make(&desc, &name(None));
        }
        let pso = match made {
            Ok(pso) => pso,
            Err(e) => {
                tracing::warn!(error = %e, ?shaders, "host pipeline refused");
                host.log_messages();
                return None;
            }
        };
        tracing::info!(
            ?shaders,
            profile = host.shader_profile.cache_key(),
            "host pipeline"
        );
        crate::host::name(&pso, || format!("{shaders:?}"));
        tracing::debug!(
            ?shaders,
            depth = ?g.depth,
            dsv = g.depth_format.0,
            targets = ?&g.targets.RTFormats[..g.targets.NumRenderTargets.min(8) as usize],
            blend = ?g.blend.RenderTarget[0],
            cull = g.rasterizer.CullMode.0,
            "host graphics state"
        );
        Some(HostPipeline {
            compute: false,
            mesh: false,
            depth_stencil: g.uses_depth_stencil(),
            mesh_fallback: None,
            pso,
            root,
            stages: bound,
            name: format!("{shaders:?}"),
            slots: g.slots(),
            inputs: g.elements(),
            rects: rect_gs.is_some(),
            state: {
                let (d, r, b) = (&g.depth, &g.rasterizer, &g.blend.RenderTarget[0]);
                let face = |f: &D3D12_DEPTH_STENCILOP_DESC| {
                    format!(
                        "fail {} depth-fail {} pass {} func {}",
                        f.StencilFailOp.0,
                        f.StencilDepthFailOp.0,
                        f.StencilPassOp.0,
                        f.StencilFunc.0
                    )
                };
                format!(
                    "depth {} write {} func {}; stencil {} read {:#x} write {:#x} front [{}] back [{}]; cull {} front-ccw {}; blend {} src {} dst {} op {} mask {:#x}",
                    d.DepthEnable.as_bool(),
                    d.DepthWriteMask.0,
                    d.DepthFunc.0,
                    d.StencilEnable.as_bool(),
                    d.StencilReadMask,
                    d.StencilWriteMask,
                    face(&d.FrontFace),
                    face(&d.BackFace),
                    r.CullMode.0,
                    r.FrontCounterClockwise.as_bool(),
                    b.BlendEnable.as_bool(),
                    b.SrcBlend.0,
                    b.DestBlend.0,
                    b.BlendOp.0,
                    b.RenderTargetWriteMask
                )
            },
        })
    }
}

/// A mesh pipeline from the title's graphics state: the pipeline-state
/// stream form (no input layout).
///
/// The stream names a triangle topology, which D3D12 does not need for a
/// mesh shader: without it the pipeline library refuses to load a stored
/// pipeline with the very stream it was made from (`E_INVALIDARG`; debug
/// layer message 972, "does not match the one used to create the PSO"), so
/// every mesh pipeline was compiled again, and its store then refused (970,
/// "already exists"). The translation makes triangle mesh shaders only
/// (`mesh_prologue` refuses others), and the debug layer wants the two to
/// agree (1323).
fn mesh_pipeline(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    ms: D3D12_SHADER_BYTECODE,
    ps: D3D12_SHADER_BYTECODE,
    g: &GraphicsState,
    name: &str,
) -> windows::core::Result<ID3D12PipelineState> {
    use windows::core::Interface;
    /// A stream subobject: its type, then its description, pointer-aligned.
    #[repr(C, align(8))]
    struct Sub<T>(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE, T);
    #[repr(C)]
    struct Stream {
        root: Sub<*mut std::ffi::c_void>,
        ms: Sub<D3D12_SHADER_BYTECODE>,
        ps: Sub<D3D12_SHADER_BYTECODE>,
        blend: Sub<D3D12_BLEND_DESC>,
        mask: Sub<u32>,
        raster: Sub<D3D12_RASTERIZER_DESC>,
        depth: Sub<D3D12_DEPTH_STENCIL_DESC>,
        targets: Sub<D3D12_RT_FORMAT_ARRAY>,
        dsv: Sub<DXGI_FORMAT>,
        samples: Sub<DXGI_SAMPLE_DESC>,
        topology: Sub<D3D12_PRIMITIVE_TOPOLOGY_TYPE>,
    }
    let mut stream = Stream {
        root: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_ROOT_SIGNATURE,
            root.as_raw(),
        ),
        ms: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_MS, ms),
        ps: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PS, ps),
        blend: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_BLEND, g.blend),
        mask: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_MASK,
            g.sample_mask,
        ),
        raster: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RASTERIZER, g.rasterizer),
        depth: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL, g.depth),
        targets: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_RENDER_TARGET_FORMATS,
            g.targets,
        ),
        dsv: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_DEPTH_STENCIL_FORMAT,
            g.depth_format,
        ),
        samples: Sub(D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_SAMPLE_DESC, g.samples),
        topology: Sub(
            D3D12_PIPELINE_STATE_SUBOBJECT_TYPE_PRIMITIVE_TOPOLOGY,
            D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
        ),
    };
    let desc = D3D12_PIPELINE_STATE_STREAM_DESC {
        SizeInBytes: size_of::<Stream>(),
        pPipelineStateSubobjectStream: (&raw mut stream).cast(),
    };
    let device = device.cast::<ID3D12Device2>()?;
    super::library::pipeline(
        name,
        // SAFETY: the stream holds whole subobjects pointing at live data.
        |library, name| unsafe { library.LoadPipeline(name, &desc) },
        // SAFETY: as above.
        || unsafe { device.CreatePipelineState(&desc) },
    )
}

/// The bytes of shader code (none for an absent stage).
fn code_bytes(code: &D3D12_SHADER_BYTECODE) -> Vec<u8> {
    if code.pShaderBytecode.is_null() {
        return Vec::new();
    }
    // SAFETY: a translation's own bytes, alive while its shader is.
    unsafe { std::slice::from_raw_parts(code.pShaderBytecode.cast::<u8>(), code.BytecodeLength) }
        .to_vec()
}

/// A stage's root arguments, as [`signature_layout`] placed them.
struct StageLayout {
    user_data: Option<(u32, u32)>,
    root_cbvs: Vec<(u8, u32)>,
    root_srvs: Vec<(u8, u32)>,
    root_uavs: Vec<(u8, u32)>,
    aliases: Vec<(u32, bool)>,
}

/// A root signature and where the arguments of its stages go.
struct Signature {
    root: ID3D12RootSignature,
    blob: Vec<u8>,
    stages: Vec<StageLayout>,
    output: Option<u32>,
}

/// D3D12's limit on root-signature arguments: 64 DWORDs (a root constant
/// is one DWORD each, a root descriptor two, a descriptor table one).
const ROOT_DWORDS: u32 = 64;

fn root_dwords(params: &[D3D12_ROOT_PARAMETER1]) -> u64 {
    params
        .iter()
        .map(|parameter| match parameter.ParameterType {
            // SAFETY: the union holds constants for this parameter type.
            D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS => {
                u64::from(unsafe { parameter.Anonymous.Constants.Num32BitValues })
            }
            D3D12_ROOT_PARAMETER_TYPE_CBV
            | D3D12_ROOT_PARAMETER_TYPE_SRV
            | D3D12_ROOT_PARAMETER_TYPE_UAV => 2,
            _ => 1,
        })
        .sum()
}

/// The flags of a signature: the input assembler for graphics, and the
/// direct-heap bits only for a stage that makes heap handles itself
/// (SM 6.6). An SM 6.5 stage reads the heap through descriptor-table
/// aliases; the direct-heap flags there make the host refuse the pipeline.
fn root_flags(compute: bool, direct_heap: bool) -> D3D12_ROOT_SIGNATURE_FLAGS {
    let mut flags = if compute {
        D3D12_ROOT_SIGNATURE_FLAG_NONE
    } else {
        D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT
    };
    if direct_heap {
        flags |= D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
            | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED;
    }
    flags
}

fn alias_range_type(class: DescriptorClass) -> D3D12_DESCRIPTOR_RANGE_TYPE {
    match class {
        DescriptorClass::Srv => D3D12_DESCRIPTOR_RANGE_TYPE_SRV,
        DescriptorClass::Uav => D3D12_DESCRIPTOR_RANGE_TYPE_UAV,
        DescriptorClass::Cbv => D3D12_DESCRIPTOR_RANGE_TYPE_CBV,
        DescriptorClass::Sampler => D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
    }
}

/// The root signature of the translated `stages`, and where each stage's
/// root arguments go.
///
/// SM 6.5 translations read the dynamic heap through one unbounded
/// descriptor-table alias per (class, space) they name; each alias becomes
/// a root parameter the list binds to the base of its heap
/// ([`crate::draw::State::bind`]). SM 6.6 translations make heap handles
/// themselves and get the direct-heap flags instead. Refused (`Err`) when
/// the arguments pass D3D12's 64-DWORD budget, or when the host refuses the
/// signature.
fn signature_layout(
    device: &ID3D12Device,
    selected: &[(&HostShader, D3D12_SHADER_VISIBILITY)],
    output: Option<(D3D12_ROOT_PARAMETER_TYPE, D3D12_SHADER_VISIBILITY)>,
    compute: bool,
    fallback: bool,
) -> Result<Signature, String> {
    let mut params: Vec<D3D12_ROOT_PARAMETER1> = Vec::new();
    let mut ranges: Vec<Box<D3D12_DESCRIPTOR_RANGE1>> = Vec::new();
    let mut samplers = Vec::new();
    let mut bound: Vec<StageLayout> = Vec::new();
    let mut alias_order: Vec<DescriptorAlias> = Vec::new();
    let mut stage_aliases: Vec<Vec<DescriptorAlias>> = Vec::new();
    for &(shader, visibility) in selected {
        let t = shader;
        let mut stage = StageLayout {
            user_data: None,
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            aliases: Vec::new(),
        };
        let dwords = t.user_data_dwords();
        if dwords > 0 {
            stage.user_data = Some((params.len() as u32, dwords));
            params.push(D3D12_ROOT_PARAMETER1 {
                ParameterType: D3D12_ROOT_PARAMETER_TYPE_32BIT_CONSTANTS,
                Anonymous: D3D12_ROOT_PARAMETER1_0 {
                    Constants: D3D12_ROOT_CONSTANTS {
                        ShaderRegister: 0,
                        RegisterSpace: USER_DATA_SPACE,
                        Num32BitValues: dwords,
                    },
                },
                ShaderVisibility: visibility,
            });
        }
        for (slots, kind, out) in [
            (
                &t.root_cbvs,
                D3D12_ROOT_PARAMETER_TYPE_CBV,
                &mut stage.root_cbvs,
            ),
            (
                &t.root_srvs,
                D3D12_ROOT_PARAMETER_TYPE_SRV,
                &mut stage.root_srvs,
            ),
            (
                &t.root_uavs,
                D3D12_ROOT_PARAMETER_TYPE_UAV,
                &mut stage.root_uavs,
            ),
        ] {
            for &slot in slots {
                if fallback && slot == victory_shader::emit::SLOT_MESH_OUTPUT {
                    continue;
                }
                let uav = kind == D3D12_ROOT_PARAMETER_TYPE_UAV;
                let ours = if uav {
                    slot == victory_shader::emit::SLOT_FAULTS
                } else {
                    slot == victory_shader::emit::SLOT_STRIDES
                        || slot == victory_shader::emit::SLOT_MEMORY
                };
                if (uav || kind == D3D12_ROOT_PARAMETER_TYPE_SRV) && !ours && bounded_root_srvs() {
                    ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
                        RangeType: if uav {
                            D3D12_DESCRIPTOR_RANGE_TYPE_UAV
                        } else {
                            D3D12_DESCRIPTOR_RANGE_TYPE_SRV
                        },
                        NumDescriptors: 1,
                        BaseShaderRegister: u32::from(slot),
                        RegisterSpace: ROOT_CBV_SPACE,
                        Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                            | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE,
                        OffsetInDescriptorsFromTableStart: 0,
                    }));
                    let at = ranges.len() - 1;
                    let range: *const D3D12_DESCRIPTOR_RANGE1 = &*ranges[at];
                    out.push((slot, params.len() as u32));
                    params.push(D3D12_ROOT_PARAMETER1 {
                        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
                        Anonymous: D3D12_ROOT_PARAMETER1_0 {
                            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                                NumDescriptorRanges: 1,
                                pDescriptorRanges: range,
                            },
                        },
                        ShaderVisibility: visibility,
                    });
                    continue;
                }
                out.push((slot, params.len() as u32));
                params.push(D3D12_ROOT_PARAMETER1 {
                    ParameterType: kind,
                    Anonymous: D3D12_ROOT_PARAMETER1_0 {
                        Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                            ShaderRegister: u32::from(slot),
                            RegisterSpace: ROOT_CBV_SPACE,
                            Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                        },
                    },
                    ShaderVisibility: visibility,
                });
            }
        }
        for (n, s) in t.static_samplers.iter().enumerate() {
            samplers.push(D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER(s.filter as i32),
                AddressU: D3D12_TEXTURE_ADDRESS_MODE(s.address[0] as i32),
                AddressV: D3D12_TEXTURE_ADDRESS_MODE(s.address[1] as i32),
                AddressW: D3D12_TEXTURE_ADDRESS_MODE(s.address[2] as i32),
                MipLODBias: s.mip_lod_bias,
                MaxAnisotropy: s.max_anisotropy,
                ComparisonFunc: D3D12_COMPARISON_FUNC(s.comparison as i32),
                BorderColor: D3D12_STATIC_BORDER_COLOR(s.border as i32),
                MinLOD: s.min_lod,
                MaxLOD: s.max_lod,
                ShaderRegister: n as u32,
                RegisterSpace: SAMPLER_SPACE,
                ShaderVisibility: visibility,
            });
        }
        let mut mine = Vec::new();
        for alias in &t.descriptor_aliases {
            if !alias_order.contains(alias) {
                alias_order.push(*alias);
            }
            if !mine.contains(alias) {
                mine.push(*alias);
            }
        }
        stage_aliases.push(mine);
        bound.push(stage);
    }
    let output = output.map(|(parameter, visibility)| {
        let index = params.len() as u32;
        params.push(D3D12_ROOT_PARAMETER1 {
            ParameterType: parameter,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                Descriptor: D3D12_ROOT_DESCRIPTOR1 {
                    ShaderRegister: u32::from(victory_shader::emit::SLOT_MESH_OUTPUT),
                    RegisterSpace: ROOT_CBV_SPACE,
                    Flags: D3D12_ROOT_DESCRIPTOR_FLAG_DATA_VOLATILE,
                },
            },
            ShaderVisibility: visibility,
        });
        index
    });
    let alias_base = params.len() as u32;
    for alias in &alias_order {
        let sampler = alias.class == DescriptorClass::Sampler;
        ranges.push(Box::new(D3D12_DESCRIPTOR_RANGE1 {
            RangeType: alias_range_type(alias.class),
            NumDescriptors: u32::MAX,
            BaseShaderRegister: 0,
            RegisterSpace: alias.space,
            Flags: D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
                | if sampler {
                    D3D12_DESCRIPTOR_RANGE_FLAG_NONE
                } else {
                    D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE
                },
            OffsetInDescriptorsFromTableStart: 0,
        }));
        let at = ranges.len() - 1;
        let range: *const D3D12_DESCRIPTOR_RANGE1 = &*ranges[at];
        params.push(D3D12_ROOT_PARAMETER1 {
            ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
            Anonymous: D3D12_ROOT_PARAMETER1_0 {
                DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                    NumDescriptorRanges: 1,
                    pDescriptorRanges: range,
                },
            },
            ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
        });
    }
    for (stage, mine) in bound.iter_mut().zip(&stage_aliases) {
        stage.aliases = mine
            .iter()
            .map(|alias| {
                let at = alias_order
                    .iter()
                    .position(|other| other == alias)
                    .expect("the alias was collected");
                (
                    alias_base + at as u32,
                    alias.class == DescriptorClass::Sampler,
                )
            })
            .collect();
    }
    let dwords = root_dwords(&params);
    if dwords > u64::from(ROOT_DWORDS) {
        return Err(format!(
            "the root signature needs {dwords} DWORDs; D3D12 allows {ROOT_DWORDS}"
        ));
    }
    let direct_heap = selected.iter().any(|(shader, _)| shader.direct_heap);
    let (root, blob) = root_signature(device, &params, &samplers, compute, direct_heap)
        .map_err(|e| e.to_string())?;
    Ok(Signature {
        root,
        blob,
        stages: bound,
        output,
    })
}

fn root_signature(
    device: &ID3D12Device,
    params: &[D3D12_ROOT_PARAMETER1],
    samplers: &[D3D12_STATIC_SAMPLER_DESC],
    compute: bool,
    direct_heap: bool,
) -> windows::core::Result<(ID3D12RootSignature, Vec<u8>)> {
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: windows::Win32::Graphics::Direct3D12::D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: samplers.len() as u32,
                pStaticSamplers: samplers.as_ptr(),
                Flags: root_flags(compute, direct_heap),
            },
        },
    };
    // SAFETY: the description points at live arrays; the blob is read in
    // full before it is released.
    unsafe {
        let mut blob: Option<ID3DBlob> = None;
        let mut error: Option<ID3DBlob> = None;
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            if let Some(error) = error {
                let text = std::slice::from_raw_parts(
                    error.GetBufferPointer().cast::<u8>(),
                    error.GetBufferSize(),
                );
                tracing::warn!("root signature: {}", String::from_utf8_lossy(text));
            }
            return Err(e);
        }
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        Ok((device.CreateRootSignature(0, bytes)?, bytes.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn depth_stencil_binding_follows_both_enable_flags() {
        let mut g = GraphicsState::default();
        for (depth, stencil, needed) in [
            (false, false, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            g.depth.DepthEnable = depth.into();
            g.depth.StencilEnable = stencil.into();
            assert_eq!(g.uses_depth_stencil(), needed);
        }
    }

    #[test]
    fn mesh_fallback_primitives_fit_the_group_record() {
        assert_eq!(mesh_fallback_max_primitives(0, 0, 0, 0, 16), Some(0));
        assert_eq!(mesh_fallback_max_primitives(1, 3, 20, 16, 112), Some(1));
        assert_eq!(mesh_fallback_max_primitives(3, 3, 20, 16, 112), None);
        assert_eq!(
            mesh_fallback_max_primitives(256, 0, 0, 0, 16 + 12 * 256 + 16),
            Some(256)
        );
        assert_eq!(
            mesh_fallback_max_primitives(4, 0, 0, 0, 16 + 12 * 4 - 1),
            None
        );
        assert_eq!(mesh_fallback_max_primitives(257, 0, 0, 0, u32::MAX), None);
        assert_eq!(
            mesh_fallback_max_primitives(u32::MAX, 0, 0, 0, u32::MAX),
            None
        );
    }

    /// A triangle mesh shader of our own and a pixel shader for it.
    const MS: &str = "struct V { float4 p : SV_Position; };
        [outputtopology(\"triangle\")] [numthreads(1, 1, 1)]
        void main(out vertices V v[3], out indices uint3 t[1]) {
            SetMeshOutputCounts(3, 1);
            v[0].p = float4(0, 0, 0, 1);
            v[1].p = float4(1, 0, 0, 1);
            v[2].p = float4(0, 1, 0, 1);
            t[0] = uint3(0, 1, 2);
        }";
    const PS: &str = "float4 main() : SV_Target { return 1; }";

    /// The PC `dxc.exe` (`VICTORY_PC_DXC`, else the newest Windows SDK's).
    fn dxc() -> Option<PathBuf> {
        let dxc = match std::env::var_os("VICTORY_PC_DXC") {
            Some(p) => PathBuf::from(p),
            None => victory_shader::emit::validator::Validator::default_path()?
                .with_file_name("dxc.exe"),
        };
        dxc.exists().then_some(dxc)
    }

    fn compile(dxc: &Path, dir: &Path, src: &str, profile: &str) -> Vec<u8> {
        let (hlsl, out) = (
            dir.join(format!("{profile}.hlsl")),
            dir.join(format!("{profile}.dxil")),
        );
        std::fs::write(&hlsl, src).unwrap();
        let status = std::process::Command::new(dxc)
            .args(["-nologo", "-T", profile, "-E", "main", "-Fo"])
            .arg(&out)
            .arg(&hlsl)
            .status()
            .unwrap();
        assert!(status.success(), "dxc failed for {profile}");
        std::fs::read(out).unwrap()
    }

    fn code(bytes: &[u8]) -> D3D12_SHADER_BYTECODE {
        D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        }
    }

    /// A mesh pipeline made once is loaded from the library the next time,
    /// not compiled again (a stream without a topology never loads).
    #[test]
    fn a_mesh_pipeline_loads_from_the_library() {
        let Some(host) = host() else {
            return; // no host GPU here
        };
        if !host.mesh_shaders() {
            eprintln!("no host mesh shader support; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let ms = compile(&dxc, dir.path(), MS, "ms_6_5");
        let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
        let (root, root_blob) = root_signature(&host.device, &[], &[], false, false).unwrap();
        let mut g = GraphicsState::default();
        // This pipeline has only a color target, with no depth buffer.
        g.depth.DepthEnable = false.into();
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        let name = super::super::library::name("mesh test", &root_blob, &[&ms, &ps], &g.describe());
        mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, &name).expect("made");
        let loaded = super::super::library::LOADED.load(Ordering::Relaxed);
        mesh_pipeline(&host.device, &root, code(&ms), code(&ps), &g, &name).expect("made again");
        assert!(
            super::super::library::LOADED.load(Ordering::Relaxed) > loaded,
            "the mesh pipeline was compiled again instead of loaded from the library"
        );
    }

    use std::sync::OnceLock;

    use victory_shader::HwStage;
    use victory_shader::RootType;
    use victory_shader::emit::{MeshFallback, ShaderProfile};

    /// The adapter's highest shader model (`D3D_SHADER_MODEL`).
    fn highest_shader_model(device: &ID3D12Device) -> u32 {
        let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
            HighestShaderModel: D3D_SHADER_MODEL_6_6,
        };
        // SAFETY: a plain feature query writing its own structure.
        let queried = unsafe {
            device.CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&raw mut model).cast(),
                size_of_val(&model) as u32,
            )
        };
        queried.expect("shader model query");
        model.HighestShaderModel.0 as u32
    }

    /// The software adapter, when it runs SM 6.6: the SM 6.6 tests run
    /// there on cards that stop at 6.5.
    fn sm66_software_device() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        use windows::Win32::Graphics::Dxgi::{
            CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, IDXGIAdapter, IDXGIFactory4,
        };
        // SAFETY: plain DXGI and D3D12 object creation.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            let device = device?;
            (highest_shader_model(&device) >= 0x66).then_some(device)
        }
    }

    fn host_shader(
        stage: HwStage,
        profile: ShaderProfile,
        dxil: Vec<u8>,
        aliases: &[(DescriptorClass, u32)],
        direct_heap: bool,
    ) -> HostShader {
        HostShader {
            stage,
            profile,
            dxil,
            descriptor_aliases: aliases
                .iter()
                .map(|&(class, space)| DescriptorAlias { class, space })
                .collect(),
            direct_heap,
            mesh_fallback: None,
            user_data: Vec::new(),
            samplers: Vec::new(),
            static_samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            root_uavs: Vec::new(),
            root_indexed_views: Vec::new(),
        }
    }

    /// A translated shader for a host pipeline, without a console
    /// container: the translation is given.
    fn shader_with(stage: &'static str, host: HostShader) -> Arc<Shader> {
        let shader = Shader {
            stage,
            hash: "0000000000000000".to_owned(),
            container: Vec::new(),
            translated: OnceLock::new(),
        };
        assert!(
            shader
                .translated
                .set(Some(Translated {
                    host,
                    used: Vec::new(),
                    stores: false,
                }))
                .is_ok(),
            "the translation was already set"
        );
        Arc::new(shader)
    }

    fn graphics_state() -> GraphicsState {
        let mut g = GraphicsState::default();
        g.depth.DepthEnable = false.into();
        g.targets.NumRenderTargets = 1;
        g.targets.RTFormats[0] = DXGI_FORMAT_R8G8B8A8_UNORM;
        g.topology = D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE;
        g
    }

    /// A buffer of `size` bytes in a heap of the given type.
    fn buffer(
        device: &ID3D12Device,
        heap: D3D12_HEAP_TYPE,
        size: u64,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut made: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a buffer; nothing is mapped yet.
        unsafe {
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: heap,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut made,
                )
                .expect("buffer")
        };
        made.expect("buffer")
    }

    const VS65: &str = "struct V { float4 p : SV_Position; };
        V main(uint id : SV_VertexID) {
            V v;
            v.p = float4(0, 0, 0, 1);
            return v;
        }";
    const VS65_PULL: &str = "struct V { float4 p : SV_Position; };
        StructuredBuffer<float4> positions : register(t0, space2);
        V main(uint id : SV_VertexID) {
            V v;
            v.p = positions[id];
            return v;
        }";
    const PS65_ALIASED: &str = "Texture2D<float4> tex : register(t0, space2);
        SamplerState samp : register(s0, space3);
        float4 main() : SV_Target { return tex.SampleLevel(samp, float2(1, 1), 0); }";
    const CS65_ALIASED: &str = "StructuredBuffer<uint> input : register(t0, space2);
        RWStructuredBuffer<uint> output : register(u1, space3);
        [numthreads(1, 1, 1)]
        void main(uint3 id : SV_DispatchThreadID) { output[0] = input[0] + 1; }";
    const CS66_DIRECT: &str = "[numthreads(1, 1, 1)]
        void main(uint3 id : SV_DispatchThreadID) {
            RWTexture2D<float4> target = ResourceDescriptorHeap[1];
            target[id.xy] = float4(1, 1, 1, 1);
        }";

    #[test]
    fn direct_heap_flags_come_only_from_an_sm66_stage() {
        let graphics = root_flags(false, false);
        assert_ne!(
            graphics.0 & D3D12_ROOT_SIGNATURE_FLAG_ALLOW_INPUT_ASSEMBLER_INPUT_LAYOUT.0,
            0
        );
        assert_eq!(
            graphics.0 & D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED.0,
            0
        );
        assert_eq!(
            graphics.0 & D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED.0,
            0
        );
        assert_eq!(root_flags(true, false).0, 0);
        let direct = root_flags(true, true);
        assert_ne!(
            direct.0 & D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED.0,
            0
        );
        assert_ne!(
            direct.0 & D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED.0,
            0
        );
    }

    /// One alias shared by two stages is one root parameter; a stage's
    /// other alias gets its own.
    #[test]
    fn aliases_are_shared_across_stages() {
        let Some(host) = host() else {
            return;
        };
        let vs = host_shader(
            HwStage::Vs,
            ShaderProfile::Sm65,
            Vec::new(),
            &[(DescriptorClass::Srv, 2)],
            false,
        );
        let ps = host_shader(
            HwStage::Ps,
            ShaderProfile::Sm65,
            Vec::new(),
            &[(DescriptorClass::Srv, 2), (DescriptorClass::Sampler, 3)],
            false,
        );
        let signature = signature_layout(
            &host.device,
            &[
                (&vs, D3D12_SHADER_VISIBILITY_VERTEX),
                (&ps, D3D12_SHADER_VISIBILITY_PIXEL),
            ],
            None,
            false,
            false,
        )
        .expect("SM 6.5 root signature");
        assert_eq!(signature.stages[0].aliases, [(0, false)]);
        assert_eq!(
            signature.stages[1].aliases,
            [(0, false), (1, true)],
            "the shared SRV alias then the sampler"
        );
    }

    #[test]
    fn a_signature_over_the_root_dword_budget_is_refused() {
        let Some(host) = host() else {
            return;
        };
        let mut shader = host_shader(HwStage::Cs, ShaderProfile::Sm65, Vec::new(), &[], false);
        // 61 dwords rounded to 64, then one root descriptor: 66.
        shader.user_data.push(victory_shader::emit::UserData {
            index: 60,
            slot: 0,
            root: RootType::Constants,
            dword: 0,
        });
        shader.root_cbvs.push(0);
        let error = signature_layout(
            &host.device,
            &[(&shader, D3D12_SHADER_VISIBILITY_ALL)],
            None,
            true,
            false,
        )
        .err()
        .expect("a signature over the budget was accepted");
        assert!(error.contains("66"), "{error}");
        assert!(error.contains("64"), "{error}");
    }

    /// SM 6.5: the dynamic-heap aliases become descriptor tables in the
    /// root signature (no direct-heap flags), and the graphics pipeline
    /// built from them passes the host.
    #[test]
    fn sm65_graphics_pipeline_binds_descriptor_aliases() {
        let Some(host) = host() else {
            return;
        };
        if highest_shader_model(&host.device) < 0x65 {
            eprintln!("no SM 6.5 device; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let vs = compile(&dxc, dir.path(), VS65_PULL, "vs_6_5");
        let ps = compile(&dxc, dir.path(), PS65_ALIASED, "ps_6_5");
        let vs = shader_with(
            "vertex",
            host_shader(
                HwStage::Vs,
                ShaderProfile::Sm65,
                vs,
                &[(DescriptorClass::Srv, 2)],
                false,
            ),
        );
        let ps = shader_with(
            "pixel",
            host_shader(
                HwStage::Ps,
                ShaderProfile::Sm65,
                ps,
                &[(DescriptorClass::Srv, 2), (DescriptorClass::Sampler, 3)],
                false,
            ),
        );
        let g = graphics_state();
        let p = HostPipeline::new(&[vs, ps], Some(&g)).expect("SM 6.5 graphics pipeline");
        assert_eq!(
            p.stages[0].aliases,
            [(0, false)],
            "the vertex stage's SRV alias"
        );
        assert_eq!(
            p.stages[1].aliases,
            [(0, false), (1, true)],
            "the shared SRV alias then the sampler"
        );
    }

    /// SM 6.5: a dispatch through two aliases bound at the heap's base
    /// reads descriptor 0 and writes descriptor 1.
    #[test]
    fn sm65_compute_dispatch_binds_descriptor_aliases() {
        use windows::core::Interface;
        let Some(host) = host() else {
            return;
        };
        if highest_shader_model(&host.device) < 0x65 {
            eprintln!("no SM 6.5 device; skipped");
            return;
        }
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cs = compile(&dxc, dir.path(), CS65_ALIASED, "cs_6_5");
        let cs = shader_with(
            "compute",
            host_shader(
                HwStage::Cs,
                ShaderProfile::Sm65,
                cs,
                &[(DescriptorClass::Srv, 2), (DescriptorClass::Uav, 3)],
                false,
            ),
        );
        let p = HostPipeline::new(&[cs], None).expect("SM 6.5 compute pipeline");
        assert_eq!(p.stages[0].aliases, [(0, false), (1, false)]);
        // SAFETY: plain D3D12 objects; every description points at live
        // data and every resource lives until the fence says the card is
        // done.
        unsafe {
            let device = &host.device;
            let input = host.uav_buffer(4).expect("input buffer");
            let output = host.uav_buffer(4).expect("output buffer");
            let upload = buffer(
                device,
                D3D12_HEAP_TYPE_UPLOAD,
                4,
                D3D12_RESOURCE_STATE_GENERIC_READ,
            );
            let readback = buffer(
                device,
                D3D12_HEAP_TYPE_READBACK,
                4,
                D3D12_RESOURCE_STATE_COPY_DEST,
            );
            let mut mapped = std::ptr::null_mut();
            upload.Map(0, None, Some(&mut mapped)).expect("map input");
            mapped.cast::<u32>().write(41);
            upload.Unmap(0, None);
            let heap: ID3D12DescriptorHeap = device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    ..Default::default()
                })
                .expect("view heap");
            let step = device
                .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
                as usize;
            let start = heap.GetCPUDescriptorHandleForHeapStart();
            device.CreateShaderResourceView(
                &input,
                Some(&D3D12_SHADER_RESOURCE_VIEW_DESC {
                    Format: DXGI_FORMAT_UNKNOWN,
                    ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                    Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                    Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_SRV {
                            FirstElement: 0,
                            NumElements: 1,
                            StructureByteStride: 4,
                            ..Default::default()
                        },
                    },
                }),
                start,
            );
            let mut at = start;
            at.ptr += step;
            device.CreateUnorderedAccessView(
                &output,
                None,
                Some(&D3D12_UNORDERED_ACCESS_VIEW_DESC {
                    Format: DXGI_FORMAT_UNKNOWN,
                    ViewDimension: D3D12_UAV_DIMENSION_BUFFER,
                    Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                        Buffer: D3D12_BUFFER_UAV {
                            FirstElement: 0,
                            NumElements: 1,
                            StructureByteStride: 4,
                            ..Default::default()
                        },
                    },
                }),
                at,
            );
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .expect("queue");
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .expect("allocator");
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, Some(&p.pso))
                .expect("list");
            list.ResourceBarrier(&[
                crate::host::transition(
                    &input,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ),
                crate::host::transition(
                    &output,
                    D3D12_RESOURCE_STATE_COMMON,
                    D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                ),
            ]);
            list.CopyBufferRegion(&input, 0, &upload, 0, 4);
            list.ResourceBarrier(&[crate::host::transition(
                &input,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
            )]);
            list.SetDescriptorHeaps(&[Some(heap.clone())]);
            list.SetComputeRootSignature(&p.root);
            let base = heap.GetGPUDescriptorHandleForHeapStart();
            for &(param, _) in &p.stages[0].aliases {
                // The same heap, from its first descriptor: alias
                // register 0 is heap slot 0 and register 1 is slot 1.
                list.SetComputeRootDescriptorTable(param, base);
            }
            list.Dispatch(1, 1, 1);
            list.ResourceBarrier(&[crate::host::transition(
                &output,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            list.CopyBufferRegion(&readback, 0, &output, 0, 4);
            list.Close().expect("close");
            queue.ExecuteCommandLists(&[Some(
                list.cast::<ID3D12CommandList>().expect("command list"),
            )]);
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).expect("fence");
            queue.Signal(&fence, 1).expect("signal");
            while fence.GetCompletedValue() < 1 {
                if let Err(e) = device.GetDeviceRemovedReason() {
                    panic!("device removed: {e}");
                }
                std::thread::yield_now();
            }
            device.GetDeviceRemovedReason().expect("device removed");
            let mut mapped = std::ptr::null_mut();
            readback
                .Map(0, None, Some(&mut mapped))
                .expect("map output");
            let value = mapped.cast::<u32>().read();
            readback.Unmap(0, None);
            assert_eq!(value, 42, "the dispatch read alias slot 0 and wrote slot 1");
        }
    }

    /// SM 6.6: the direct-heap flags are added, without alias tables.
    /// Runs on the card when it reaches SM 6.6, else on the software
    /// adapter when that one does.
    #[test]
    fn sm66_compute_pipeline_uses_direct_heaps() {
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cs = compile(&dxc, dir.path(), CS66_DIRECT, "cs_6_6");
        let cs = host_shader(HwStage::Cs, ShaderProfile::Sm66, cs, &[], true);
        if host().is_some_and(|host| highest_shader_model(&host.device) >= 0x66) {
            let shader = shader_with("compute", cs);
            let p = HostPipeline::new(&[shader], None).expect("SM 6.6 compute pipeline");
            assert!(p.stages[0].aliases.is_empty());
            return;
        }
        let Some(device) = sm66_software_device() else {
            eprintln!("no SM 6.6 device or software adapter; skipped");
            return;
        };
        let signature = signature_layout(
            &device,
            &[(&cs, D3D12_SHADER_VISIBILITY_ALL)],
            None,
            true,
            false,
        )
        .expect("SM 6.6 root signature");
        assert!(signature.stages[0].aliases.is_empty());
        let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
            // SAFETY: a borrowed pointer the description never releases.
            pRootSignature: unsafe { std::mem::transmute_copy(&signature.root) },
            CS: code(&cs.dxil),
            ..Default::default()
        };
        // SAFETY: the description points at live data.
        let _pso: ID3D12PipelineState =
            unsafe { device.CreateComputePipelineState(&desc) }.expect("SM 6.6 compute pipeline");
    }

    /// The compute-emulated mesh path builds its dispatch and draw
    /// pipelines from the SM 6.5 signatures (the dispatch's output table and
    /// the draw's vertex stage are extra root parameters).
    #[test]
    fn sm65_mesh_fallback_builds_both_pipelines() {
        if !crate::mesh_fallback::enabled() {
            eprintln!("hardware mesh shaders are used; skipped");
            return;
        }
        let Some(host) = host() else {
            return;
        };
        if highest_shader_model(&host.device) < 0x65 {
            eprintln!("no SM 6.5 device; skipped");
            return;
        }
        eprintln!(
            "mesh fallback pipeline: HighestShaderModel={:#x} selected={}",
            highest_shader_model(&host.device),
            host.shader_profile.cache_key()
        );
        let Some(dxc) = dxc() else {
            eprintln!("no PC dxc.exe; skipped");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let cs = compile(&dxc, dir.path(), CS65_ALIASED, "cs_6_5");
        let vs = compile(&dxc, dir.path(), VS65, "vs_6_5");
        let ps = compile(&dxc, dir.path(), PS, "ps_6_5");
        let mut mesh = host_shader(
            HwStage::Ms,
            ShaderProfile::Sm65,
            cs,
            &[(DescriptorClass::Srv, 2), (DescriptorClass::Uav, 3)],
            false,
        );
        mesh.mesh_fallback = Some(MeshFallback {
            vertex_shader: vs,
            group_bytes: 256,
            vertex_stride: 12,
            primitive_stride: 12,
            max_vertices: 3,
            max_primitives: 1,
        });
        let mesh = shader_with("mesh", mesh);
        let ps = shader_with(
            "pixel",
            host_shader(HwStage::Ps, ShaderProfile::Sm65, ps, &[], false),
        );
        let g = graphics_state();
        let p = HostPipeline::new(&[mesh, ps], Some(&g)).expect("mesh fallback pipeline");
        assert!(p.mesh);
        let fallback = p.mesh_fallback.as_ref().expect("compute fallback");
        assert_eq!(fallback.max_primitives, 1);
        assert_eq!(fallback.group_bytes, 256);
        // The mesh stage's aliases belong to the compute signature, after
        // its output UAV; the pixel stage has none.
        assert_eq!(p.stages[0].aliases, [(1, false), (2, false)]);
        assert!(p.stages[1].aliases.is_empty());
    }
}
