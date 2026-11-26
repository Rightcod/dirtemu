# RX 570 progress — 2026-11-26

Status: initialization and visible menus work on the tested machine. World rendering and stability are not yet working reliably; a black screen, long stalls and the title watchdog are still under investigation. The presentation counter is not evidence of playable world frame rates.

## Compatibility fixes

- Strip the bindless heap flags from serialized title root signatures. The system serializer rejects those flags with E_INVALIDARG; translated host pipelines build their own root signatures separately. This fixes the NULL lod_lights program table and the Game Init access violation at image RVA 0xb5a5f8.
- Composite the scene and interface planes passed to retail PresentX, including the 72-byte plane layout and scaled premultiplied alpha. Previously only the first plane was shown. The interface was already correct in its render target while the presented image was black.
- Decode BC3 with a four-colour palette for both endpoint orders, including equal endpoints.
- Keep console CPU instruction emulation installed when VICTORY_CRASH_REPORT=0 disables crash output.
- Add the GUID functions requested through rpcrt4, the retail console identifier, optional title frame-rate settings and opt-in XMem tracing retained from the preceding investigation.
- Remove the VICTORY_DISABLE_UPSCALER DLL-loading block. The title still uses FSR when its provider is refused; its jitter phase query leaves zero and the first dispatch divides by zero at image RVA 0x28f03e8. With the provider loaded, the observed 1280-to-1920 query returns 18 phases and a non-NULL context.
- Create capture directories and support RGBA/BGRA typeless readbacks.

The compositor and related retail-plane handling were adapted from the Pareto snapshot in caruno-git/Americas-Shim. RPC and frame-rate work were adapted from the local cool_fox and VIctory-7900XTX references. Existing source attribution and the repository licence apply.

## Validation

- Release and diagnostics builds succeed with the locked toolchain and dependencies.
- Four root-signature tests pass against the system serializer.
- Four BC3 decoder tests pass.
- The display compositor GPU test reads back the expected transparent, half-transparent and opaque pixels after scaling.
- The console-instruction execution test passes with crash reporting disabled.
- The game reaches its settings UI and loading screens with two-plane composition active.
- The FSR-enabled run has a valid jitter phase count; it later stalls and triggers the title watchdog at image RVA 0x23cf683. This is a separate unresolved failure.

## Running

Build with `cargo build --release -p victory --locked`, then use `scripts/play.ps1 -Game <path-to-game-executable>`. The launcher does not inject keys or automatically terminate the process. Use external cache and data directories on a drive with sufficient free space.

The tested machine has a Xeon E3-1240 v3, 16 GiB RAM and RX 570 8 GiB under Windows 10. Series S memory limits, SM 6.5 translation and compute mesh fallback are active. Initial scene setup also causes extensive shader and driver pipeline compilation. These results do not establish a playable port.
