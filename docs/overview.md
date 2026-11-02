# Overview

## Purpose

Python has no built-in way to hold a secret (a key, a token, a password) in memory that is protected from the usual leaks. `bytes` and `str` objects are immutable, can be copied freely by the interpreter, cannot be wiped, and can be written to swap or to a core dump. `secure_memory` provides one narrow primitive instead: a fixed-size byte buffer whose memory is managed outside the Python heap and treated with care.

## What it does

1. **Allocates on dedicated pages.** Each buffer is placed on whole memory pages that belong to it alone, obtained directly from the OS and zero-filled.
2. **Locks the pages into RAM** (`mlock` on Unix, `VirtualLock` on Windows) so they are not written to swap. Locking is best-effort by default and can be made mandatory with `require_lock=True`.
3. **Keeps the pages out of core dumps** on Linux and Android (`MADV_DONTDUMP`), best-effort.
4. **Zeroizes before release.** The contents are overwritten with zeros on `clear()` and again, unconditionally, when the object is dropped, before the pages are unlocked and returned to the OS.
5. **Gives zero-copy, read-only access** through the Python buffer protocol (`memoryview(sm)`), `with_buffer()` and `view()`, so consumers such as `hashlib` can read the secret without it being copied into a Python object.
6. **Keeps views consistent.** `write()` is refused while any view exists, so a reader never observes a half-written secret, and `with_buffer()`/`view()` detect views that were kept alive past their scope.

## What it does not do

- It does not encrypt, authenticate or otherwise transform data.
- It does not protect against a privileged attacker, a debugger, or code that copies the secret out (`read()`, `as_bytes()`, `bytes(mv)`, or a library that copies internally). See [Security](security.md).
- It does not resize buffers, expose writable views, or support pickling.
- It does not guarantee locking: without `require_lock=True` a failed lock is silent apart from `is_locked()` returning `False`.
- It does not apply core-dump exclusion on macOS or Windows.
- It does not handle `fork()`: a forked child inherits a copy of the secret that is no longer locked.

## Key terms

| Term | Meaning |
|---|---|
| **Secure buffer** | A `SecureMemory` object: the Python-visible handle plus its backing store. |
| **Length (`len`)** | The size you asked for. `len()`, `read()` and `write()` are bounded by it. |
| **Region** | The whole pages actually allocated: `len` rounded up to a multiple of the page size. Locking and core-dump exclusion apply to the region. |
| **Zeroize boundary** | The memory the library controls and wipes: the `len` bytes of the buffer. Anything copied out of it is outside the boundary. |
| **Export / view** | A live buffer-protocol export of the buffer, typically a `memoryview`. The library counts them. |
| **Best-effort** | The operation is attempted and a failure is tolerated (`is_locked()` reports the lock result). |
| **Strict mode** | `require_lock=True`: a failure to lock raises instead of being tolerated. |

## At a glance

```python
import hashlib
from secure_memory import SecureMemory

sm = SecureMemory(32, require_lock=True)   # OSError if the OS refuses to lock

key = bytearray(b"0123456789abcdef0123456789abcdef")
sm.write(0, key)                           # copied straight into locked pages
key[:] = bytes(len(key))                   # wipe the caller's copy

digest = sm.with_buffer(lambda mv: hashlib.sha256(mv).hexdigest())

sm.clear()                                 # zeroize now; also happens on drop
```

## Platform and toolchain support

| Platform | Allocation | Locking | Core-dump exclusion |
|---|---|---|---|
| Linux, Android | `mmap` | `mlock` | `MADV_DONTDUMP` |
| macOS and other Unix | `mmap` | `mlock` | not applied |
| Windows | `VirtualAlloc` | `VirtualLock` | not applied |
| Anything else | `std::alloc` fallback | reports "unsupported" | not applied |

Python 3.9 or newer; Rust 1.83 or newer (the minimum required by PyO3 0.29, which the crate builds against). Details are in [Memory Protection ](memory-protection.md).
