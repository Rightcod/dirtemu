# Architecture

```
 user's files (outside repo)             VIctory
 ───────────────────────────             ─────────────────────────────────────────
 game assets ──────────────────────────► victory-pipeline + victory-game
                                         (victory-formats, victory-shader)
                                                    │
                                                    ▼
                                         cache_dir: converted data
                                                    │
 game executable ─► victory-loader ────► title code (native x86-64)
                    prepare, map, bind      │ imports
                                            ▼
                                         victory-hle ── host Windows (shared APIs)
                                            │ D3D12.X
                                            ▼
                                         victory-gpu ─► host D3D12
                                            │ shaders at pipeline creation
                                            ▼
                                         victory-shader ◄─► cache_dir
```

Shaders are translated when the title creates a pipeline, from the bytes it
passes, wherever it loaded them from (executable, archive, memory).
Converting them ahead of time from files is only a warm-up.

## Crates

| Crate | Role |
|-------|------|
| `victory-core` | config, asset-policy guard, logging |
| `victory-pe` | read-only PE32+ and import-library analysis |
| `victory-loader` | prepare, map, bind and start the title image |
| `victory-hle` | console API implementations, host pass-through policy |
| `victory-gpu` | D3D12.X objects, command lists and GPU memory → host D3D12 |
| `victory-shader` | Xbox shader containers → host shaders |
| `victory-formats` | the GPU's texture layouts (sizes, tiling, detiling), audio codecs; planned: BCPack |
| `victory-pipeline` | converter trait, content-addressed cache |
| `victory-game` | the title-specific formats, build identities, hooks |
| `apps/victory` | command-line front end |
| `xtask` | asset-policy check, git hooks |
| `tests/fixtures/hle-probe` | a stand-in "title" (our own code) for loader tests |

Everything except `victory-game` is platform-level and can be tested with
any Xbox Series title.

## Import resolution

For every `dll!function` the title imports, in order:

1. our implementation in `victory-hle::modules` (always wins);
2. the host's own function, if `HostPolicy` allows that DLL;
3. an export of a DLL the title ships beside its executable, mapped like
   the executable (only DLLs neither of the above serves);
4. a generated stub that names the import when called (`--unimplemented
   trap` breaks into the debugger and aborts; `zero` logs once and returns 0).

`LoadLibrary*` of a console-only DLL we implement (e.g. `xgameruntime.dll`)
returns a pseudo handle; `GetProcAddress` on it finds our functions.
`LoadLibrary*` of a title DLL returns its mapped image, mapping it first if
nothing imports it. By name, as by import, a title gets only host DLLs
`HostPolicy` passes through, and `GetProcAddress` on one returns our
override where import binding would.
`QueryApiImpl` hands out our runtime objects by provider and interface id
(`victory-hle::modules::xgameruntime` lists the areas); online areas answer
as an offline console. Game saves are files under `VICTORY_DATA_DIR`.

Title storage is a drive on the console, and titles build paths on that.
`victory-hle::modules::files` binds every path-taking title import
(kernel32 and the C/C++ runtime) and rewrites paths on a virtual drive to
the host folder behind it; persistent local storage is one such drive over
a folder under `VICTORY_DATA_DIR`. Other paths reach the host unchanged.

An exe's code reaches its static TLS as slot 0; mapped as a DLL it gets
another slot, so the loader redirects those loads (`victory-loader::tls`).
`victory run --execute` installs a crash reporter that names faults as
`module+rva` and unwinds the faulting thread's stack
(`victory-loader::crash`).

`victory census` shows this classification without running anything (it
does not map title DLLs; `victory run` without `--execute` does).

## Memory profiles

The title is told a console's title memory, and the layer keeps host copies,
staging buffers and card-side copies beside it. On a PC with 8 GiB of video
memory or 16 GiB of RAM those caches, not the title's figure, exhaust the
machine first. `VICTORY_MEMORY_PROFILE` (or `[memory].profile`) selects a
profile; `auto` follows the host:

