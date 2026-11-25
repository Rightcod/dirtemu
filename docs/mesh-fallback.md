# Mesh shaders without MeshShaderTier

`VICTORY_MESH_FALLBACK=auto` is the default. A device without mesh-shader
support translates mesh programs differently; `off` keeps the hardware-only
path and `on` forces the fallback even where mesh shaders exist. The choice is
part of the translated-shader cache key. Compute and draw PSOs use separate
`mesh compute fallback v1` / `mesh draw fallback v1` library keys. The shader
cache encoding is `VSH2`, so older entries cannot be mistaken for this path.

## Vertical slice

A mesh program is emitted as a compute shader with its original thread-group
shape (up to 128 threads), group-shared memory, resource bindings, lane model,
and output exports. Each group writes one fixed-size record into a per-list,
per-pipeline UAV buffer:

- `D3D12_DRAW_ARGUMENTS`, with the clamped `primitive_count * 3` vertices,
  one instance and the group number as `StartInstanceLocation`;
- three 32-bit vertex indices per triangle at byte 16 of the record, so the
  index for corner `c` of primitive `p` is the dword at
  `16 + 4 * (3 * p + c)` (culled primitives become 0, 0, 0);
- all per-vertex outputs as 32-bit components;
- all per-primitive outputs as 32-bit components.

The allocation request (`s_sendmsg`, m0 = vertices | primitives << 12) writes
the real dynamic primitive count. It is clamped to the declared maximum to
keep malformed input inside the record. Vertex and primitive exports are raw
buffer stores rather than mesh-output intrinsics. A generated SM vertex shader
uses `SV_VertexID` and `SV_InstanceID`, pulls the selected index and outputs,
and feeds the original pixel shader through ordinary VS/PS semantics. The host
inserts UAV barriers between the compute and draw work; no CPU readback is
needed.

A direct fallback dispatch is one compute `Dispatch(groups, 1, 1)` followed by
one `DrawInstanced(max_primitives * 3, groups, 0, 0)`: the vertex shader takes
the group from `SV_InstanceID` and the primitive and corner from
`SV_VertexID = 3 * primitive + corner`, so every group is drawn in the same
instanced draw without `ExecuteIndirect` and without per-group chunks. Direct
dispatches are limited to 65,535 one-dimensional groups; their workspace is
`groups * group_bytes` bytes and cannot be reused until the barriers order the
draw after the compute.

Indirect mesh execution stays a separate path: one record without a count
buffer and up to 1,024 groups. Its compute command and its draw commands both
go through `ExecuteIndirect`; the draw reads each group record's draw
arguments, and the workspace is always `1024 * group_bytes` bytes.

## Supported subset and diagnostics

This is deliberately not advertised as a universal mesh/amplification
implementation:

- triangle topology only, at most 128 threads, 256 vertices and 256 primitives;
- at most 32 combined VS output rows, and no duplicate vertex/primitive
  semantic (these fail translation with a named reason);
- dynamic primitive counts and primitive culling are supported; vertices and
  outputs follow the existing console lane/export model;
- cross-lane code uses the compute translator's exact wave-size machinery.
  Programs the existing wave64 lowering cannot represent are refused rather
  than silently run with a different wave layout;
- indirect mesh execution supports one record without a count buffer and up
  to 1,024 groups. A larger group request is deterministically converted to
  an empty record by the argument clamp; multi-record and count-buffer forms
  are refused with diagnostics. A warning names this limit once;
- amplification shaders, `DispatchMeshX` with a GPU-memory count/payload,
  line/point topology, and geometry beyond these limits remain unsupported;
- the rest of the translated program keeps its existing feature requirements
  (in particular, the current SM 6.6 binding model). The fallback itself does
  not lower the device's shader-model requirement.

The hardware `DispatchMesh` and mesh PSO path is unchanged when selected.
