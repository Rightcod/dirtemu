# dirtemu

Experimental **AMD RX 570 / Polaris** compatibility fork of
[ztrx](https://github.com/Zettrox/ztrx) for running the Xbox Series build of
GTA VI on Windows PCs with older AMD graphics cards.

**This repository contains no game files or assets.** You need your own copy
of the game. Not affiliated with or endorsed by its developer or publisher.

## Status (2026-11-25)

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
| Series-S memory profile | in progress; title/console budget and resource caps being wired |
| Barycentrics / `attributeAtVertex` | **not solved on RX 570**; 346 real PS affected |
| Full game boot / world / FPS | **not verified**; game package was incomplete during development |

Known hard blockers:

- RX 570 has no hardware barycentrics support; 346 pixel shaders use
  `SV_Barycentrics` and/or `attributeAtVertex`.
- 16 GiB RAM is below the practical working set observed in related forks;
  a large pagefile is required and performance will still be limited.
- The inspected game package lacked a complete executable/data set during
  development, so end-to-end boot and world rendering have not been proven.
- Some HLE imports are still missing or need title-DLL/host routing.

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

For RX 570 / Polaris experiments:

```powershell
$env:VICTORY_SHADER_PROFILE = "auto"
$env:VICTORY_MESH_FALLBACK = "on"
$env:VICTORY_BVH_PROFILE = "low"
$env:VICTORY_AMBIENT_FALLBACK = "auto"
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
| `VICTORY_D3D12_DRED=1` | report commands in flight on GPU failure |
| `VICTORY_GPU_TIMING=1` | GPU timing diagnostics (use with DRED/diagnostics build) |

## Documentation

- `docs/architecture.md` — translation layer architecture
- `docs/mesh-fallback.md` — compute mesh fallback subset and scheduling
- `docs/fork-ray-tracing.md` — BVH/RT translation status
- `docs/asset-policy.md` — no game assets in this repository

## License

GPL-3.0-or-later (see `LICENSE`). Based on VIctory (Marvisak),
victory-fork (fxceless), and ztrx (Zettrox).