//! The console's ray-tracing node test, `image_bvh_intersect_ray` and
//! `image_bvh64_intersect_ray`, as inline DXIL: the host has no instruction
//! that tests one ray against one node of the title's own structures.
//!
//! [`crate::bvh`] says what the instruction computes and is the reference:
//! the code made here does the same float operations in the same order
//! (DXIL float operations carry no fast-math flags, so the host may not
//! reorder or fuse them), and the tests in `intersect_tests.rs` run it on
//! the card against the reference.
//!
//! The title's nodes are in title memory, named by address: they are read
//! through the host's copies of title memory ([`Lower::memory`]), one look
//! in its directory per node (two for a 128-byte box node).
//!
//! Shape of the code, under the instruction's lane guard:
//!
//! ```text
//! kind = pointer & 7
//! box (4, 5):  4 child pointers; 24 box floats (kind 5) or 24 halves widened (kind 4);
//!              four slab tests; the sorting network
//! triangle (0-3): the id dword, three vertices at the bytes it names; the watertight test
//! other (6, 7): a triangle miss
//! ```
//!
//! Each lane reads only the dwords its node's kind needs: 28 for a 32-bit
//! box node, 16 for a 16-bit one, 10 for a triangle.

use super::super::ray::RayProfile;
use super::{Attr, Bin, Fields, Inst, Lower, Operand, Reg, Term, V, Why, flags, pred};
use crate::bvh;
use crate::resource::BvhNodes;

