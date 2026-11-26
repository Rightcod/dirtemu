//! Mesh groups as compute work plus a vertex-pulling graphics pass.
//!
//! A group has a fixed-size record in the output buffer: draw arguments,
//! three vertex indices per primitive, per-vertex dwords and per-primitive
//! dwords. The compute pass fills it; the vertex shader reads the index for
//! its corner and forwards the stored outputs under their original semantics.

use super::bitcode::{Attr, Bin, Cast, Term, V};
use super::container::SigElement;
use super::shader::{SLOT_MESH_OUTPUT, Shader};
use crate::native::MeshIo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeshFallback {
    pub vertex_shader: Vec<u8>,
    pub group_bytes: u32,
    pub vertex_stride: u32,
    pub primitive_stride: u32,
    pub max_vertices: u32,
    pub max_primitives: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub vertex_offsets: Vec<u32>,
    pub primitive_offsets: Vec<u32>,
    pub vertex_base: u32,
    pub primitive_base: u32,
    pub group_bytes: u32,
    pub vertex_stride: u32,
    pub primitive_stride: u32,
    pub max_vertices: u32,
    pub max_primitives: u32,
}

impl Layout {
    pub fn new(
        outputs: &[SigElement],
        prims: &[SigElement],
        mesh: &MeshIo,
    ) -> Result<Self, &'static str> {
        if mesh.topology != 2 {
            return Err("a mesh shader not drawing triangles");
        }
        if mesh.threads == 0
            || mesh.threads > 128
            || mesh.max_vertices > 256
            || mesh.max_primitives > 256
        {
            return Err("a mesh shader beyond the compute fallback's limits");
        }
        let stride = |elements: &[SigElement]| {
            elements
                .iter()
                .try_fold(0u32, |bytes, e| bytes.checked_add(4 * u32::from(e.cols)))
        };
        let vertex_stride = stride(outputs).ok_or("mesh vertex outputs too large")?;
        let primitive_stride = stride(prims).ok_or("mesh primitive outputs too large")?;
        let offsets = |elements: &[SigElement]| {
            let mut next = 0;
            elements
                .iter()
                .map(|e| {
                    let at = next;
                    next += 4 * u32::from(e.cols);
                    at
                })
                .collect::<Vec<_>>()
        };
        let vertex_base = 16u32
            .checked_add(12 * mesh.max_primitives)
            .and_then(|n| n.checked_add(15))
            .map(|n| n & !15)
            .ok_or("mesh output layout too large")?;
        let primitive_base = vertex_base
            .checked_add(
                vertex_stride
                    .checked_mul(mesh.max_vertices)
                    .ok_or("mesh output layout too large")?,
            )
            .ok_or("mesh output layout too large")?;
        let group_bytes = primitive_base
            .checked_add(
                primitive_stride
                    .checked_mul(mesh.max_primitives)
                    .ok_or("mesh output layout too large")?,
            )
            .and_then(|n| n.checked_add(15))
            .map(|n| n & !15)
            .ok_or("mesh output layout too large")?;
        Ok(Layout {
            vertex_offsets: offsets(outputs),
            primitive_offsets: offsets(prims),
            vertex_base,
            primitive_base,
            group_bytes,
            vertex_stride,
            primitive_stride,
            max_vertices: mesh.max_vertices,
            max_primitives: mesh.max_primitives,
        })
    }

    pub fn fallback(&self, vertex_shader: Vec<u8>) -> MeshFallback {
        MeshFallback {
            vertex_shader,
            group_bytes: self.group_bytes,
            vertex_stride: self.vertex_stride,
            primitive_stride: self.primitive_stride,
            max_vertices: self.max_vertices,
            max_primitives: self.max_primitives,
        }
    }
}

fn system_input(name: &str, kind: u8, row: u32) -> SigElement {
    SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 5,
        interp: 0,
        row: Some(row),
        col: 0,
        cols: 1,
        usage: 1,
        half: false,
    }
}

fn load_input(s: &mut Shader, id: usize) -> V {
    let b = s.entry;
    let args = [
        s.m.i32(id as u32),
        s.m.i32(0),
        s.m.i8(0),
        s.m.undef(s.t.i32),
    ];
    s.op(b, "loadInput.i32", Attr::ReadNone, s.t.i32, 4, &args)
}

