# RX 570 display loss, 2026-11-27

The Series S run started at 17:45 Omsk using the app-local Agility SM 6.6
translation profile and the completed shader archive. Pipeline states were
created during execution. The user reported about 0.5 FPS, artifacts extending
onto the desktop, then loss of display signal, followed by a manual restart.

Windows created a `Kernel_141` report at approximately 17:55 and recorded an
unclean restart at 17:56. Microsoft identifies live dump code
[0x141 as a video-engine timeout](https://learn.microsoft.com/en-us/windows-hardware/drivers/debugger/bug-check-0x141---video-engine-timeout-detected).
The protected report contents were not readable through the current Windows
account. No GPU dump was copied or created during this investigation. This
evidence does not identify the failing shader or establish a hardware defect.

## Evidence in the runtime log

- About 1,900 pipeline states were still being made near the end of the run.
  Thousands of draw and indirect commands were dropped each frame.
- Host buffer copies reached 2,559 MiB of the 2,560 MiB cap. Reported allocation
  refusals exceeded 16,000. Missing buffers therefore remained a rendering
  correctness problem even after completing shader translation.
- One frame took 3,039 ms: 1,845 ms preparing data, including 746 ms preparing
  textures, while the reported GPU time was approximately 50 ms. Other later
  frames included much longer GPU waits. A single FPS number does not describe
  all of these stalls.
- The direct constants-section optimization failed to open the large scattered
  section and fell back to copying. The log therefore does not establish that
  this section used the in-place GPU path.

## Corrections

Host-copy admission now attempts to reclaim the oldest unused cached windows
before refusing an allocation. A reclaimable window must have no command-list
reference, no descriptor reference, no pending shader writes and no in-place
allocation. Active resources remain alive. A buffer allocation uses a nonblocking
window-cache lookup to preserve the existing lock order.

A required root CBV or SRV whose buffer cannot be prepared now rejects the
draw or dispatch. Previously the binding loop continued without replacing
the old root binding. That could expose data from a previous command.
For bounded root SRV/UAV tables, a failed buffer allocation is also rejected
before caching a descriptor. A transient allocation failure therefore does not
become a permanently cached null view at that address.

These changes address concrete faults in memory admission and command binding.
They are not proof that the video-engine timeout has been eliminated. The game
was not relaunched after the display-loss report.

## Verification and next diagnosis

A WARP test creates real software-adapter resources and verifies reclaim order,
command-list references, descriptor references and pending writes. Five existing
window tests and strict Clippy checks also pass. Neither the test nor the
subsequent builds dispatches game shaders to the RX 570.

The local diagnostic launcher enables DRED and the D3D12 debug layer, uses
serialized submission, and prepares previously requested pipelines before title
execution. It is prepared for a later controlled investigation and has not been
run against the game after this failure. No Windows timeout, driver or GPU clock
settings were changed.