#[cfg(test)]
thread_local! {
    /// The tests' stand-in for title memory ([`Lower::bvh_memory`]): a
    /// heap slot holding a raw buffer, and the title address of its first
    /// byte. (The card tests have no host to keep copies of title memory
    /// and their directory.)
    pub(in crate::emit) static TEST_MEMORY: std::cell::Cell<Option<(u32, u64)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    static TEST_BVH_REAL_CALLS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Where the instruction's node is.
#[derive(Clone, Copy)]
enum NodeAt {
    /// At this byte of a raw buffer of the host.
    Buffer { handle: V, byte: V },
    /// In title memory: the raw view of the host copy holding the node's
    /// first 64 bytes and the node's byte in it ([`Lower::bvh_memory`]),
    /// and the node's address, for the second half of a 128-byte node.
    Memory { handle: V, byte: V, low: V, high: V },
}

/// The ray operands, as floats.
struct Ray {
    extent: V,
    origin: [V; 3],
    direction: [V; 3],
    inverse: [V; 3],
}

struct BvhInst {
    dst: u8,
    site: (usize, usize),
    rsrc: Operand,
    nodes: BvhNodes,
    sort: bool,
    guard: Option<V>,
    pointer: V,
    pointer_high: Option<V>,
    ray: Ray,
}

const BOX_MISS: [u32; 4] = [bvh::NONE; 4];
const TRIANGLE_MISS: [u32; 4] = [
    f32::INFINITY.to_bits(),
    1.0f32.to_bits(),
    0.0f32.to_bits(),
    0.0f32.to_bits(),
];

#[cfg(test)]
fn miss_words(pointer: u32) -> [u32; 4] {
    if pointer & 6 == 4 {
        BOX_MISS
    } else {
        TRIANGLE_MISS
    }
}

impl<'a> Lower<'a> {
    /// `image_bvh_intersect_ray` and `image_bvh64_intersect_ray`.
    pub(super) fn bvh(&mut self, inst: &Inst, b: usize, i: usize) -> Result<(), Why> {
        let original_guard = self.guard(b, i)?;
        match self.bvh_profile {
            RayProfile::Full => {
                let parsed = self.bvh_parse(inst, b, i, original_guard)?;
                let results = self.bvh_real(inst, &parsed)?;
                self.bvh_write(inst, &parsed, &results);
                Ok(())
            }
            RayProfile::StaticMiss => self.bvh_static_miss(inst, b, i, original_guard),
            RayProfile::Limited(_) => self.bvh_limited(inst, b, i, original_guard),
        }
    }

    pub(super) fn bvh_static_miss(
        &mut self,
        inst: &Inst,
        b: usize,
        i: usize,
        original_guard: Option<V>,
    ) -> Result<(), Why> {
        let parsed = self.bvh_parse(inst, b, i, original_guard)?;
        let results = self.bvh_miss(&parsed);
        self.bvh_write(inst, &parsed, &results);
        Ok(())
    }

    pub(super) fn bvh_limited(
        &mut self,
        inst: &Inst,
        b: usize,
        i: usize,
        original_guard: Option<V>,
    ) -> Result<(), Why> {
        #[cfg(test)]
        {
            self.bvh_limited_sites += 1;
        }
        let parsed = self.bvh_parse(inst, b, i, original_guard)?;
        let budget = self
            .bvh_budget
            .as_ref()
            .and_then(|budget| budget.current)
            .ok_or("a limited ray-tracing node test without a budget")?;
        let outside = self.bvh_miss(&parsed);
        let c = self.cur;
        let zero = self.s.m.i32(0);
        let has_budget = self.s.m.cmp(c, pred::NE, budget, zero);
        let run = match original_guard {
            Some(guard) => self.s.m.bin(c, Bin::And, has_budget, guard),
            None => has_budget,
        };
        let gate = self.begin_guarded(Some(run));
        let inside = self.bvh_real(inst, &parsed)?;
        let pairs: Vec<(V, V)> = inside.into_iter().zip(outside).collect();
        let results = self.end_guarded(gate, &pairs);
        let one = self.s.m.i32(1);
        let spent = self.s.m.bin(self.cur, Bin::Sub, budget, one);
        let next = self.s.m.select(self.cur, run, spent, budget);
        self.bvh_budget.as_mut().expect("limited").current = Some(next);
        self.bvh_write(inst, &parsed, &results);
        Ok(())
    }

    fn bvh_parse(
        &mut self,
        inst: &Inst,
        b: usize,
        i: usize,
        original_guard: Option<V>,
    ) -> Result<BvhInst, Why> {
        let Fields::Image {
            data,
            addr,
            rsrc,
            dmask,
            a16,
            d16,
            tfe,
            lwe,
            ..
        } = &inst.fields
        else {
            unreachable!()
        };
        if *a16 || *d16 || *tfe || *lwe || *dmask != 0xf {
            return Err("a ray-tracing node test with 16-bit operands or a partial result");
        }
        let wide = inst.name() == "image_bvh64_intersect_ray";
        // Operand VGPRs in order: the node pointer (two dwords in the
        // 64-bit form), the ray's extent, origin, direction and inverse
        // direction.
        let mut regs = Vec::new();
        for a in addr {
            if let Operand::Vector { reg, dwords } = *a {
                regs.extend((0..dwords).map(|k| reg + k));
            }
        }
        if regs.len() != if wide { 12 } else { 11 } {
            return Err("a ray-tracing node test's operands");
        }
        let Operand::Vector { reg: dst, .. } = *data else {
            return Err("a ray-tracing node test's result");
        };
        let site = (b, i);
        let nodes = self
            .x
            .res
            .bvh
            .iter()
            .find(|n| n.0 == site)
            .map(|n| n.1)
            .ok_or("an untracked ray-tracing structure")?;
        let (sort, barycentrics) = match nodes {
            BvhNodes::Resource => (true, true),
            BvhNodes::Memory { word1, word3 } => (
                word1 & bvh::BOX_SORT != 0,
                word3 & bvh::RETURN_BARYCENTRICS != 0,
            ),
        };
        if !barycentrics {
            // Mode 0 returns a triangle id and a hit status in the last two
            // dwords; no program of the title asks for it.
            return Err("a ray-tracing structure that returns triangle ids");
        }
        let guard = original_guard;
        let mut x = Vec::with_capacity(regs.len());
        for &r in &regs {
            let v = inst.read(Reg::V(r)).ok_or("an unread operand")?;
            x.push(self.i32_of(v)?);
        }
        let pointer = x[0];
        let pointer_high = wide.then(|| x[1]);
        let f: Vec<V> = x[1 + usize::from(wide)..]
            .iter()
            .map(|&v| self.as_float(v))
            .collect();
        let ray = Ray {
            extent: f[0],
            origin: [f[1], f[2], f[3]],
            direction: [f[4], f[5], f[6]],
            inverse: [f[7], f[8], f[9]],
        };
        Ok(BvhInst {
            dst,
            site,
            rsrc: *rsrc,
            nodes,
            sort,
            guard,
            pointer,
            pointer_high,
            ray,
        })
    }

    fn bvh_real(&mut self, inst: &Inst, parsed: &BvhInst) -> Result<[V; 4], Why> {
        #[cfg(test)]
        TEST_BVH_REAL_CALLS.with(|calls| calls.set(calls.get() + 1));
        let (site, nodes, sort, guard) = (parsed.site, parsed.nodes, parsed.sort, parsed.guard);
        let (pointer, pointer_high) = (parsed.pointer, parsed.pointer_high);
        let c = self.cur;
        let at = match nodes {
            BvhNodes::Resource => {
                let src = self
                    .descriptors
                    .get(&(site, false))
                    .copied()
                    .ok_or("an untracked ray-tracing structure")?;
                let handle = self.handle(site, src, false, (11, 0))?;
                // (pointer >> 3) << 6 from the buffer's start.
                let (kind_bits, k3) = (self.s.m.i32(!7), self.s.m.i32(3));
                let byte = self.s.m.bin(c, Bin::And, pointer, kind_bits);
                let byte = self.s.m.bin(c, Bin::Shl, byte, k3);
                NodeAt::Buffer { handle, byte }
            }
            BvhNodes::Memory { .. } => {
                let Operand::Scalar { code, .. } = parsed.rsrc else {
                    return Err("a ray-tracing structure descriptor not in SGPRs");
                };
                let mut words = [pointer; 2];
                for (k, w) in words.iter_mut().enumerate() {
                    let v = inst
                        .read(Reg::S(code + k as u8))
                        .ok_or("an unread operand")?;
                    *w = self.i32_of(v)?;
                }
                let (low, high) = self.bvh_address(words[0], words[1], pointer, pointer_high);
                // One look for the node's copy, before the branches: a
                // node is 64 bytes on 64, so its first half lies in one
                // 64 KiB page whatever its kind.
                let (handle, byte) = self.bvh_memory(low, high, guard);
                NodeAt::Memory {
                    handle,
                    byte,
                    low,
                    high,
                }
            }
        };
        let zero = self.s.m.i32(0);
        let gate = self.begin_guarded(guard);
        let head = self.cur;
        let (k4, k6, k7) = (self.s.m.i32(4), self.s.m.i32(6), self.s.m.i32(7));
        let kind = self.s.m.bin(head, Bin::And, pointer, k7);
        let pair = self.s.m.bin(head, Bin::And, pointer, k6);
        let is_box = self.s.m.cmp(head, pred::EQ, pair, k4);
        let is_triangle = self.s.m.cmp(head, pred::IULT, kind, k4);
        let (box_block, leaf_block) = (self.s.m.new_block(), self.s.m.new_block());
        let (triangle_block, join) = (self.s.m.new_block(), self.s.m.new_block());
        self.s
            .m
            .terminate(head, Term::CondBr(is_box, box_block, leaf_block));
        self.s
            .m
            .terminate(leaf_block, Term::CondBr(is_triangle, triangle_block, join));
        self.cur = box_block;
        let boxes = self.bvh_boxes(at, kind, &parsed.ray, sort);
        let box_end = self.cur;
        self.s.m.terminate(box_end, Term::Br(join));
        self.cur = triangle_block;
        let triangle = self.bvh_triangle(at, kind, &parsed.ray);
        let triangle_end = self.cur;
        self.s.m.terminate(triangle_end, Term::Br(join));
        self.cur = join;
        // Kinds 6 and 7 (an instance, a procedural primitive): not known
        // what the console returns; the title's programs do not read it.
        let other = TRIANGLE_MISS.map(|v| self.s.m.i32(v));
        let i32t = self.s.t.i32;
        let mut results = Vec::with_capacity(4);
        for k in 0..4 {
            let phi = self.s.m.phi(join, i32t);
            self.s.m.set_phi(
                phi,
                vec![
                    (boxes[k], box_end),
                    (triangle[k], triangle_end),
                    (other[k], leaf_block),
                ],
            );
            results.push((phi, zero));
        }
        let results = self.end_guarded(gate, &results);
        Ok([results[0], results[1], results[2], results[3]])
    }

    fn bvh_miss(&mut self, parsed: &BvhInst) -> [V; 4] {
        let c = self.cur;
        let (k4, k6) = (self.s.m.i32(4), self.s.m.i32(6));
        let pair = self.s.m.bin(c, Bin::And, parsed.pointer, k6);
        let is_box = self.s.m.cmp(c, pred::EQ, pair, k4);
        let triangle = TRIANGLE_MISS.map(|v| self.s.m.i32(v));
        std::array::from_fn(|k| {
            let none = self.s.m.i32(BOX_MISS[k]);
            self.s.m.select(c, is_box, none, triangle[k])
        })
    }

    fn bvh_write(&mut self, inst: &Inst, parsed: &BvhInst, results: &[V]) {
        for (k, &r) in results.iter().enumerate() {
            self.write(inst, Reg::V(parsed.dst + k as u8), r, parsed.guard);
        }
    }

    /// Title memory at `low`/`high` for a node's bytes: the raw view of the
    /// host copy holding the address and the byte in that view
    /// ([`Lower::memory`]: a page the host has no copy of reads zeros and
    /// is left for the host to fetch, for lanes that run).
    fn bvh_memory(&mut self, low: V, high: V, guard: Option<V>) -> (V, V) {
        #[cfg(test)]
        if let Some((slot, base)) = TEST_MEMORY.get() {
            // The buffer at heap slot `slot` holds the bytes from title
            // address `base` on; any other address reads past its end,
            // which is zeros.
            let c = self.cur;
            let slot = self.s.m.i32(slot);
            let handle = self.s.heap_handle(c, slot, false, false, (11, 0));
            let (base_low, base_high) =
                (self.s.m.i32(base as u32), self.s.m.i32((base >> 32) as u32));
            let at = self.s.m.bin(c, Bin::Sub, low, base_low);
            let in_low = self.s.m.cmp(c, pred::IUGE, low, base_low);
            let in_high = self.s.m.cmp(c, pred::EQ, high, base_high);
            let inside = self.s.m.bin(c, Bin::And, in_low, in_high);
            let outside = self.s.m.i32(0xffff_ff00);
            let at = self.s.m.select(c, inside, at, outside);
            return (handle, at);
        }
        self.memory(low, high, guard)
    }

    /// `count` dwords of the node from its byte `base` + `offset` (`base`
    /// computed, `offset` fixed; none of the node's bytes past 127), read
    /// four at a time.
    fn bvh_node_words(&mut self, at: NodeAt, base: Option<V>, offset: u32, count: u32) -> Vec<V> {
        let c = self.cur;
        let (undef, align) = (self.s.m.undef(self.s.t.i32), self.s.m.i32(4));
        // The second half of a 128-byte node may lie in the next 64 KiB
        // page, which may be in another copy: its own look, made once.
        let mut second = None;
        let mut out = Vec::with_capacity(count as usize);
        for start in (0..count).step_by(4) {
            let n = (count - start).min(4);
            let node_byte = offset + 4 * start;
            let (handle, mut byte) = match at {
                NodeAt::Buffer { handle, byte } => (handle, byte),
                NodeAt::Memory { handle, byte, .. } if node_byte < 64 => (handle, byte),
                NodeAt::Memory { low, high, .. } => *second.get_or_insert_with(|| {
                    let (low, high) = self.address_plus(low, high, 64);
                    // Every lane in this block runs the instruction.
                    let (handle, byte) = self.bvh_memory(low, high, None);
                    let k64 = self.s.m.i32(64);
                    (handle, self.s.m.bin(c, Bin::Sub, byte, k64))
                }),
            };
            if let Some(b) = base {
                byte = self.s.m.bin(c, Bin::Add, byte, b);
            }
            let k = self.s.m.i32(node_byte);
            let byte = self.s.m.bin(c, Bin::Add, byte, k);
            let mask = self.s.m.i8((1u8 << n) - 1);
            let r = self.s.op(
                c,
                "rawBufferLoad.f32",
                Attr::ReadOnly,
                self.s.t.res_ret,
                139,
                &[handle, byte, undef, mask, align],
            );
            for k in 0..n {
                let f = self.s.m.extract(c, r, k);
                out.push(self.as_int(f));
            }
        }
        out
    }

    /// The node's title address, [`bvh::node_address`]: the descriptor's
    /// base (dword 0 and the low byte of dword 1, in 256-byte units) plus
    /// `(pointer >> 3) << 6`.
    fn bvh_address(&mut self, word0: V, word1: V, pointer: V, pointer_high: Option<V>) -> (V, V) {
        let c = self.cur;
        let i64t = self.s.m.int(64);
        let (zero, byte) = (self.s.m.i32(0), self.s.m.i32(0xff));
        let base_high = self.s.m.bin(c, Bin::And, word1, byte);
        let base = self.join64(word0, base_high);
        let k8 = self.s.m.int_const(i64t, 8);
        let base = self.s.m.bin(c, Bin::Shl, base, k8);
        let p = self.join64(pointer, pointer_high.unwrap_or(zero));
        let (k3, k6) = (self.s.m.int_const(i64t, 3), self.s.m.int_const(i64t, 6));
        let p = self.s.m.bin(c, Bin::LShr, p, k3);
        let p = self.s.m.bin(c, Bin::Shl, p, k6);
        let a = self.s.m.bin(c, Bin::Add, base, p);
        self.s.flags |= flags::INT64;
        self.split64(a)
    }

    /// The box test ([`bvh::boxes`]): four result dwords. Starts in the
    /// current block and leaves `self.cur` at the block it ends in.
    fn bvh_boxes(&mut self, at: NodeAt, kind: V, ray: &Ray, sort: bool) -> [V; 4] {
        let head = self.cur;
        let children = self.bvh_node_words(at, None, 0, 4);
        let k5 = self.s.m.i32(5);
        let wide = self.s.m.cmp(head, pred::EQ, kind, k5);
        let (wide_block, half_block) = (self.s.m.new_block(), self.s.m.new_block());
        let join = self.s.m.new_block();
        self.s
            .m
            .terminate(head, Term::CondBr(wide, wide_block, half_block));
        // 32-bit floats: 24 dwords from byte 16.
        self.cur = wide_block;
        let k16 = self.s.m.i32(16);
        let words = self.bvh_node_words(at, None, 16, 24);
        let floats: Vec<V> = words.iter().map(|&w| self.as_float(w)).collect();
        self.s.m.terminate(wide_block, Term::Br(join));
        // 16-bit floats: 24 halves in 12 dwords from byte 16, the low half
        // of a dword first.
        self.cur = half_block;
        let words = self.bvh_node_words(at, None, 16, 12);
        let f32t = self.s.t.f32;
        let mut halves = Vec::with_capacity(24);
        for n in 0..24 {
            let mut h = words[n / 2];
            if n % 2 == 1 {
                h = self.s.m.bin(half_block, Bin::LShr, h, k16);
            }
            // The conversion reads the low 16 bits.
            halves.push(self.s.op(
                half_block,
                "legacyF16ToF32",
                Attr::ReadNone,
                f32t,
                131,
                &[h],
            ));
        }
        self.s.m.terminate(half_block, Term::Br(join));
        self.cur = join;
        let mut coords = Vec::with_capacity(24);
        for n in 0..24 {
            let phi = self.s.m.phi(join, f32t);
            self.s
                .m
                .set_phi(phi, vec![(floats[n], wide_block), (halves[n], half_block)]);
            coords.push(phi);
        }
        // The inverse direction held finite ([`bvh::boxes`]).
        let (most, least) = (
            self.s.m.f32(f32::MAX.to_bits()),
            self.s.m.f32((-f32::MAX).to_bits()),
        );
        let inverse = ray.inverse.map(|x| {
            let x = self.binary(35, x, least);
            self.binary(36, x, most)
        });
        let c = join;
        let (none, infinity, fzero) = (
            self.s.m.i32(BOX_MISS[0]),
            self.s.m.f32(f32::INFINITY.to_bits()),
            self.s.m.f32(0),
        );
        let mut ids = [none; 4];
        let mut distances = [infinity; 4];
        for n in 0..4 {
            let bx = &coords[6 * n..6 * n + 6];
            let (mut near, mut far) = ([fzero; 3], [fzero; 3]);
            for a in 0..3 {
                let low = self.s.m.bin(c, Bin::Sub, bx[a], ray.origin[a]);
                let low = self.s.m.bin(c, Bin::Mul, low, inverse[a]);
                let high = self.s.m.bin(c, Bin::Sub, bx[3 + a], ray.origin[a]);
                let high = self.s.m.bin(c, Bin::Mul, high, inverse[a]);
                near[a] = self.binary(36, low, high);
                far[a] = self.binary(35, low, high);
            }
            let enter = self.binary(35, near[0], near[1]);
            let enter = self.binary(35, enter, near[2]);
            let leave = self.binary(36, far[0], far[1]);
            let leave = self.binary(36, leave, far[2]);
            let from = self.binary(35, enter, fzero);
            // Ordered compares: a NaN anywhere is a miss.
            let through = self.s.m.cmp(c, pred::OGE, leave, from);
            let before = self.s.m.cmp(c, pred::OLT, enter, ray.extent);
            let off = self.is_nan_f32(bx[0]);
            let on = self.s.m.bool(true);
            let on = self.s.m.bin(c, Bin::Xor, off, on);
            let hit = self.s.m.bin(c, Bin::And, through, before);
            let hit = self.s.m.bin(c, Bin::And, hit, on);
            ids[n] = self.s.m.select(c, hit, children[n], none);
            distances[n] = self.s.m.select(c, hit, enter, infinity);
        }
        if sort {
            for (a, b) in [(0, 1), (2, 3), (0, 2), (1, 3), (1, 2)] {
                let swap = self.s.m.cmp(c, pred::OLT, distances[b], distances[a]);
                let (da, db) = (distances[a], distances[b]);
                distances[a] = self.s.m.select(c, swap, db, da);
                distances[b] = self.s.m.select(c, swap, da, db);
                let (ia, ib) = (ids[a], ids[b]);
                ids[a] = self.s.m.select(c, swap, ib, ia);
                ids[b] = self.s.m.select(c, swap, ia, ib);
            }
        }
        ids
    }

    /// The triangle test ([`bvh::triangle`] of [`bvh::triangle_vertices`]):
    /// four result dwords, made in the current block.
    fn bvh_triangle(&mut self, at: NodeAt, kind: V, ray: &Ray) -> [V; 4] {
        let c = self.cur;
        let k = |l: &mut Self, v: u32| l.s.m.i32(v);
        // The triangle's byte of the dword at byte 60: which of its
        // vertices barycentric I and J weigh.
        let id = self.bvh_node_words(at, None, 60, 1)[0];
        let (k2, k3, k12, k15) = (k(self, 2), k(self, 3), k(self, 12), k(self, 15));
        let shift = self.s.m.bin(c, Bin::Shl, kind, k3);
        let id = self.s.m.bin(c, Bin::LShr, id, shift);
        let i = self.s.m.bin(c, Bin::And, id, k3);
        let j = self.s.m.bin(c, Bin::LShr, id, k2);
        let j = self.s.m.bin(c, Bin::And, j, k3);
        let first = self.s.m.bin(c, Bin::Sub, k3, i);
        let first = self.s.m.bin(c, Bin::Sub, first, j);
        let first = self.s.m.bin(c, Bin::And, first, k3);
        // The node's vertices the triangle is made of, four bits each.
        let (k0, k1) = (k(self, 0), k(self, 1));
        let maps: [V; 4] = std::array::from_fn(|n| k(self, bvh::vertex_map(n as u32)));
        let (is0, is1, is2) = (
            self.s.m.cmp(c, pred::EQ, kind, k0),
            self.s.m.cmp(c, pred::EQ, kind, k1),
            self.s.m.cmp(c, pred::EQ, kind, k2),
        );
        let map = self.s.m.select(c, is2, maps[2], maps[3]);
        let map = self.s.m.select(c, is1, maps[1], map);
        let map = self.s.m.select(c, is0, maps[0], map);
        let mut p = Vec::with_capacity(3);
        for n in [first, i, j] {
            let nibble = self.s.m.bin(c, Bin::Shl, n, k2);
            let vertex = self.s.m.bin(c, Bin::LShr, map, nibble);
            let vertex = self.s.m.bin(c, Bin::And, vertex, k15);
            let byte = self.s.m.bin(c, Bin::Mul, vertex, k12);
            let words = self.bvh_node_words(at, Some(byte), 0, 3);
            // Relative to the ray's origin.
            let r: Vec<V> = (0..3)
                .map(|a| {
                    let f = self.as_float(words[a]);
                    self.s.m.bin(c, Bin::Sub, f, ray.origin[a])
                })
                .collect();
            p.push([r[0], r[1], r[2]]);
        }
        // The ray's largest axis z, and x and y after it in an order that
        // keeps the winding.
        let fzero = self.s.m.f32(0);
        let size = ray.direction.map(|d| self.unary(6, d));
        let xy = self.s.m.cmp(c, pred::OGE, size[0], size[1]);
        let xz = self.s.m.cmp(c, pred::OGE, size[0], size[2]);
        let yz = self.s.m.cmp(c, pred::OGE, size[1], size[2]);
        let z0 = self.s.m.bin(c, Bin::And, xy, xz);
        let on = self.s.m.bool(true);
        let not_xy = self.s.m.bin(c, Bin::Xor, xy, on);
        let z1 = self.s.m.bin(c, Bin::And, not_xy, yz);
        // Component (kz + n) % 3 of `q`.
        let pick = |l: &mut Self, q: [V; 3], n: usize| {
            let rest = l.s.m.select(c, z1, q[(1 + n) % 3], q[(2 + n) % 3]);
            l.s.m.select(c, z0, q[n % 3], rest)
        };
        let dz = pick(self, ray.direction, 0);
        let backwards = self.s.m.cmp(c, pred::OLT, dz, fzero);
        // (x, y, z) of `q`: the two after the largest axis, swapped for a
        // ray that runs backwards along it.
        let axes = |l: &mut Self, q: [V; 3]| {
            let (x, y) = (pick(l, q, 1), pick(l, q, 2));
            [
                l.s.m.select(c, backwards, y, x),
                l.s.m.select(c, backwards, x, y),
                pick(l, q, 0),
            ]
        };
        let d = axes(self, ray.direction);
        let sz = pick(self, ray.inverse, 0);
        let sx = self.s.m.bin(c, Bin::Mul, d[0], sz);
        let sy = self.s.m.bin(c, Bin::Mul, d[1], sz);
        let mut sheared = Vec::with_capacity(3);
        for r in p {
            let r = axes(self, r);
            let x = self.s.m.bin(c, Bin::Mul, sx, r[2]);
            let x = self.s.m.bin(c, Bin::Sub, r[0], x);
            let y = self.s.m.bin(c, Bin::Mul, sy, r[2]);
            let y = self.s.m.bin(c, Bin::Sub, r[1], y);
            let z = self.s.m.bin(c, Bin::Mul, sz, r[2]);
            sheared.push((x, y, z));
        }
        let [(ax, ay, az), (bx, by, bz), (cx, cy, cz)] = sheared[..] else {
            unreachable!()
        };
        // p * q - r * s.
        let cross = |l: &mut Self, p: V, q: V, r: V, s: V| {
            let a = l.s.m.bin(c, Bin::Mul, p, q);
            let b = l.s.m.bin(c, Bin::Mul, r, s);
            l.s.m.bin(c, Bin::Sub, a, b)
        };
        let u = cross(self, cx, by, cy, bx);
        let v = cross(self, ax, cy, ay, cx);
        let w = cross(self, bx, ay, by, ax);
        let any = |l: &mut Self, p: u8| {
            let a = l.s.m.cmp(c, p, u, fzero);
            let b = l.s.m.cmp(c, p, v, fzero);
            let d = l.s.m.cmp(c, p, w, fzero);
            let a = l.s.m.bin(c, Bin::Or, a, b);
            l.s.m.bin(c, Bin::Or, a, d)
        };
        let behind = any(self, pred::OLT);
        let before = any(self, pred::OGT);
        let outside = self.s.m.bin(c, Bin::And, behind, before);
        let inside = self.s.m.bin(c, Bin::Xor, outside, on);
        let det = self.s.m.bin(c, Bin::Add, v, w);
        let det = self.s.m.bin(c, Bin::Add, u, det);
        let t = self.s.m.bin(c, Bin::Mul, u, az);
        let t2 = self.s.m.bin(c, Bin::Mul, v, bz);
        let t = self.s.m.bin(c, Bin::Add, t, t2);
        let t2 = self.s.m.bin(c, Bin::Mul, w, cz);
        let t = self.s.m.bin(c, Bin::Add, t, t2);
        // The distance is not negative: the numerator has the
        // denominator's sign (or is 0). Ordered compares: a denominator
        // of 0 or a NaN is a miss.
        let front = self.s.m.cmp(c, pred::OGT, det, fzero);
        let front_ahead = self.s.m.cmp(c, pred::OGE, t, fzero);
        let front = self.s.m.bin(c, Bin::And, front, front_ahead);
        let back = self.s.m.cmp(c, pred::OLT, det, fzero);
        let back_ahead = self.s.m.cmp(c, pred::OLE, t, fzero);
        let back = self.s.m.bin(c, Bin::And, back, back_ahead);
        let ahead = self.s.m.bin(c, Bin::Or, front, back);
        let hit = self.s.m.bin(c, Bin::And, inside, ahead);
        let miss = bvh::MISS.map(|f| self.s.m.f32(f.to_bits()));
        let out = [t, det, v, w];
        std::array::from_fn(|n| {
            let f = self.s.m.select(c, hit, out[n], miss[n]);
            self.as_int(f)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Lowered, Parts, lower, lower_with_profile};
    use super::{BOX_MISS, TEST_BVH_REAL_CALLS, TRIANGLE_MISS, miss_words};
    use crate::bvh;
    use crate::emit::HostShader;
    use crate::emit::ray::{RayProfile, TEST_BVH_PROFILE};
    use crate::emit::{SLOT_FAULTS, SLOT_MEMORY};
    use crate::native::{
        ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
    };
    use crate::{io, ir, resource, structure};

    struct TestMode;

    impl TestMode {
        fn new(profile: RayProfile) -> Self {
            TEST_BVH_PROFILE.set(Some(profile));
            TEST_BVH_REAL_CALLS.set(0);
            Self
        }
    }

    impl Drop for TestMode {
        fn drop(&mut self) {
            TEST_BVH_PROFILE.set(None);
            TEST_BVH_REAL_CALLS.set(0);
        }
    }

    fn prologue(_wide: bool) -> Vec<u32> {
        let mut words = vec![
            0xBE85_0380,
            0xF408_0202,
            0xFA00_0000,
            0xF408_0302,
            0xFA00_0020,
            0x3428_0086,
            0xE038_1000,
            0x8002_0114,
            0xE038_1010,
            0x8002_0514,
            0xE038_1020,
            0x8002_0914,
            0xE030_1030,
            0x8002_1114,
        ];
        for k in 0..4u32 {
            words.push(0x7E00_0200 | (13 + k) << 17 | (0x81 + k));
        }
        words.extend([
            0xBE98_0380,
            0xBE99_0B81,
            0xBE9A_03C1,
            0xBE9B_03FF,
            0x8900_03FF,
        ]);
        words
    }

    fn mask(reg: u32) -> [u32; 2] {
        [0xBE94_037E, 0x7DAA_2280 & !(0xFF << 9) | reg << 9]
    }

    fn bvh_words(wide: bool, dst: u32) -> [u32; 5] {
        let first = 0x0006_0001 | dst << 8;
        if wide {
            [0xF19C_9F07, first, 0x0504_0302, 0x0908_0706, 0x000C_0B0A]
        } else {
            [0xF198_9F07, first, 0x0605_0403, 0x0A09_0807, 0x0000_0C0B]
        }
    }

    fn epilogue(words: &mut Vec<u32>, shift: u32, stores: &[(u32, u32)]) {
        finish(words, shift, stores, true);
    }

    fn finish(words: &mut Vec<u32>, shift: u32, stores: &[(u32, u32)], restore: bool) {
        if restore {
            words.push(0xBEFE_0314);
        }
        words.push(0x342A_0080 | shift);
        for &(dst, offset) in stores {
            words.push(0xE078_1000 | offset);
            words.push(0x8003_0000 | dst << 8 | 0x15);
        }
        words.push(0xBF81_0000);
    }

    fn program(wide: bool) -> Vec<u32> {
        let mut words = prologue(wide);
        words.extend(mask(17));
        words.extend(bvh_words(wide, 13));
        epilogue(&mut words, 4, &[(13, 0)]);
        words
    }

    fn program_two(wide: bool) -> Vec<u32> {
        let mut words = prologue(wide);
        words.extend(mask(17));
        words.extend(bvh_words(wide, 13));
        words.extend(bvh_words(wide, 17));
        epilogue(&mut words, 5, &[(13, 0), (17, 16)]);
        words
    }

    fn program_plain(wide: bool) -> Vec<u32> {
        let mut words = prologue(wide);
        words.extend(bvh_words(wide, 13));
        finish(&mut words, 4, &[(13, 0)], false);
        words
    }

    fn program_two_flags(wide: bool) -> Vec<u32> {
        let mut words = prologue(wide);
        words.extend([0xE030_1034, 0x8002_1614]);
        words.extend(mask(17));
        words.extend(bvh_words(wide, 13));
        words.push(0xBEFE_0314);
        words.extend(mask(22));
        words.extend(bvh_words(wide, 17));
        epilogue(&mut words, 5, &[(13, 0), (17, 16)]);
        words
    }

    fn program_loop(wide: bool) -> Vec<u32> {
        let mut words = prologue(wide);
        words.extend(mask(17));
        words.push(0xBE86_0382);
        let start = words.len();
        words.extend(bvh_words(wide, 13));
        words.push(0x8106_C106);
        words.push(0xBF02_8006);
        let branch = words.len();
        let back = start as isize - (branch as isize + 1);
        words.push(0xBF85_0000 | u32::from(back as i16 as u16));
        epilogue(&mut words, 4, &[(13, 0)]);
        words
    }

    fn program_critical(wide: bool) -> Vec<u32> {
        let mut words = prologue(wide);
        words.extend(mask(17));
        words.push(0xBF06_0080);
        let branch = words.len();
        words.push(0);
        words.extend(bvh_words(wide, 13));
        words.push(0xBF82_0000);
        let join = words.len();
        words[branch] = 0xBF85_0000 | u32::from((join as isize - (branch as isize + 1)) as u16);
        words.extend(bvh_words(wide, 17));
        epilogue(&mut words, 5, &[(13, 0), (17, 16)]);
        words
    }

    fn native(words: &[u32]) -> NativeProgram {
        native_wave(words, false, 32, 22)
    }

    fn native_wave(words: &[u32], wave64: bool, threads: u32, vgprs: u16) -> NativeProgram {
        let table = |kind, at| UsedResource {
            kind,
            root: RootType::Table,
            table_offset_dwords: at,
            lower_bound: 0,
            stride: 4,
            dwords: 4,
            user_data: 4,
            slot: 1,
        };
        NativeProgram {
            stage: HwStage::Cs,
            wave64,
            vgprs,
            layout: vec![
                RootParam {
                    root: RootType::Constants,
                    table_offset_dwords: 0,
                    sgprs: 4,
                    base_register: 0,
                    user_data: 0,
                    slot: 0,
                },
                RootParam {
                    root: RootType::Table,
                    table_offset_dwords: 0,
                    sgprs: 1,
                    base_register: 0,
                    user_data: 4,
                    slot: 1,
                },
            ],
            used: vec![table(ResourceType::Srv, 0), table(ResourceType::Uav, 8)],
            io: StageIo::Cs(ComputeIo {
                rsrc2: 5 << 1,
                threads: [threads, 1, 1],
            }),
            code: words.iter().flat_map(|word| word.to_le_bytes()).collect(),
        }
    }

    fn lower_native_with(
        native: &NativeProgram,
        profile: crate::emit::ShaderProfile,
    ) -> Result<Lowered, crate::ShaderError> {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, native).expect("io::raise");
        lower_with_profile(
            &Parts {
                native,
                p: &p,
                st: &st,
                io: &io,
                res: &res,
                inputs: Vec::new(),
                outputs: Vec::new(),
                prims: Vec::new(),
                mesh_stride: 0,
                mesh_fallback: false,
            },
            profile,
        )
    }

    fn lower_native(native: &NativeProgram) -> Result<Lowered, crate::ShaderError> {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st))?;
        let io = io::raise(&p, native).expect("io::raise");
        lower(&Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
            mesh_fallback: false,
        })
    }

    fn lower_program(wide: bool) -> Result<Lowered, crate::ShaderError> {
        lower_native(&native(&program(wide)))
    }

    fn shader_for(
        native: &NativeProgram,
        profile: RayProfile,
    ) -> (HostShader, resource::Resources) {
        let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
        let st = structure::structure(&p).expect("structure");
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("tracked");
        let io = io::raise(&p, native).expect("io::raise");
        let was = TEST_BVH_PROFILE.replace(Some(profile));
        let lowered = lower(&Parts {
            native,
            p: &p,
            st: &st,
            io: &io,
            res: &res,
            inputs: Vec::new(),
            outputs: Vec::new(),
            prims: Vec::new(),
            mesh_stride: 0,
            mesh_fallback: false,
        })
        .expect("lowers");
        TEST_BVH_PROFILE.set(was);
        let shader = HostShader {
            stage: HwStage::Cs,
            profile: lowered.profile,
            descriptor_aliases: lowered.descriptor_aliases(),
            direct_heap: lowered.direct_heap,
            dxil: lowered.dxil,
            mesh_fallback: None,
            user_data: lowered.user_data,
            samplers: lowered.samplers,
            static_samplers: Vec::new(),
            root_cbvs: lowered.root_cbvs,
            root_srvs: lowered.root_srvs,
            root_uavs: lowered.root_uavs,
            root_indexed_views: res.root_indexed_views.clone(),
        };
        (shader, res)
    }

    fn dxil_hash(bytes: &[u8]) -> u64 {
        bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |hash, &b| {
            (hash ^ u64::from(b)).wrapping_mul(0x1000_0000_01b3)
        })
    }

    #[cfg(windows)]
    fn validate(lowered: &Lowered) {
        if let Ok(mut validator) = crate::emit::validator::Validator::load(
            crate::emit::validator::Validator::default_path().as_deref(),
        ) {
            let mut dxil = lowered.dxil.clone();
            validator
                .validate(&mut dxil)
                .expect("DXIL validation failed");
        }
    }

    #[cfg(not(windows))]
    fn validate(_: &Lowered) {}

    #[test]
    fn deterministic_misses_match_the_current_node_result_abi() {
        assert_eq!(TRIANGLE_MISS, bvh::MISS.map(f32::to_bits));
        for kind in 0..8u32 {
            let expected = if matches!(kind, 4 | 5) {
                BOX_MISS
            } else {
                TRIANGLE_MISS
            };
            assert_eq!(miss_words(0x1234_5000 | kind), expected, "kind {kind}");
            assert_eq!(miss_words(0xFEDC_BA80 | kind), expected, "kind {kind}");
        }
    }

    #[test]
    fn static_miss_lowering_never_enters_the_real_node_or_fault_paths() {
        for wide in [false, true] {
            TEST_BVH_REAL_CALLS.set(0);
            let full = lower_program(wide).expect("full lowering");
            assert_eq!(TEST_BVH_REAL_CALLS.get(), 1);
            assert!(full.root_srvs.contains(&SLOT_MEMORY));
            assert!(full.root_uavs.contains(&SLOT_FAULTS));
            let mode = TestMode::new(RayProfile::StaticMiss);
            let static_miss = lower_program(wide).expect("static-miss lowering");
            validate(&static_miss);
            assert_eq!(TEST_BVH_REAL_CALLS.get(), 0, "wide {wide}");
            assert!(!static_miss.root_srvs.contains(&SLOT_MEMORY), "wide {wide}");
            assert!(!static_miss.root_uavs.contains(&SLOT_FAULTS), "wide {wide}");
            drop(mode);
        }
    }

    #[cfg(windows)]
    fn run_two_on_card(
        words: &[u32],
        profile: RayProfile,
        flags: &[(u32, u32)],
    ) -> Option<(Vec<u32>, [u32; 4])> {
        use crate::emit::host::{Dispatch, Host, Root, Stride, View};
        let _one = crate::emit::valu_tests::ONE_AT_A_TIME
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        const THREADS: usize = 32;
        const RECORD: usize = 16;
        let ray = bvh::Ray::new([-10.0, 0.0, 0.0], [1.0, 0.0, 0.0], 100.0);
        let boxes = [[-1.0f32, -1.0, -1.0, 1.0, 1.0, 1.0]; 4];
        let node = bvh::tests::box32([0x1005, 0x2005, 0x3005, 0x4005], boxes);
        let expected = bvh::intersect(&node, 5, &ray, true);
        let mut data = vec![0u32; THREADS * RECORD + 28];
        let pointer = ((THREADS * 64) as u32 >> 3) | 5;
        for t in 0..THREADS {
            let r = &mut data[t * RECORD..][..RECORD];
            r[0] = pointer;
            r[2] = ray.extent.to_bits();
            for a in 0..3 {
                r[3 + a] = ray.origin[a].to_bits();
                r[6 + a] = ray.direction[a].to_bits();
                r[9 + a] = ray.inverse[a].to_bits();
            }
            (r[12], r[13]) = flags.get(t).copied().unwrap_or((0, 0));
        }
        data[THREADS * RECORD..].copy_from_slice(&node);
        let native = native_wave(words, false, THREADS as u32, 23);
        let (shader, _) = shader_for(&native, profile);
        let host = match Host::new(false) {
            Ok(host) => host,
            Err(e) => {
                eprintln!("SKIP no hardware device: {e}");
                return None;
            }
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("compute pipeline");
        let aliases = host.compute_alias_parameters(&shader);
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    roots: vec![(0, Root::Constants(vec![0; 5]))],
                    aliases: &aliases,
                    views: &[
                        View::Buffer {
                            data: &data,
                            stride: Stride::Raw,
                        },
                        View::RawUav(8 * THREADS as u32),
                    ],
                    samplers: &[],
                    groups: [1, 1, 1],
                },
            )
            .expect("dispatch");
        Some((out.into_iter().next().expect("a UAV"), expected))
    }

    #[cfg(windows)]
    #[test]
    fn limited_budget_exhausts_after_exactly_n_tests_on_the_card() {
        let flags = [(1, 1); 32];
        let Some((out, expected)) =
            run_two_on_card(&program_two(false), RayProfile::Limited(1), &flags)
        else {
            return;
        };
        for t in 0..32 {
            assert_eq!(&out[8 * t..8 * t + 4], &expected, "thread {t} first");
            assert_eq!(&out[8 * t + 4..8 * t + 8], &BOX_MISS, "thread {t} second");
        }
        for (profile, second) in [
            (RayProfile::Limited(2), expected),
            (RayProfile::Full, expected),
            (RayProfile::StaticMiss, BOX_MISS),
        ] {
            let Some((out, expected)) = run_two_on_card(&program_two(false), profile, &flags)
            else {
                return;
            };
            let first = if profile == RayProfile::StaticMiss {
                BOX_MISS
            } else {
                expected
            };
            for t in 0..32 {
                assert_eq!(
                    &out[8 * t..8 * t + 4],
                    &first,
                    "{profile:?} thread {t} first"
                );
                assert_eq!(
                    &out[8 * t + 4..8 * t + 8],
                    &second,
                    "{profile:?} thread {t} second"
                );
            }
        }
    }

    #[cfg(windows)]
    #[test]
    fn a_predicated_off_test_does_not_spend_the_budget_on_the_card() {
        let flags = [(0, 1); 32];
        let Some((out, expected)) =
            run_two_on_card(&program_two_flags(false), RayProfile::Limited(1), &flags)
        else {
            return;
        };
        for t in 0..32 {
            assert_eq!(&out[8 * t..8 * t + 4], &[1, 2, 3, 4], "thread {t} first");
            assert_eq!(
                &out[8 * t + 4..8 * t + 8],
                &expected,
                "thread {t} second keeps the budget"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn limited_budget_bounds_a_loop_on_the_card() {
        let flags = [(1, 1); 32];
        let Some((out, _)) = run_two_on_card(&program_loop(false), RayProfile::Limited(1), &flags)
        else {
            return;
        };
        for t in 0..32 {
            assert_eq!(&out[4 * t..4 * t + 4], &BOX_MISS, "thread {t}");
        }
        let Some((out, expected)) =
            run_two_on_card(&program_loop(false), RayProfile::Limited(2), &flags)
        else {
            return;
        };
        for t in 0..32 {
            assert_eq!(&out[4 * t..4 * t + 4], &expected, "thread {t}");
        }
    }

    #[test]
    fn full_profile_dxil_is_unchanged() {
        let _mode = TestMode::new(RayProfile::Full);
        for (wide, len, hash) in [
            (false, 6668, 0x26f8_7f80_b6b1_e72c),
            (true, 6692, 0x09f7_3bdc_1831_ccfd),
        ] {
            let lowered =
                lower_native_with(&native(&program(wide)), crate::emit::ShaderProfile::Sm66)
                    .expect("full lowering");
            validate(&lowered);
            assert_eq!(lowered.dxil.len(), len, "wide {wide}");
            assert_eq!(dxil_hash(&lowered.dxil), hash, "wide {wide}");
            assert!(lowered.bvh_budget_phi_blocks.is_empty());
            assert!(lowered.bvh_budget_edges.is_empty());
            assert_eq!(lowered.bvh_limited_sites, 0);
        }
    }

    #[test]
    fn limited_profile_has_a_phi_at_each_reachable_block_entry() {
        for (wave64, threads, words) in
            [(false, 32, program(false)), (true, 64, program_plain(true))]
        {
            let _mode = TestMode::new(RayProfile::Limited(4));
            let native = native_wave(&words, wave64, threads, 22);
            let lowered = lower_native(&native).expect("limited lowering");
            validate(&lowered);
            let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
            let order = ir::reverse_postorder(&p.blocks);
            let mut got = lowered.bvh_budget_phi_blocks.clone();
            got.sort_unstable();
            let mut want = order[1..].to_vec();
            want.sort_unstable();
            assert_eq!(got, want, "wave64={wave64}");
            assert_eq!(lowered.bvh_limited_sites, 1);
        }
    }

    #[test]
    fn limited_profile_lowers_sequential_calls_on_one_budget() {
        let _mode = TestMode::new(RayProfile::Limited(1));
        let lowered = lower_native(&native(&program_two(false))).expect("limited lowering");
        validate(&lowered);
        assert_eq!(lowered.bvh_limited_sites, 2);
        assert!(lowered.bvh_budget_phi_blocks.is_empty());
    }

    #[test]
    fn limited_profile_carries_a_loop_header_phi_from_the_back_edge_latch() {
        let _mode = TestMode::new(RayProfile::Limited(1));
        let lowered = lower_native(&native(&program_loop(false))).expect("limited loop");
        validate(&lowered);
        assert_eq!(lowered.bvh_limited_sites, 1);
        let back = lowered
            .bvh_budget_edges
            .iter()
            .find(|edge| edge.back)
            .expect("a back edge");
        assert!(
            lowered
                .bvh_budget_edges
                .iter()
                .any(|edge| edge.to == back.to && !edge.back),
            "the header phis take a forward entry too"
        );
    }

    #[test]
    fn limited_profile_splits_a_critical_edge_to_a_budget_phi() {
        let _mode = TestMode::new(RayProfile::Limited(1));
        let lowered = lower_native(&native(&program_critical(false))).expect("critical edge");
        validate(&lowered);
        assert_eq!(lowered.bvh_limited_sites, 2);
        let critical = lowered
            .bvh_budget_edges
            .iter()
            .find(|edge| edge.critical)
            .unwrap_or_else(|| panic!("{:?}", lowered.bvh_budget_edges));
        assert_ne!(critical.pred, critical.to);
    }
}
