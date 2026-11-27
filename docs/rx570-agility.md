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
