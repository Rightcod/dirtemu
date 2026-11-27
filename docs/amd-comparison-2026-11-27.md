# RX 570 rendering and AMD fork comparison

Compared public revisions on 27 November 2026:

- [Belobug/VIctory-Belobug-AMD, 4f62c2a](https://github.com/Belobug/VIctory-Belobug-AMD/tree/4f62c2a)
- [avatarmanz/VIctory-7900XTX, 014d52d](https://github.com/avatarmanz/VIctory-7900XTX/tree/014d52d)

Both are GPL-3.0 projects. The integer blend normalization and pixel quad-read
fix follow Belobug's implementation. The constants section mapping, contiguous
section view discovery, ordered indirect preparation, and submission-time texture
upload follow avatarmanz's implementation, adapted to this tree's buffer copies,
memory profiles, and SM 6.5 mesh fallback. Credits also belong to pgm1207 and
Arvi23 for the integer target fix cited by Belobug.

## Evidence from this RX 570

The previous `frame-profiler.err.log` contained 816,129 refused buffer copies at
2,559 MiB of a 2,560 MiB limit, and 216,065 refused sampled textures at about
511 MiB of a 512 MiB limit. These refusals discard data needed by rendering.
They are separate from driver pipeline refusals and slow title streaming.

## Changes

| Path | Change | Reason |
|---|---|---|
| Graphics and mesh PSOs | Disable blending only on integer attachments, expanding shared blend state as needed | D3D12 rejects integer blending and loses every draw using that PSO |
| Pixel translation | Use quad reads for quad-local swizzles and DPP even without a discard | AMD wave reads exclude helper pixels used for derivatives at triangle edges |
| Texture submission | Refresh staging before deciding to record an upload in the list prelude | Data arriving after command recording must be uploaded in that submission |
| Partial textures | Retry on elapsed time, every 250 ms, rather than 30 title ticks | Low frame rates must not add tens of seconds to texture arrival |
| Staging pressure | Reclaim oldest staging buffers held only by the cache before refusing a new one | GPU texture contents remain valid and active lists retain their staging |
| Sampled textures | `VICTORY_TEXTURE_MB` override, bounded to one quarter of physical VRAM | The launcher uses 1,536 MiB instead of 512 MiB on the 8 GiB RX 570 |
| Root windows | Enable the existing `VICTORY_WINDOW_TAILS=1` path | Avoid copying the unused prefix of root-view windows |
| Constants | `VICTORY_PORT_CONSTANTS=1`, separate view of contiguous low-address section pages | GPU accesses the title's bytes without a duplicate host copy and copy-back |
| Indirect arguments | Disable speculative window clamps for in-place arguments/counts | Earlier culling writes must complete before the arguments are prepared |
| Buffer admission | Try in-place placement before applying the copy budget | A shared buffer does not consume copy memory |
| Precompile | Persist shader failure details with the archive summary | A completed scan must expose actual translation refusals |

The constants optimization is limited to contiguous section memory below 4 GiB.
Failure to open it leaves the copy path available. Like its reference
implementation, successful auxiliary views remain mapped for the process lifetime;
the host heap and cached buffers determine how long their pages stay pinned.

The 7900XTX tree also has raw-byte CBV reads, patched descriptor-store journals,
parallel write guards, texture tiles on demand, and asynchronous submission.
Those are distinct changes with dependencies on its newer memory architecture.
They have not been transplanted by copying that tree's large RAM/VRAM settings.
That fork still documents slow low-detail streaming at low frame rates.

## Validation

The test suite covers shared and independent blend state, contiguous and
discontinuous physical views, GPU reads and writes across reserved-section views,
pixel quad translation validation, and texture data changed after command
recording. Performance and screenshot comparisons require a new game run after
the full shader/pipeline preparation completes.
