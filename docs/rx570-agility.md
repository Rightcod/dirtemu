# RX 570 Shader Model and app-local DirectX

Author: carunodev

Measured on 2026-11-27, Windows 10 Pro 22H2, Radeon RX 570 (1002:67df),
AMD driver 31.0.21925.1001. These are capability and synthetic GPU results;
they do not establish gameplay performance or complete rendering correctness.

| Check | System runtime | Agility 1.620.2 |
|---|---|---|
| Shader Model 6.6 request | succeeds, returns 6.5 | succeeds, returns 6.6 |
| Shader Model 6.7 request | E_INVALIDARG | succeeds, returns 6.7 |
| Feature Level | 12_0 | 12_0 |
| Wave width | 64 | 64 |
| Typed / descriptor-heap atomic64 | feature query unavailable | both supported |
| Barycentrics | unsupported | unsupported |
| Mesh shaders | unsupported | unsupported |
| Conservative rasterization | unsupported | unsupported |
| ID3D12Device10 | unavailable | available |

The driver and hardware were identical in both runs. The system D3D12Core
fixed-file version was 6.2.19041.5794; the app-local core was 1.620.2.0.
Both app-local DLLs have valid Microsoft Corporation Authenticode signatures.
The SDK is loaded through exports from the main executable, following
[Microsoft's Agility integration](https://devblogs.microsoft.com/directx/gettingstarted-dx12agility/).
No system DLL, driver or Windows setting was changed.

The translator emits at most SM 6.6; SM 6.7 reporting does not make its output
SM 6.7. `auto` selects SM 6.6 when the created device accepts it. The host
separately queries barycentrics and typed/descriptor-heap atomic64 support.
Unsupported barycentrics continue to use the software geometry path even with
SM 6.6. These capability bits are part of translation and preparation keys.
An unrecognized SM 6.7 query falls back to the SM 6.6 query for system runtimes.

## Actual dispatch results

A synthetic SM 6.6 compute shader accesses an R32G32_UINT texture as
`RWTexture2D<uint64_t>` through `ResourceDescriptorHeap`. The test initializes
it to 0x00000000fffffff0 and adds 0x0000000100000001 per invocation.
It waits for a GPU fence and reads back the texture and returned originals.

| Variant, 64 invocations | Result on this driver |
|---|---|
| One thread per group, 64 groups, same texel | correct |
| 64 lanes, each addressing its own texel | correct |
| 64 lanes, dynamic `id.x / 64` coordinate, same texel | correct |
| Dynamic coordinate, all returned originals checked | correct |
| Explicit serialization across wave lanes | correct |
| Serialized lanes, all returned originals checked | correct |
| Literal coordinate, all lanes addressing texel zero | one update instead of 64 |
| Literal coordinate and `globallycoherent` | one update instead of 64 |
| Literal coordinate with atomic32 | one update instead of 64 |

The successful collision cases end at 0x0000004100000030. Their returned
originals are all 64 expected unique values, with zero mismatches and no
device removal. Returning the original value does not fix the literal-address
failure. The observed difference suggests driver/compiler lowering of a
statically uniform address; the internal driver instructions were not inspected.
This defect is distinct from missing SM 6.6 support. The dynamic-coordinate
probe does not prove every translated game atomic shader correct.

## Complete archive translation

The SM 6.6 preparation processed all 26,503 archive records: 25,586 ready,
303 refused and 614 duplicates. The SM 6.5 result had been 24,907 ready and
982 refused. All 679 former SM 6.6 image-atomic refusals therefore translate
with the real device capabilities enabled.

Remaining refusals include 195 records without a supported executable stage,
40 descriptor-control-flow cases, 40 descriptor-data reads, ten hardware SGPR
inputs, eight partial interpolations and five float64-to-float32 conversions.
The remainder concerns a few lane-mask, half-float and permutation operations.
These are translator gaps; a higher reported Shader Model does not implement
them. Real PSO creation and game rendering remain separate validation steps.

## Build and cache

`cargo build --release -p victory --features agility` exports SDK version 620
and the path `.\D3D12\`. `scripts/setup-agility.ps1` obtains the official
Microsoft NuGet package, validates its pinned hash and signatures, and copies
the core and matching debug layer into that directory. DLLs and package caches
are not tracked by Git.

Switching from system SM 6.5 to Agility SM 6.6 changes translation identity.
The first preparation with the new profile must finish before launching the
game. Later preparations reuse compatible translations and checkpointed
pipeline states. Driver cache acceptance is checked before trusting pipeline
preparation progress.

## Wave32 compute groups on a wave64 GPU

Agility correctly rejects a compute shader declaring `WaveSize(32)` on this
device, whose minimum and maximum wave size are both 64. Eight selected refused
game PSOs all had exactly 32 threads per group (`32x1x1` or `8x4x1`). One used
5,120 bytes of LDS; none contained a group barrier.

For logical wave32 groups containing at most 32 threads on a minimum-wave64
device, the emitter now declares physical wave64 in metadata and PSV. It keeps
logical thread relabeling and wave arithmetic unchanged. A physical wave contains
only that group's active threads, so no second logical wave can contaminate its
collectives. Groups larger than 32 are excluded from this conversion.

All eight selected PSOs now create with the debug layer: eight ready, zero
refused. Four independent GPU readback tests cover both group shapes, with and
without LDS and a group barrier, plus ballots, reductions, prefixes, lane reads
and divergence. Every expected value matches, with no device removal. The
metadata boundary tests also verify that larger groups keep their original
declaration.

A census of 19,106 observed containers found 1,464 native-wave32 compute
entries in groups of at most 32, and 107 in larger groups. The larger group
case requires separate subwave emulation: 12 entries use explicit cross-lane
operations and 67 use read-first-lane. It is not covered by this small-group
correction, even when a particular PSO creates successfully. The census covers
observed containers; completeness of recipe references was not asserted.
