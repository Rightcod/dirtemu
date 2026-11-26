# RX 570 progress — 2026-11-26

Status: initialization, visible menus and entry into Game Running work on the tested machine. World rendering and stability are not yet working reliably; the observed world is black at roughly 700 ms per frame, with later driver faults still under investigation. The presentation counter is not evidence of playable world frame rates.

## Compatibility fixes

- Strip the bindless heap flags from serialized title root signatures. The system serializer rejects those flags with E_INVALIDARG; translated host pipelines build their own root signatures separately. This fixes the NULL lod_lights program table and the Game Init access violation at image RVA 0xb5a5f8.
- Composite the scene and interface planes passed to retail PresentX, including the 72-byte plane layout and scaled premultiplied alpha. Previously only the first plane was shown. The interface was already correct in its render target while the presented image was black.
- Decode BC3 with a four-colour palette for both endpoint orders, including equal endpoints.
- Keep console CPU instruction emulation installed when VICTORY_CRASH_REPORT=0 disables crash output.
- Add the GUID functions requested through rpcrt4, the retail console identifier, optional title frame-rate settings and opt-in XMem tracing retained from the preceding investigation.
- Remove the VICTORY_DISABLE_UPSCALER DLL-loading block. The title still uses FSR when its provider is refused; its jitter phase query leaves zero and the first dispatch divides by zero at image RVA 0x28f03e8. With the provider loaded, the observed 1280-to-1920 query returns 18 phases and a non-NULL context.
- Create capture directories and support RGBA/BGRA typeless readbacks.
- Defer output-merger targets until the graphics pipeline is known, and bind its depth view only when depth or stencil is enabled. The D3D12 debug layer identified a 1920x1080 RTV paired with an unused 1280x720 DSV as a device-removing invalid call.
- Copy the ambient light UAV into the shader-visible descriptor heap before clearing it. The CPU and GPU clear descriptors must be identical.
- Use RGBA16_FLOAT for R9G9B9E5 render/UAV targets on adapters that cannot write the packed HDR format, with consistent pipeline and view formats.
- Preserve output register rows and component masks in the mesh fallback vertex wrapper. Repacking every semantic into a new register breaks linkage with the pixel shader. Overlapping vertex/primitive outputs remain explicitly unsupported.
- Keep launcher caches, data, logs and TEMP/TMP on the repository drive by default.

The compositor and related retail-plane handling were adapted from the Pareto snapshot in caruno-git/Americas-Shim. RPC and frame-rate work were adapted from the local cool_fox and VIctory-7900XTX references. Existing source attribution and the repository licence apply.

## Validation

- Release and diagnostics builds succeed with the locked toolchain and dependencies.
- Four root-signature tests pass against the system serializer.
- Four BC3 decoder tests pass.
- The display compositor GPU test reads back the expected transparent, half-transparent and opaque pixels after scaling.
- The console-instruction execution test passes with crash reporting disabled.
- Five further regressions pass on the RX 570 with the D3D12 debug layer: mismatched unused depth targets, depth/stencil enable flags, ambient clear pixel readback, typed format capabilities, and HDR RTV/UAV creation.
- The game reaches its settings UI and loading screens with two-plane composition active.
- Seven mesh fallback tests pass, including packed TEXCOORD components, system semantics, emitted storeOutput semantic IDs and vertex/primitive collisions.
- After the output-merger and clear fixes, the game entered Game Running and stayed alive for about ten minutes with no recurrence of D3D12 validation IDs 728/646/599/38. It then crashed in the AMD driver while recording CopyTextureRegion. The new mesh-wrapper fix still needs a full game run.
- The Series S profile reaches its 2.5 GiB host-copy and 512 MiB streamed-texture budgets and refuses further resources. The tested run had about 1 GiB system RAM available, substantial paging and roughly 14 GiB process private commit. These observations do not prove VRAM exhaustion.

## Running

Build with `cargo build --release -p victory --locked`, then use `scripts/play.ps1 -Game <path-to-game-executable>`. The launcher does not inject keys or automatically terminate the process. Use external cache and data directories on a drive with sufficient free space.

The tested machine has a Xeon E3-1240 v3, 16 GiB RAM and RX 570 8 GiB under Windows 10. Series S memory limits, SM 6.5 translation and compute mesh fallback are active. Initial scene setup also causes extensive shader and driver pipeline compilation. These results do not establish a playable port.