fn raw_load(s: &mut Shader, handle: V, address: V) -> V {
    let b = s.entry;
    let (index, mask, align) = (s.m.undef(s.t.i32), s.m.i8(1), s.m.i32(4));
    let r = s.op(
        b,
        "rawBufferLoad.f32",
        Attr::ReadOnly,
        s.t.res_ret,
        139,
        &[handle, address, index, mask, align],
    );
    let f = s.m.extract(b, r, 0);
    s.m.cast(b, Cast::Bitcast, f, s.t.i32)
}

fn store_output(s: &mut Shader, id: usize, col: u8, comp: u8, value: V) {
    let b = s.entry;
    s.outputs[id].usage |= 1 << col;
    let value = if comp == 9 {
        s.m.cast(b, Cast::Bitcast, value, s.t.f32)
    } else {
        value
    };
    let suffix = if comp == 9 { ".f32" } else { ".i32" };
    let args = [s.m.i32(id as u32), s.m.i32(0), s.m.i8(col), value];
    s.op(
        b,
        &format!("storeOutput{suffix}"),
        Attr::NoUnwind,
        s.t.void,
        5,
        &args,
    );
}

pub(crate) fn vertex_shader(
    layout: &Layout,
    outputs: &[SigElement],
    prims: &[SigElement],
    profile: super::ShaderProfile,
) -> Result<Vec<u8>, &'static str> {
    if outputs
        .len()
        .checked_add(prims.len())
        .is_none_or(|n| n > 32)
    {
        return Err("mesh outputs beyond a vertex shader's 32 rows");
    }
    let mut seen = std::collections::HashSet::new();
    let mut occupied = [0u8; 32];
    for e in outputs.iter().chain(prims) {
        if !seen.insert((e.kind, e.name.clone(), e.index)) {
            return Err("a vertex and primitive output with the same semantic");
        }
        if e.cols == 0 || u32::from(e.col) + u32::from(e.cols) > 4 {
            return Err("a mesh output outside its register components");
        }
        if let Some(row) = e.row {
            let used = occupied
                .get_mut(row as usize)
                .ok_or("mesh outputs beyond a vertex shader's 32 rows")?;
            let mask = ((1u8 << e.cols) - 1) << e.col;
            if *used & mask != 0 {
                return Err("overlapping mesh outputs in the vertex shader signature");
            }
            *used |= mask;
        }
    }
    let mut s = Shader::new(false, profile);
    s.inputs = vec![
        system_input("SV_VertexID", 1, 0),
        system_input("SV_InstanceID", 2, 1),
    ];
    s.outputs = outputs.iter().chain(prims).cloned().collect();
    for e in &mut s.outputs {
        e.usage = 0;
    }
    let handle = s.root_srv_handle(SLOT_MESH_OUTPUT);
    let vertex_id = load_input(&mut s, 0);
    let group = load_input(&mut s, 1);
    let b = s.entry;
    let group_bytes = s.m.i32(layout.group_bytes);
    let group_base = s.m.bin(b, Bin::Mul, group, group_bytes);
    let three = s.m.i32(3);
    let primitive = s.m.bin(b, Bin::UDiv, vertex_id, three);
    let corner = s.m.bin(b, Bin::URem, vertex_id, three);
    let triangle = s.m.bin(b, Bin::Mul, primitive, three);
    let index = s.m.bin(b, Bin::Add, triangle, corner);
    let index_bytes = s.m.i32(4);
    let index_at = s.m.bin(b, Bin::Mul, index, index_bytes);
    let index_at = s.m.bin(b, Bin::Add, index_at, group_base);
    let sixteen = s.m.i32(16);
    let index_at = s.m.bin(b, Bin::Add, index_at, sixteen);
    let vertex = raw_load(&mut s, handle, index_at);
    let vertex_stride = s.m.i32(layout.vertex_stride);
    let vertex_at = s.m.bin(b, Bin::Mul, vertex, vertex_stride);
    let vertex_base = s.m.i32(layout.vertex_base);
    let vertex_at = s.m.bin(b, Bin::Add, vertex_at, vertex_base);
    let vertex_at = s.m.bin(b, Bin::Add, vertex_at, group_base);
    for (id, e) in outputs.iter().enumerate() {
        for col in 0..e.cols {
            let offset = layout.vertex_offsets[id] + 4 * u32::from(col);
            let offset = s.m.i32(offset);
            let at = s.m.bin(b, Bin::Add, vertex_at, offset);
            let value = raw_load(&mut s, handle, at);
            store_output(&mut s, id, col, e.comp, value);
        }
    }
    let primitive_stride = s.m.i32(layout.primitive_stride);
    let primitive_at = s.m.bin(b, Bin::Mul, primitive, primitive_stride);
    let primitive_base = s.m.i32(layout.primitive_base);
    let primitive_at = s.m.bin(b, Bin::Add, primitive_at, primitive_base);
    let primitive_at = s.m.bin(b, Bin::Add, primitive_at, group_base);
    for (n, e) in prims.iter().enumerate() {
        let id = outputs.len() + n;
        for col in 0..e.cols {
            let offset = layout.primitive_offsets[n] + 4 * u32::from(col);
            let offset = s.m.i32(offset);
            let at = s.m.bin(b, Bin::Add, primitive_at, offset);
            let value = raw_load(&mut s, handle, at);
            store_output(&mut s, id, col, e.comp, value);
        }
    }
    s.m.terminate(s.entry, Term::Ret);
    Ok(s.finish())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::{Container, FourCC};
    use crate::native::{HwStage, NativeProgram, StageIo, Usage, VsOutput};
    use crate::{ShaderKind, io, ir, resource, structure};
    use std::collections::HashMap;

    fn element(name: &str, kind: u8, row: u32, cols: u8) -> SigElement {
        SigElement {
            name: name.into(),
            index: 0,
            kind,
            comp: 9,
            interp: 0,
            row: Some(row),
            col: 0,
            cols,
            usage: 0,
            half: false,
        }
    }

    fn translated(
        mode: super::super::MeshTranslation,
    ) -> Result<super::super::HostShader, crate::ShaderError> {
        let words = [
            0xBEFC_03FFu32,
            0x0000_1003,
            0xBF90_0009,
            0x7E00_0280,
            0x7E02_0280,
            0x7E04_0280,
            0x7E06_02F2,
            0xF800_18CF,
            0x0302_0100,
            0x7E08_02FF,
            0x0020_0400,
            0xF800_1201,
            0x0000_0004,
            0xF800_1221,
            0x0000_0004,
            0xF800_0941,
            0x0000_0004,
            0xBF81_0000,
        ];
        let outputs = vec![
            element("SV_Position", 3, 0, 4),
            element("TEXCOORD", 0, 1, 1),
        ];
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: true,
            vgprs: 5,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(MeshIo {
                outputs: vec![
                    VsOutput {
                        usage: Usage::Generic,
                        row: 1,
                        param: 0,
                        fp16: false,
                        pack2: false,
                    },
                    VsOutput {
                        usage: Usage::Generic,
                        row: 0,
                        param: 0x82,
                        fp16: false,
                        pack2: false,
                    },
                ],
                clip_cntl: 0,
                threads: 32,
                max_vertices: 3,
                max_primitives: 1,
                topology: 2,
                lds_bytes: 0,
            }),
            code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
        };
        let signature = super::super::container::signature(&outputs, false);
        let primitive = super::super::container::signature(&[element("PRIMDATA", 0, 2, 4)], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &signature), (FourCC::PSG1, &primitive)]);
        let container = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.code, native.wave64, native.vgprs)?;
        let st = structure::structure(&p)?;
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native)?;
        super::super::emit_mode(&container, &native, &p, &st, &io, &res, mode)
    }

    /// the same mesh program lowered for an explicitly chosen profile, so
    /// the compute stage and the vertex-pulling stage can be inspected
    /// without the process-wide profile.
    fn lowered_with_profile(
        mode: super::super::MeshTranslation,
        profile: super::super::ShaderProfile,
    ) -> Result<super::super::lower::Lowered, crate::ShaderError> {
        let words = [
            0xBEFC_03FFu32,
            0x0000_1003,
            0xBF90_0009,
            0x7E00_0280,
            0x7E02_0280,
            0x7E04_0280,
            0x7E06_02F2,
            0xF800_18CF,
            0x0302_0100,
            0x7E08_02FF,
            0x0020_0400,
            0xF800_1201,
            0x0000_0004,
            0xF800_1221,
            0x0000_0004,
            0xF800_0941,
            0x0000_0004,
            0xBF81_0000,
        ];
        let outputs = vec![
            element("SV_Position", 3, 0, 4),
            element("TEXCOORD", 0, 1, 1),
        ];
        let native = NativeProgram {
            stage: HwStage::Ms,
            wave64: true,
            vgprs: 5,
            layout: vec![],
            used: vec![],
            io: StageIo::Ms(MeshIo {
                outputs: vec![
                    VsOutput {
                        usage: Usage::Generic,
                        row: 1,
                        param: 0,
                        fp16: false,
                        pack2: false,
                    },
                    VsOutput {
                        usage: Usage::Generic,
                        row: 0,
                        param: 0x82,
                        fp16: false,
                        pack2: false,
                    },
                ],
                clip_cntl: 0,
                threads: 32,
                max_vertices: 3,
                max_primitives: 1,
                topology: 2,
                lds_bytes: 0,
            }),
            code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
        };
        let signature = super::super::container::signature(&outputs, false);
        let primitive = super::super::container::signature(&[element("PRIMDATA", 0, 2, 4)], false);
        let bytes = crate::dxbc::build(&[(FourCC::OSG1, &signature), (FourCC::PSG1, &primitive)]);
        let container = Container::parse(&bytes).unwrap();
        let p = ir::build(&native.code, native.wave64, native.vgprs)?;
        let st = structure::structure(&p)?;
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, &native)?;
        let sig = |fourcc| -> Result<Vec<super::super::container::SigElement>, crate::ShaderError> {
            match container.part(fourcc) {
                Some(part) => super::super::signature(part.data),
                None => Ok(Vec::new()),
            }
        };
        let mut outputs = sig(FourCC::OSG1)?;
        for e in &mut outputs {
            e.interp = match (e.kind, e.comp) {
                (3, _) => 4,
                (0 | 6 | 7, 9) => 2,
                (0, _) => 1,
                _ => 0,
            };
        }
        let mut prims = sig(FourCC::PSG1)?;
        prims.retain(|e| !matches!(e.kind, 10 | 30));
        for e in &mut prims {
            e.interp = 1;
        }
        let mesh_stride = match &native.io {
            StageIo::Ms(ms) => ms.threads.max(ms.max_vertices).max(ms.max_primitives),
            _ => 0,
        };
        super::super::lower::lower_with_profile(
            &super::super::lower::Parts {
                native: &native,
                p: &p,
                st: &st,
                io: &io,
                res: &res,
                inputs: Vec::new(),
                outputs,
                prims,
                mesh_stride,
                mesh_fallback: mode == super::super::MeshTranslation::ComputeFallback
                    && native.stage == HwStage::Ms,
            },
            profile,
        )
    }

    #[test]
    fn the_compute_fallback_stages_take_the_explicit_profile() {
        for (profile, model, header) in [
            (super::super::ShaderProfile::Sm65, (6u8, 5u8), 0x65u32),
            (super::super::ShaderProfile::Sm66, (6u8, 6u8), 0x66),
        ] {
            let lowered =
                lowered_with_profile(super::super::MeshTranslation::ComputeFallback, profile)
                    .unwrap();
            assert_eq!(lowered.profile, profile);
            let fallback = lowered.mesh_fallback.expect("fallback metadata");
            for (bytes, kind) in [
                (&lowered.dxil, ShaderKind::Compute),
                (&fallback.vertex_shader, ShaderKind::Vertex),
            ] {
                let container = Container::parse(bytes).unwrap();
                let program =
                    crate::DxilProgram::parse(container.part(FourCC::DXIL).unwrap().data).unwrap();
                assert_eq!(program.kind, kind);
                assert_eq!(program.shader_model, model);
                assert_eq!(
                    container.part(FourCC::DXIL).unwrap().data[..4],
                    [
                        header as u8,
                        0,
                        if kind == ShaderKind::Compute { 5 } else { 1 },
                        0
                    ]
                );
            }
        }
    }

    #[test]
    fn mesh_translates_to_compute_and_a_vertex_pulling_shader() {
        let shader = translated(super::super::MeshTranslation::ComputeFallback).unwrap();
        let fallback = shader.mesh_fallback.expect("fallback metadata");
        assert_eq!(fallback.vertex_stride, 20);
        assert_eq!(fallback.primitive_stride, 16);
        assert_eq!(fallback.max_vertices, 3);
        assert_eq!(fallback.max_primitives, 1);
        assert_eq!(fallback.group_bytes, 112);
        for (bytes, kind) in [
            (&shader.dxil, ShaderKind::Compute),
            (&fallback.vertex_shader, ShaderKind::Vertex),
        ] {
            #[cfg(windows)]
            if let Ok(mut validator) = super::super::validator::Validator::load(
                super::super::validator::Validator::default_path().as_deref(),
            ) {
                validator.validate(&mut bytes.clone()).unwrap();
            }
            let container = Container::parse(bytes).unwrap();
            let program =
                crate::DxilProgram::parse(container.part(FourCC::DXIL).unwrap().data).unwrap();
            assert_eq!(program.kind, kind);
        }
        assert!(shader.root_uavs.contains(&SLOT_MESH_OUTPUT));
    }

    #[test]
    fn hardware_mesh_translation_stays_a_mesh_shader() {
        let shader = translated(super::super::MeshTranslation::Hardware).unwrap();
        assert!(shader.mesh_fallback.is_none());
        let container = Container::parse(&shader.dxil).unwrap();
        let program =
            crate::DxilProgram::parse(container.part(FourCC::DXIL).unwrap().data).unwrap();
        assert_eq!(program.kind, ShaderKind::Mesh);
    }

    fn wrapper(outputs: &[SigElement], prims: &[SigElement]) -> Result<Vec<u8>, &'static str> {
        let mesh = MeshIo {
            outputs: vec![],
            clip_cntl: 0,
            threads: 32,
            max_vertices: 3,
            max_primitives: 1,
            topology: 2,
            lds_bytes: 0,
        };
        let layout = Layout::new(outputs, prims, &mesh)?;
        vertex_shader(&layout, outputs, prims, super::super::ShaderProfile::Sm65)
    }

    #[test]
    fn packed_outputs_keep_the_pixel_shader_registers_and_components() {
        let mut uv1 = element("TEXCOORD", 0, 1, 2);
        uv1.index = 1;
        uv1.col = 2;
        let mut clip1 = element("SV_ClipDistance", 6, 5, 2);
        clip1.index = 1;
        clip1.col = 2;
        let outputs = vec![
            element("SV_Position", 3, 0, 4),
            element("TEXCOORD", 0, 1, 2),
            uv1,
            element("COLOR", 0, 3, 4),
            element("SV_ClipDistance", 6, 5, 2),
            clip1,
            element("SV_CullDistance", 7, 6, 1),
        ];
        let prims = vec![element("PRIMDATA", 0, 4, 4)];
        let bytes = wrapper(&outputs, &prims).unwrap();
        let container = Container::parse(&bytes).unwrap();
        let emitted = super::super::signature(container.part(FourCC::OSG1).unwrap().data).unwrap();
        let expected: Vec<_> = outputs.iter().chain(&prims).collect();
        let program =
            crate::DxilProgram::parse(container.part(FourCC::DXIL).unwrap().data).unwrap();
        let bitcode = Bitcode::parse(program.bitcode);
        let stores: Vec<_> = expected
            .iter()
            .enumerate()
            .flat_map(|(id, e)| (0..e.cols).map(move |col| (id as u64, 0, u64::from(col))))
            .collect();
        assert_eq!(bitcode.output_stores(), stores);
        assert_eq!(emitted.len(), expected.len());
        for original in expected {
            let got = emitted
                .iter()
                .find(|e| e.name == original.name && e.index == original.index)
                .unwrap();
            assert_eq!((got.kind, got.comp), (original.kind, original.comp));
            assert_eq!(
                (got.row, got.col, got.cols),
                (original.row, original.col, original.cols),
                "{}{}",
                original.name,
                original.index
            );
        }
        #[cfg(windows)]
        if let Ok(mut validator) = super::super::validator::Validator::load(
            super::super::validator::Validator::default_path().as_deref(),
        ) {
            validator.validate(&mut bytes.clone()).unwrap();
        }
    }

    #[test]
    fn vertex_and_primitive_outputs_share_only_disjoint_components() {
        let outputs = vec![
            element("SV_Position", 3, 0, 4),
            element("TEXCOORD", 0, 1, 2),
        ];
        let mut primitive = element("PRIMDATA", 0, 1, 2);
        primitive.col = 2;
        let bytes = wrapper(&outputs, &[primitive.clone()]).unwrap();
        let container = Container::parse(&bytes).unwrap();
        let emitted = super::super::signature(container.part(FourCC::OSG1).unwrap().data).unwrap();
        assert_eq!(
            (emitted[1].row, emitted[1].col, emitted[1].cols),
            (Some(1), 0, 2)
        );
        assert_eq!(
            (emitted[2].row, emitted[2].col, emitted[2].cols),
            (Some(1), 2, 2)
        );
        primitive.col = 1;
        assert_eq!(
            wrapper(&outputs, &[primitive]).unwrap_err(),
            "overlapping mesh outputs in the vertex shader signature"
        );
    }

    #[test]
    fn unsupported_mesh_shapes_are_refused_before_emission() {
        let mut mesh = MeshIo {
            outputs: vec![],
            clip_cntl: 0,
            threads: 32,
            max_vertices: 3,
            max_primitives: 1,
            topology: 1,
            lds_bytes: 0,
        };
        assert!(Layout::new(&[], &[], &mesh).is_err());
        mesh.topology = 2;
        mesh.threads = 256;
        assert!(Layout::new(&[], &[], &mesh).is_err());
    }

    const MODULE_BLOCK: u64 = 8;
    const CONSTANTS_BLOCK: u64 = 11;
    const FUNCTION_BLOCK: u64 = 12;
    const VALUE_SYMTAB_BLOCK: u64 = 14;
    const TYPE_BLOCK: u64 = 17;

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TypeKind {
        Void,
        Int(u32),
        Func(u32),
        Other,
    }

    struct Record {
        id: Option<u32>,
        code: u64,
        ops: Vec<u64>,
    }

    struct Bits {
        words: Vec<u32>,
        at: usize,
    }

    impl Bits {
        fn new(bytes: &[u8]) -> Self {
            let words = bytes
                .chunks_exact(4)
                .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
                .collect();
            Self { words, at: 0 }
        }

        fn read(&mut self, width: u32) -> u64 {
            let mut value = 0;
            let mut done = 0;
            while done < width {
                let word = self.words[self.at / 32];
                let offset = (self.at % 32) as u32;
                let take = (32 - offset).min(width - done);
                let mask = u32::MAX >> (32 - take);
                value |= u64::from((word >> offset) & mask) << done;
                self.at += take as usize;
                done += take;
            }
            value
        }

        fn vbr(&mut self, width: u32) -> u64 {
            let hi = 1u64 << (width - 1);
            let mut value = 0;
            let mut shift = 0;
            loop {
                let piece = self.read(width);
                value |= (piece & (hi - 1)) << shift;
                if piece & hi == 0 {
                    return value;
                }
                shift += width - 1;
            }
        }

        fn align(&mut self) {
            self.at = self.at.div_ceil(32) * 32;
        }
    }

    fn signed(v: u64) -> i64 {
        ((v >> 1) as i64) ^ -((v & 1) as i64)
    }

    struct Bitcode {
        types: Vec<TypeKind>,
        consts: Vec<u64>,
        globals: u32,
        funcs: Vec<String>,
        symbols: Vec<(u32, String)>,
        records: Vec<Record>,
        by_id: HashMap<u32, usize>,
        next_id: u32,
    }

    impl Bitcode {
        fn parse(dxil: &[u8]) -> Self {
            let at = dxil
                .windows(4)
                .position(|w| w == b"BC\xC0\xDE")
                .expect("LLVM bitcode");
            let mut bits = Bits::new(&dxil[at..]);
            assert_eq!(bits.read(32), u64::from(u32::from_le_bytes(*b"BC\xC0\xDE")));
            let mut bitcode = Self {
                types: Vec::new(),
                consts: Vec::new(),
                globals: 0,
                funcs: Vec::new(),
                symbols: Vec::new(),
                records: Vec::new(),
                by_id: HashMap::new(),
                next_id: 0,
            };
            assert_eq!(bits.read(2), 1);
            let id = bits.vbr(8);
            assert_eq!(id, MODULE_BLOCK);
            bitcode.block(&mut bits, id);
            for (value, name) in &bitcode.symbols {
                let index = value.wrapping_sub(bitcode.globals) as usize;
                if *value >= bitcode.globals && index < bitcode.funcs.len() {
                    bitcode.funcs[index].clone_from(name);
                }
            }
            bitcode
        }

        fn block(&mut self, bits: &mut Bits, id: u64) {
            let width = bits.read(4) as u32;
            bits.align();
            let len = bits.read(32) as usize;
            let end = bits.at + len * 32;
            while bits.at < end {
                match bits.read(width) {
                    0 => break,
                    1 => {
                        let sub = bits.vbr(8);
                        self.block(bits, sub);
                    }
                    3 => {
                        let code = bits.vbr(6);
                        let n = bits.vbr(6) as usize;
                        let ops = (0..n).map(|_| bits.vbr(6)).collect();
                        self.record(id, code, ops);
                    }
                    other => panic!("bitstream abbreviation {other} in block {id}"),
                }
            }
            bits.at = end;
        }

        fn record(&mut self, block: u64, code: u64, ops: Vec<u64>) {
            match block {
                TYPE_BLOCK => match code {
                    1 | 19 => {}
                    2 => self.types.push(TypeKind::Void),
                    7 => self.types.push(TypeKind::Int(ops[0] as u32)),
                    21 => self.types.push(TypeKind::Func(ops[1] as u32)),
                    _ => self.types.push(TypeKind::Other),
                },
                MODULE_BLOCK => match code {
                    7 => self.globals += 1,
                    8 => self.funcs.push(String::new()),
                    _ => {}
                },
                CONSTANTS_BLOCK => match code {
                    1 => {}
                    4 => self.consts.push(signed(ops[0]) as u64),
                    _ => self.consts.push(0),
                },
                VALUE_SYMTAB_BLOCK => {
                    if code == 1 {
                        let name =
                            String::from_utf8(ops[1..].iter().map(|&b| b as u8).collect()).unwrap();
                        self.symbols.push((ops[0] as u32, name));
                    }
                }
                FUNCTION_BLOCK => {
                    if code == 1 {
                        self.next_id =
                            self.globals + self.funcs.len() as u32 + self.consts.len() as u32;
                        return;
                    }
                    let id = if self.produces(code, &ops) {
                        let id = self.next_id;
                        self.next_id += 1;
                        Some(id)
                    } else {
                        None
                    };
                    if let Some(id) = id {
                        self.by_id.insert(id, self.records.len());
                    }
                    self.records.push(Record { id, code, ops });
                }
                _ => {}
            }
        }

        fn produces(&self, code: u64, ops: &[u64]) -> bool {
            match code {
                10 | 11 | 12 | 24 | 44 => false,
                34 => match self.types.get(ops[2] as usize) {
                    Some(TypeKind::Func(ret)) => {
                        self.types.get(*ret as usize) != Some(&TypeKind::Void)
                    }
                    _ => true,
                },
                _ => true,
            }
        }

        fn output_stores(&self) -> Vec<(u64, u64, u64)> {
            let mut next = self.globals + self.funcs.len() as u32 + self.consts.len() as u32;
            let mut stores = Vec::new();
            for record in &self.records {
                let id = record.id.unwrap_or(next);
                next = id + u32::from(record.id.is_some());
                if record.code != 34 {
                    continue;
                }
                let callee = id - record.ops[3] as u32;
                let name = &self.funcs[(callee - self.globals) as usize];
                if name.starts_with("dx.op.storeOutput.") {
                    let value =
                        |n: usize| self.eval(id - record.ops[n] as u32, 0, 0, &mut HashMap::new());
                    stores.push((value(5), value(6), value(7)));
                }
            }
            stores
        }

        fn first_raw_load_address(&self) -> u32 {
            for record in &self.records {
                if record.code != 34 {
                    continue;
                }
                let id = record.id.expect("a value");
                let callee = id - record.ops[3] as u32;
                let name = &self.funcs[(callee - self.globals) as usize];
                if name == "dx.op.rawBufferLoad.f32" {
                    return id - record.ops[6] as u32;
                }
            }
            panic!("no rawBufferLoad");
        }

        fn eval(&self, id: u32, vertex: u64, group: u64, memo: &mut HashMap<u32, u64>) -> u64 {
            if let Some(&value) = memo.get(&id) {
                return value;
            }
            let funcs_at = self.globals + self.funcs.len() as u32;
            let base = funcs_at + self.consts.len() as u32;
            let value = if id < funcs_at {
                0
            } else if id < base {
                self.consts[(id - funcs_at) as usize]
            } else {
                let record = &self.records[self.by_id[&id]];
                let arg = |n: usize| id - record.ops[n] as u32;
                match record.code {
                    2 => {
                        let a = self.eval(arg(0), vertex, group, memo);
                        let b = self.eval(arg(1), vertex, group, memo);
                        match record.ops[2] {
                            0 => a.wrapping_add(b),
                            1 => a.wrapping_sub(b),
                            2 => a.wrapping_mul(b),
                            3 => a / b,
                            4 => (a as i64).wrapping_div(b as i64) as u64,
                            5 => a % b,
                            7 => a.wrapping_shl(b as u32),
                            8 => a.wrapping_shr(b as u32),
                            9 => (a as i64).wrapping_shr(b as u32) as u64,
                            10 => a & b,
                            11 => a | b,
                            12 => a ^ b,
                            op => panic!("binary op {op}"),
                        }
                    }
                    3 => {
                        let value = self.eval(arg(0), vertex, group, memo);
                        match self.types[record.ops[1] as usize] {
                            TypeKind::Int(bits) => value & (u64::MAX >> (64 - bits)),
                            _ => value,
                        }
                    }
                    34 => {
                        let callee = arg(3);
                        let name = &self.funcs[(callee - self.globals) as usize];
                        match name.as_str() {
                            "dx.op.loadInput.i32" => {
                                let slot = self.eval(arg(5), vertex, group, memo);
                                if slot == 0 { vertex } else { group }
                            }
                            other => panic!("call {other} in the index address"),
                        }
                    }
                    code => panic!("instruction {code} in the index address"),
                }
            };
            memo.insert(id, value);
            value
        }
    }

    #[test]
    fn every_corner_reads_its_index_at_a_four_byte_address() {
        let outputs = vec![
            element("SV_Position", 3, 0, 4),
            element("TEXCOORD", 0, 1, 1),
        ];
        let prims = vec![element("PRIMDATA", 0, 2, 4)];
        let mesh = MeshIo {
            outputs: vec![],
            clip_cntl: 0,
            threads: 32,
            max_vertices: 9,
            max_primitives: 3,
            topology: 2,
            lds_bytes: 0,
        };
        let layout = Layout::new(&outputs, &prims, &mesh).unwrap();
        let shader =
            vertex_shader(&layout, &outputs, &prims, super::super::ShaderProfile::Sm66).unwrap();
        let container = Container::parse(&shader).unwrap();
        let program =
            crate::DxilProgram::parse(container.part(FourCC::DXIL).unwrap().data).unwrap();
        let bitcode = Bitcode::parse(program.bitcode);
        let address = bitcode.first_raw_load_address();
        for group in [0u64, 7] {
            for primitive in 0..3u64 {
                let mut offsets = Vec::new();
                for corner in 0..3u64 {
                    let vertex_id = primitive * 3 + corner;
                    let mut memo = HashMap::new();
                    let got = bitcode.eval(address, vertex_id, group, &mut memo);
                    let base = u64::from(layout.group_bytes) * group + 16;
                    assert_eq!(
                        got,
                        base + 4 * vertex_id,
                        "group {group}, primitive {primitive}, corner {corner}"
                    );
                    offsets.push(got - base);
                }
                assert_eq!(
                    offsets,
                    [12 * primitive, 12 * primitive + 4, 12 * primitive + 8],
                    "primitive {primitive}"
                );
            }
        }
    }
}