| fact | `auto` picks |
|------|--------------|
| dedicated video memory <= 8 GiB | `series-s` |
| physical memory <= 20 GiB | `series-s` |
| Windows commit limit <= 24 GiB | `series-s` |
| commit headroom < 8 GiB | `series-s` |
| otherwise (or unknown) | `series-x` |

`off` keeps the console's figure and the former unbounded caches; `series-x`
is a generous set of caps; `series-s` reports 7,776 MiB of title memory (one
standard range, no GPU-optimal split) and caps the host side:

| cache | `series-s` | `series-x` |
|-------|-----------:|-----------:|
| sampled streamed texture host copies | 512 MiB at 8 GiB VRAM (`VRAM/16`, 512 MiB to 2 GiB) | 4 GiB |
| one sampled streamed texture | half the texture budget | 2 GiB |
| texture staging | 256 MiB total, 128 MiB per texture | 1 GiB / 512 MiB |
| upload scratch (tile pool) | 256 MiB, 32 MiB kept idle | 512 MiB |
| title buffers copied to the card | 256 MiB, copies >= 64 MiB | 512 MiB |
| host copies of title memory (buffers and windows) | 2.5 GiB | 6 GiB |
| shadow snapshot pool / kept snapshots | 64 MiB / 64 MiB | 512 MiB / 256 MiB |
| render target and depth view rings | 1024 each (unchanged; already small) | 1024 each |
| page pool the title may ask `xmem` for | title memory - 512 MiB | title memory - 512 MiB |

Render targets, depth targets and unordered-access textures are never
shrunk or refused; only sampled, streamed content is. The sampled texture
budget admits a texture whole or not at all: a partial D3D12 mip chain would
leave shader resource views naming levels the host texture does not have, so
per-mip residency needs descriptor-view clamping (not implemented) and is
not attempted. Staging is cut from the top of the mip chain: levels above
the per-texture staging cap are not uploaded and read as their previous
host contents, which costs detail at distance. A texture whose top mip alone
exceeds the cap is not uploaded. These refusals are logged with the cap, the
requested bytes and the bytes in use; they replace an eventual Windows
commit failure.

`victory-gpu` enforces the caps in `resource` (host texture admission),
`texture` (staging and the mip cut), `shadow` (host copies, card copies,
snapshot pools, window grid) and `host` (scratch chunks and view rings);
`xmem` checks the page-pool budget before creating a section. The budget
fields for descriptor heaps and the shader cache are advisory for now: their
creation lives in files outside this profile's scope, and they are logged so
a later change can enforce them.

## Shaders

`victory_shader::ir::build` turns a native program into SSA, one
instruction per machine instruction. `victory_shader::structure` then
removes EXEC: lane masks become per-lane booleans, wave-wide mask tests
become per-lane branches, and vector instructions keep a guard only where
a lane can reach them with its EXEC bit clear. `victory_shader::resource`
traces each descriptor and scalar memory address to the root parameter it
comes from (table, heap, root descriptor or constant). A descriptor a
waterfall loop reads from the first lane (`v_readfirstlane_b32`) is traced
to the per-lane load or choice that gives each lane its own (a mask's
`v_cndmask_b32`, or an if/else whose arms each set it: traced on the paths
the lanes take, from structurisation), and accessed with a non-uniform heap
index in one pass. `victory translate`
runs every shader it finds (containers, SGD5 files) through these and
lists what they cannot handle yet.

Without host mesh-shader support, `victory-shader::emit::mesh_fallback`
emits mesh groups as compute writes to intermediate records plus a
vertex-pulling VS. `victory-gpu::draw::mesh` runs one compute dispatch and one
instanced draw for a direct dispatch, and `ExecuteIndirect` for the separate
single-record indirect path; `victory-gpu::mesh_fallback` selects the path. Its
deliberately bounded subset is documented in `docs/mesh-fallback.md`.
