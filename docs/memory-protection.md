# Memory protection

Four measures protect the secret while it is in the buffer. This document describes each one, how it behaves on each platform, and where it stops.

| Measure | Purpose | Strength |
|---|---|---|
| Dedicated pages | Keep the secret apart from other heap data and make locking exact | Always applied |
| Locking | Keep the pages out of swap | Best-effort, or mandatory with `require_lock=True` |
| Core-dump exclusion | Keep the pages out of crash dumps | Best-effort, Linux and Android only |
| Zeroization | Remove the secret before the memory is reused | Always applied (`clear()` and drop) |

## Allocation

### Page size

The host page size is queried once and cached: `sysconf(_SC_PAGESIZE)` on Unix, `GetSystemInfo().dwPageSize` on Windows, 4096 elsewhere. If the OS reports something that is not a power of two, 4096 is used.

### Rounding

A request for `len` bytes becomes a region of `region_len = (len + page - 1) & !(page - 1)` bytes. The request fails (`MemoryError`) if that arithmetic overflows or if `region_len` exceeds `isize::MAX`. `len == 0` allocates nothing and has `region_len == 0`.

### Getting the pages

Pages are requested directly from the OS rather than from the heap allocator:

| Platform | Allocate | Release |
|---|---|---|
| Unix | `mmap(PROT_READ \| PROT_WRITE, MAP_PRIVATE \| MAP_ANON)` | `munmap` |
| Windows | `VirtualAlloc(MEM_COMMIT \| MEM_RESERVE, PAGE_READWRITE)` | `VirtualFree(MEM_RELEASE)` |
| Other | `std::alloc::alloc_zeroed` with page alignment | `std::alloc::dealloc` |

Fresh anonymous pages are zero-filled by the OS and committed lazily as they are touched. The buffer therefore starts zeroed, an oversized request does not write zeros over the whole range up front, and the mapping belongs to this buffer alone.

### Why dedicated pages matter

`mlock` and `munlock` (and `VirtualLock`/`VirtualUnlock`) act on whole pages, and on Linux and Windows locks on a page do not nest: a single unlock releases the page however many times it was locked. If a buffer shared a page with other heap data, two things would go wrong:

- unlocking one buffer on drop would silently unlock a neighbouring buffer that lives on the same page, while that neighbour's `is_locked()` still said `True`;
- a tiny secret would drag unrelated, non-secret data into the locked set.

With a mapping of its own, locking and unlocking affect only this buffer.

### Cost

Every buffer occupies at least one page (4 KiB on typical x86-64 systems, 16 KiB on Apple Silicon, up to 64 KiB on some ARM Linux configurations) and counts that much against the locked-memory limit. Many tiny buffers therefore consume the budget quickly. If you hold many small secrets, store them in one buffer at different offsets.

## Locking

For a non-empty buffer, the whole region is locked right after allocation: `mlock(region)` on Unix, `VirtualLock(region)` on Windows. Other platforms report "unsupported".

### Best-effort and strict

| Mode | On lock failure |
|---|---|
| default | The buffer is created anyway; `is_locked()` returns `False`. |
| `require_lock=True` | `OSError` is raised, with the OS error in the message. Nothing was written, so the freshly mapped pages are simply unmapped. |

`require_lock=True` with `size == 0` raises `ValueError`: an empty buffer has nothing to lock, and returning it would break the "never unlocked" promise.

### Why locking can fail

| Platform | Typical cause |
|---|---|
| Linux, Android | `RLIMIT_MEMLOCK` too low for the region (the default varies by distribution and kernel; containers often set it low). |
| macOS | Per-process locked-memory limit. |
| Windows | The process's minimum working-set size is too small; `VirtualLock` fails when the request does not fit. |
| any | Many other locked buffers already use the budget (see [Cost](#cost)). |

Raise the limit (`ulimit -l`, a systemd `LimitMEMLOCK=`, a container `--ulimit memlock=`) or lower the amount you lock. `is_locked()` is the per-buffer truth; check it, or use strict mode.

### Scope of the guarantee

A locked page is not written to swap while the process runs. Locking does not protect against hibernation images, a privileged reader of process memory, or swap used by a child created with `fork()` (locks are not inherited). See [Security](security.md).

## Core dumps

On Linux and Android, after locking, `madvise(region, MADV_DONTDUMP)` marks the pages so they are left out of core dumps. Before the memory is returned, `madvise(region, MADV_DODUMP)` undoes it, so the allocator or kernel does not inherit a stale exclusion.

- The result of `madvise` is ignored: if it fails, the pages simply stay dumpable.
- macOS and Windows have no equivalent applied. A crash dump there can contain the secret.
- Exclusion does not help against other ways of reading memory (a debugger, a live `/proc/<pid>/mem` reader).

## Zeroization

The `len` bytes of the buffer are overwritten with zeros, using the `zeroize` crate (volatile writes that the optimiser may not remove), in two places:

- `clear()`, on demand, in place; the length is unchanged;
- `Drop`, always, before unlocking and unmapping.

Page padding beyond `len` is never written, so it is still zero from the allocation and needs no wipe.

What is outside the zeroize boundary: any copy made from the buffer (`read()`, `as_bytes()`, `bytes(mv)`), the source object you passed to `write()`, and anything a consumer of a view copies internally.

## Release order

`Drop for SecureMemory` runs, then the `PageBuf` field drops:

1. zeroize;
2. unlock (if locked);
3. `MADV_DODUMP` (Linux/Android);
4. `munmap` / `VirtualFree`.

## Platform matrix

| | Linux | Android | macOS | Other Unix | Windows | Other |
|---|---|---|---|---|---|---|
| Page size | `sysconf` | `sysconf` | `sysconf` | `sysconf` | `GetSystemInfo` | 4096 |
| Allocation | `mmap` | `mmap` | `mmap` | `mmap` | `VirtualAlloc` | `std::alloc` |
| Locking | `mlock` | `mlock` | `mlock` | `mlock` | `VirtualLock` | unsupported |
| Core-dump exclusion | yes | yes | no | no | no | no |
| Zeroization | yes | yes | yes | yes | yes | yes |

## Failure modes

| Situation | Result |
|---|---|
| Size overflows during rounding, or exceeds `isize::MAX` | `MemoryError` |
| OS refuses the mapping | `MemoryError` |
| OS refuses to lock, default mode | Buffer works, `is_locked()` is `False` |
| OS refuses to lock, `require_lock=True` | `OSError` |
| Platform cannot lock at all, default mode | Buffer works, `is_locked()` is `False` |
| Platform cannot lock at all, `require_lock=True` | `OSError` |
| `madvise(MADV_DONTDUMP)` fails | Ignored; pages stay dumpable |
| `munlock`/`madvise(MADV_DODUMP)` fails on drop | Ignored; memory is unmapped regardless |
