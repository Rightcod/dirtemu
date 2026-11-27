# RX 570 pipeline preparation, 2026-11-27

Preparation used the Series S profile on the RX 570 and Xeon E3-1240 v3
(4 cores, 8 logical processors). The title was not executed.

## Saved first 20,000 states

The selected prefix prioritizes previously requested states, then recipe keys.
It contains 20,000 unique preparation outcomes: 19,958 ready, 41 translation or
pipeline creation refusals, and one independently reproduced driver compiler
access violation recorded as failed. No recipe was unreadable. The remaining
count was zero. The driver library was saved before the final journal update.
The shader archive remained cached; it was not rescanned or rebuilt.

## Compilation concurrency

Preparation now defaults to every logical CPU, capped at 16 threads. An explicit
`--jobs` value overrides the default. The RX 570 launcher uses 8 threads on this
machine. Lower values remain available for machines with less memory.

A read-only stack snapshot during the 8-thread run found multiple workers
waiting inside D3D12 library load/store locks while other workers compiled.
Preparation workers now retain new PSOs in a pending map. Lookups can reuse
pending PSOs, and the next checkpoint stores them before serializing the driver
library and advancing the recipe journal. Failed stores prevent checkpointing.
The pending map is emptied at checkpoints, and the existing limit of 2,048 new
states per process still releases compiler memory between processes.
Rendering uses immediate stores. `VICTORY_PRECOMPILE_DEFER_STORES=0` restores
immediate stores in preparation workers for comparison.

## Validation and measurements

An identical sample of 128 recorded, previously successful states was prepared
with 8 threads in two separate empty pipeline libraries:

| Store mode | Whole process | PSO preparation phase | Result |
| --- | ---: | ---: | --- |
| Immediate | 38.257 s | 7.528 s | 128 ready, 0 refused |
| Deferred | 30.544 s | 1.300 s | 128 ready, 0 refused |

The phase runs from the early-list initialization log to the library-save log.
These
are single A/B measurements with driver and OS caches already warmed by earlier
runs. They do not establish the speedup for a complete cold game package.
Device initialization accounts for most of the whole-process time in this
small sample.

A fresh process then prepared the same 128 states after the sample journal was
moved aside. It accepted the saved 24,090-byte D3D12 library, returned 128 ready
and zero refused, and left the library unchanged. This checks actual pipeline
loading without relying on preparation journal entries.

The remaining 3,103 states of the requested 20,000-state prefix were completed
with deferred stores and 8 threads. The final driver library was 2,801,886 bytes.
Strict workspace Clippy and an Agility release build passed.

## Unresolved compiler fault

Recipe `172d48ae3eaae23deecd83077868ead6`, ordinal 9,763 in this selection,
uses vertex shader `4afb138ba2e54313` and pixel shader `cad3ecfdcdcf2a7d`.
It terminated the preparation worker with `0xc0000005` both with 3 threads and
with 1 thread. A debugger placed the fault in `amdxc64.dll` while servicing
`CreateGraphicsPipelineState`.

The same recipe was reproduced alone in an empty diagnostic cache with the
D3D12 debug layer. The layer reported no PSO error before the access violation.
This does not prove the translated shaders are correct; the cause in the
translator/driver interaction remains unresolved.

After the failed attempts, that recipe received a negative preparation outcome.
It is included in the 42 unsuccessful states and is not counted as ready.
This preparation record does not change runtime pipeline creation. It is not
a fix for the earlier in-game GPU timeout 0x141.
