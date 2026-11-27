# dirtemu

Experimental **AMD RX 570 / Polaris** compatibility fork of
[ztrx](https://github.com/Zettrox/ztrx) for running the Xbox Series build of
GTA VI on Windows PCs with older AMD graphics cards.

**This repository contains no game files or assets.** You need your own copy
of the game. Not affiliated with or endorsed by its developer or publisher.

## Status (2026-11-27)

Work in progress. **Not a playable release yet.**

Target test machine:

- Windows 10 x64
- Intel Xeon E3-1240 v3 (4C/8T, AVX2)
- 16 GiB RAM
- AMD Radeon RX 570 8 GiB (Polaris, SM 6.5, wave64, no mesh shaders, no DXR)

What is already implemented and tested at shader/PSO level:

| Area | Status |
|---|---|
| SM 6.5 shader translation | implemented; the 22 baseline SM 6.6 GPU failures pass on RX 570 |
| SM 6.5 descriptor tables | implemented; alias-table PSO/dispatch tests pass on RX 570 |
| Mesh shader fallback | compute emulation + vertex pulling + one `DrawInstanced`; 9 279 / 9 282 real mesh shaders translate |
| Amplification shaders | none found in the inspected shader package |
| BVH / RT profiles | `Full`, `StaticMiss`, `Limited(N)` implemented and tested on RX 570 |
| Shadowmask PSV fix | typed-UAV PSV ordering fixed; post/shadow validation improved to 97/104 |
| Ambient fallback | constant indirect-light fill after ScreenProbes temporal resolve, Polaris default 0.2 |
| BC3 texture fallback | five unaligned BC3 textures decoded to RGBA8 with original dimensions |
| Series-S memory profile | title memory, host-copy, texture and staging caps active |
| Barycentrics / `attributeAtVertex` | SM 6.5 software path through a geometry stage; ordinary triangles, rectangle lists and compute mesh fallback supported |
| Integer render targets | blending normalized so D3D12 accepts the pipeline |
| Conservative rasterization | ordinary raster coverage on unsupported devices; conservative edge coverage is not reproduced |
| Streamed textures | upload decisions follow staging refresh, so new data reaches the same submission; partial uploads retry by elapsed time |
| Memory pressure | reclaim unused staging buffers; configurable sampled-texture budget; root-window tails reduce duplicate data |
| Shared constants | optional direct GPU access to the title's constants section, with ordered indirect argument preparation |
| Precompilation | package shaders and every recorded pipeline state; translator-keyed disk cache preserved across runtime-only rebuilds |
| Full game boot / world / FPS | boot, menus and world image confirmed in earlier runs; new rendering fixes still need an in-game comparison after full precompilation |

Latest validation on the RX 570:

- **53/53 selected problem pipeline states** now create successfully with the
  D3D12 debug layer, with no refused or unreadable states. Unsupported hardware
  barycentrics and conservative rasterization caused their earlier failures.
- GPU readback tests verify perspective and linear barycentric weights with
  unequal clip-space W, per-vertex attributes, and both halves of rectangle lists.
- A texture changed after command-list recording reaches the GPU in that same
  submission. Ten GPU regression checks and nine interpolation checks passed.

Known limitations:

- The previous complete shader-archive pass prepared **24,907 translations**
  and refused 982 records. Of those refusals, 679 require 64-bit image atomics
  from SM 6.6 and 195 contain an unsupported shader stage. Precompilation does
  not supply missing instruction support.
- The recorded set contains **83,562 pipeline states**. The first full warm
  reached 17,792 states before the barycentrics correction; the full pass with
  the final translator must finish before the next game run. Preparation of
  every state is not yet a verified result.
- 16 GiB RAM is below the practical working set observed in related forks;
  a large pagefile is required and performance will still be limited.
- Shader translation and driver pipeline refusals still omit rendering passes.
- CPU preparation, demand paging and buffer read-back cause long frame times.
- Missing geometry, low-detail textures and watchdog failures in the previous
  game runs are not yet claimed to be fully fixed. No new gameplay FPS result
  is available for these changes.

## Requirements

- Windows 10/11 x64, a CPU with AVX2
- Direct3D 12 GPU
  - hardware path: shader model 6.6 + mesh shaders
  - RX 570 / Polaris path: shader model 6.5 + compute mesh fallback
- Rust (the version in `rust-toolchain.toml`)
- Several GiB of free Windows commit memory; for world loading a large
  pagefile is recommended (32 GiB or more on SSD)

## Build

```powershell
cargo build --release -p victory
```

Diagnostics build:

```powershell
cargo build --profile diag -p victory --features diagnostics
```

## Play

```powershell
powershell -ExecutionPolicy Bypass -File scripts\play.ps1 -Game "<your game folder>\<the game's exe>"
```

The launcher waits for shader translation and recorded D3D12 pipelines before
starting the title. The first run scans the package's shader archives and
DLLs. It records pipeline states when the title creates them. Later runs
prepare those exact states before the title starts. Keep the cache directory
on the same drive across runs. Pipeline recipes are specific to the game
build and console profile. Shader and driver refusals appear in the summary.

To warm the cache without starting the game, use the same console profile and
environment as the launcher.

```powershell
.\target\release\victory.exe precompile "<your game folder>\<the game's exe>" --console series-x --jobs 3
```

Add `--used-only` to build only states used in previous runs.
Without that option, every recorded state is attempted. Use the same release
binary, cache, console profile and shader settings for preparation and play.
Failure details are stored in `shader-scan/*.failures.txt` in the configured
cache. Shader/pipeline preparation is separate from the game's asset streaming.
Translations are keyed by the translator sources, dependencies, target GPU
and settings. Rebuilding rendering or runtime code preserves them.
The pipeline recording and source fingerprint follow the approach in
[carlm125/cool_fox](https://github.com/carlm125/cool_fox).

For RX 570 / Polaris experiments:

```powershell
$env:VICTORY_SHADER_PROFILE = "auto"
$env:VICTORY_MESH_FALLBACK = "on"
$env:VICTORY_BVH_PROFILE = "auto"
$env:VICTORY_AMBIENT_FALLBACK = "auto"
$env:VICTORY_MEMORY_PROFILE = "series-s"
$env:VICTORY_TEXTURE_MB = "1536"
$env:VICTORY_WINDOW_TAILS = "1"
$env:VICTORY_PORT_CONSTANTS = "1"
```

Memory profile work is exposed through the console/profile configuration;
see `victory.example.toml` and `docs/architecture.md`.

## Important switches

| Switch | Effect |
|---|---|
| `VICTORY_SHADER_PROFILE=auto/sm65/sm66` | select shader translation profile; auto uses device capability |
| `VICTORY_MESH_FALLBACK=auto/off/on` | select mesh emulation; auto uses it when mesh shaders are unavailable |
| `VICTORY_BVH_PROFILE=auto/full/off/low/static-miss/limited:N` | BVH/RT workload profile; `low` maps to `StaticMiss`, and `auto` selects it on RX 570 |
| `VICTORY_BVH_BUDGET=N` | numeric budget for `limited` |
| `VICTORY_AMBIENT_FALLBACK=off/auto/0.0..1.0` | constant indirect-light fallback; auto enables 0.2 on Polaris |
| `VICTORY_TEXTURE_MB=1536` | sampled-texture budget in MiB, bounded to one quarter of physical VRAM; launcher default for the 8 GiB RX 570 |
| `VICTORY_PORT_CONSTANTS=1` | GPU reads and writes a contiguous low-address constants section in place; falls back to copies if opening it fails |
| `VICTORY_WINDOW_TAILS=1` | omit unused prefixes of root-view memory windows |
| `VICTORY_CARD_BUFFERS_MB=768` | launcher budget for buffer copies held in VRAM |
| `VICTORY_D3D12_DRED=1` | report commands in flight on GPU failure |
| `VICTORY_GPU_TIMING=1` | GPU timing diagnostics (use with DRED/diagnostics build) |

## Documentation

- `docs/architecture.md` — translation layer architecture
- `docs/mesh-fallback.md` — compute mesh fallback subset and scheduling
- `docs/fork-ray-tracing.md` — BVH/RT translation status
- `docs/asset-policy.md` — no game assets in this repository
- [AMD fork comparison and RX 570 fixes](docs/amd-comparison-2026-11-27.md)

## AMD work and credits

This fork adapts integer-target blending and pixel quad reads from
[Belobug/VIctory-Belobug-AMD](https://github.com/Belobug/VIctory-Belobug-AMD),
and shared constants, ordered indirect preparation and submission-time texture
uploads from [avatarmanz/VIctory-7900XTX](https://github.com/avatarmanz/VIctory-7900XTX).
The SM 6.5 barycentrics geometry path is additional Polaris work in this fork.
The comparison document records the source revisions, tests and remaining gaps.

## License

GPL-3.0-or-later (see `LICENSE`). Based on VIctory (Marvisak),
victory-fork (fxceless), and ztrx (Zettrox).
