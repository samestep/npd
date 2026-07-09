# Draft upstream issue for NixOS/nix-eval-jobs

Title: **~100× slowdown on macOS: `--max-memory-size` check misreads `ru_maxrss` (bytes on Darwin, KiB on Linux), so the worker restarts after every job**

## Summary

On macOS, `nix-eval-jobs` evaluates a full nixpkgs at ~1–2 attrs/s while the
same version on Linux (same hardware, aarch64 VMs side by side) does ~170
attrs/s. The cause is a units bug in the worker-restart check, not eval
performance: plain `nix-instantiate` over the same package set is equally fast
on both OSes.

`shouldRestart` in `src/worker.cc` does:

```cpp
const size_t maxrss = resourceUsage.ru_maxrss;
static constexpr size_t KB_TO_BYTES = 1024;
return maxrss > args.maxMemorySize * KB_TO_BYTES;
```

`--max-memory-size` is documented in MiB. On **Linux**, `ru_maxrss` is in
**kilobytes** (getrusage(2)), so `MiB * 1024 = KiB` and the comparison is
correct. On **macOS**, `ru_maxrss` is in **bytes**, per Apple's current
[getrusage(2) man page](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/man/man2/getrusage.2)
("the maximum resident set size utilized (in bytes)", corrected in OS X 10.8 —
beware the pre-2012 copy in Apple's Documentation Archive that still says
"kilobytes") and the kernel itself: XNU fills the field from Mach task info,
whose unit is bytes
([`kern_resource.c`: `ru_maxrss = (long)tinfo.resident_size_max`](https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/kern_resource.c),
[`task_info.h`: `resident_size_max; /* maximum resident memory size (bytes) */`](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/mach/task_info.h)).
This is a classic portability trap — libuv normalizes it with the comment
["Most platforms report ru_maxrss in kilobytes; macOS and Solaris are the
outliers because of course they are"](https://github.com/libuv/libuv/blob/v1.x/src/unix/core.c#L1126-L1133).
Empirically, a process that touches 256 MiB reports `ru_maxrss = 263104` on
Linux and `269910016` on macOS. So on Darwin the effective limit is
`--max-memory-size` **KiB**: the default 4096 becomes a 4 MiB cap. Every
nixpkgs eval worker exceeds that the moment it imports nixpkgs, so
`processJobRequest` returns false after the **first** job, the worker exits
with `restart`, and the collector forks a fresh worker that re-imports all of
nixpkgs — for **every single attribute** (`GC_DONT_GC=1` means restarts are the
only memory reclamation, so each restart is a full re-eval of the import).

This also can't be worked around with a merely "large" value: `--max-memory-size
999999` still yields a ~1 GB effective cap on Darwin, below a full-set worker's
RSS.

## Reproduction (aarch64-darwin, nix-eval-jobs from nixpkgs)

300 attrs of `python3Packages`, one worker:

```console
$ time nix-eval-jobs --workers 1 --max-memory-size 4096 --expr "$EXPR" | wc -l
300
... 101.33s user 38.00s system 78% cpu 2:56.83 total   # ~0.6 s/attr, mostly fork+reimport

# compensate the 1024× unit error: 4 GiB expressed as 4194304
$ time nix-eval-jobs --workers 1 --max-memory-size 4194304 --expr "$EXPR" | wc -l
300
... 0.99s user 0.33s system 81% cpu 1.617 total        # 110× faster
```

Full nixpkgs, one worker, 30-second window, attrs emitted:

| | Linux VM | macOS VM (default) | macOS VM (limit ×1024) |
|---|---|---|---|
| attrs / 30 s | 5134 | **60** | **7671** |

(`EXPR` = `import (builtins.fetchTarball "https://github.com/NixOS/nixpkgs/archive/3af24d1a5fc8.tar.gz") { config = {}; overlays = []; }`,
wrapped with `lib.genAttrs (lib.take 300 (attrNames pkgs.python3Packages))` for the small case.)

The high system time (38 s for 300 attrs) is the per-job `fork()`; heap/GC are
uninvolved (with `GC_PRINT_STATS=1` the worker never collects and its heap
never grows past the initial size, because it never lives past one job).

## Fix

Scale `ru_maxrss` per platform, e.g.:

```cpp
auto shouldRestart(const MyArgs &args) -> bool {
    struct rusage resourceUsage = {};
    getrusage(RUSAGE_SELF, &resourceUsage);
    // ru_maxrss is in kilobytes on Linux, bytes on macOS (getrusage(2)).
#ifdef __APPLE__
    const size_t maxrssBytes = resourceUsage.ru_maxrss;
#else
    const size_t maxrssBytes = resourceUsage.ru_maxrss * 1024UL;
#endif
    constexpr size_t MB_TO_BYTES = 1024UL * 1024UL;
    return maxrssBytes > args.maxMemorySize * MB_TO_BYTES;
}
```

(FreeBSD/NetBSD use kilobytes like Linux; only Darwin documents bytes.)

Possibly related: #389 reports the limit misbehaving on Linux for different
reasons (RSS vs swap); this issue is specifically the Darwin bytes/KiB mismatch,
which turns the limiter into a restart-per-job loop.
