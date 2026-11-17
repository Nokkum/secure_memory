# Architecture

## Shape of the project

`secure_memory` is one Rust crate that builds as both a `cdylib` (the Python extension module) and an `rlib` (so `cargo test` can link the unit tests). All code is in `src/lib.rs`. The Python module is also called `secure_memory` and exposes two classes: `SecureMemory` and `SecureView`.

```
 Python code
    |   SecureMemory(...)   .write()  .read()  .clear()  .with_buffer()  .view()
    |   memoryview(sm)      hashlib.sha256(sm)
    v
 +---------------------------- PyO3 layer -----------------------------+
 |  #[pymodule] secure_memory                                          |
 |    SecureMemory (#[pyclass])          SecureView (#[pyclass])       |
 |      - argument validation, error mapping                           |
 |      - buffer protocol: __getbuffer__ / __releasebuffer__           |
 |      - exports counter (AtomicUsize)                                |
 +----------------------------------+----------------------------------+
                                    |
 +---------------------- memory layer (plain Rust) --------------------+
 |  PageBuf: page-aligned, page-multiple, zero-filled region           |
 |  platform functions:                                                |
 |    page_size  os_alloc/os_free  lock_memory/unlock_memory           |
 |    exclude_from_core_dumps/include_in_core_dumps                    |
 +----------------------------------+----------------------------------+
                                    |
   OS:  mmap/munmap, mlock/munlock, madvise   (Unix)
        VirtualAlloc/VirtualFree, VirtualLock/VirtualUnlock (Windows)
```

## Source map

Everything below is in `src/lib.rs`, in this order.

| Item | Role |
|---|---|
| `LEAKED_VIEW_MSG`, `WRITE_WHILE_VIEWED_MSG`, `BUSY_MSG` | Text of the `BufferError`s raised for view conflicts. |
| `secure_memory` (`#[pymodule]`) | Module initialiser; registers `SecureMemory` and `SecureView`. |
| `SecureMemory` (`#[pyclass]`) | The buffer handle. Fields: `data: PageBuf`, `locked: bool`, `exports: AtomicUsize`, `dirty: usize`. |
| `#[pymethods] impl SecureMemory` | The public API: `new`, `write`, `read`, `clear`, `is_locked`, `len`, `__len__`, `__bool__`, `as_bytes`, `with_buffer`, `view`, `__getbuffer__`, `__releasebuffer__`. |
| `SecureView` (`#[pyclass]`) | Context manager returned by `view()`. Fields: `owner: Py<SecureMemory>`, `view: Option<Py<PyAny>>`. Methods: `__enter__`, `__exit__`. |
| `impl SecureMemory` (private, first block) | Helpers: `memoryview`, `exports`, `exports_checked`, `write_range`, `wipe`, and `write_slice` (test builds only). |
| `page_size`, `query_page_size` | Host page size, queried once and cached. |
| `os_alloc`, `os_free` | Page allocation and release, one implementation per platform family. |
| `PageBuf` | Owner of one page-aligned region. Implements `Deref` to `[u8]` (no `DerefMut`; see invariant 7), `Drop`, and `Send`/`Sync`. |
| `impl SecureMemory` (private, platform block) | `lock_memory`, `lock_error`, `unlock_memory`, `exclude_from_core_dumps`, `include_in_core_dumps`. |
| `Drop for SecureMemory` | Zeroize, unlock, restore dump eligibility. |
| `mod tests` | Rust unit tests (embedded Python via PyO3's `auto-initialize`). |

## Data model

```
SecureMemory                              PageBuf
+---------------------------+             +---------------------------+
| data: PageBuf ------------+------------>| ptr: NonNull<u8>          |
| locked: bool              |             | len: usize        (asked) |
| exports: AtomicUsize      |             | region_len: usize (pages) |
| dirty: usize              |             +---------------------------+
+---------------------------+

SecureView
+---------------------------+
| owner: Py<SecureMemory>   |  strong reference, keeps the buffer alive
| view: Option<Py<PyAny>>   |  the memoryview handed out by __enter__
+---------------------------+
```

`PageBuf` derefs to `[u8]` of length `len`. `region_len` is `len` rounded up to
a whole number of pages, or `0` for an empty buffer (which allocates nothing).

`dirty` is the length of the prefix of the buffer that has ever been written: the end of the furthest range `write_range` has handed out since the last wipe (see invariant 7).

### Invariants

1. `data` is never resized or moved while the object lives. Exported views hold a raw pointer into it.
2. Every successful `__getbuffer__` stores a strong reference to the `SecureMemory` in the `Py_buffer`, so the object, and therefore its `Drop`, outlives every view.
3. `exports` equals the number of successful `__getbuffer__` calls minus the `__releasebuffer__` calls: the number of live exports.
4. `write()` only proceeds while `exports == 0`.
5. `locked` is `true` only if the whole region was locked successfully, and `Drop` unlocks the region if and only if `locked` is `true`.
6. Bytes in the region beyond `len` (page padding) are never written, so they stay zero from allocation.
7. `dirty <= len`, and every byte at offset `dirty` or above is zero. `write_range` raises `dirty` to the end of each range it hands out, before that range is written; `wipe` zeroizes `[0, dirty)` and resets `dirty` to 0. Nothing else can write to the buffer: `PageBuf` has no `DerefMut`, `as_mut_slice` is private to the module, and the buffer protocol export is read-only.
8. The pointer stored in a `Py_buffer` is the region's own pointer (`PageBuf::region`), not one derived from a `&[u8]`. Taking `&mut self` for `write()` or `clear()` therefore does not invalidate the pointers that live views hold.

## Lifecycle

### Creation: `SecureMemory(size, *, require_lock=False)`

1. If `size == 0` and `require_lock`, raise `ValueError`.
2. `PageBuf::new_zeroed(size)`: round `size` up to whole pages (failing on overflow or on a size above `isize::MAX`), then `os_alloc`. Failure raises `MemoryError`.
3. For a non-empty buffer, `lock_memory(region)`. On failure raise `OSError` (built by `lock_error`) if `require_lock`, otherwise record `locked = false` and carry on.
4. `exclude_from_core_dumps(region)` (Linux and Android only; result ignored).
5. Return the object with `exports = 0`.

If step 3 raises, the `PageBuf` is dropped and unmapped; nothing was ever written to it.

### Use

`write`, `read`, `as_bytes`, `clear`, views and `with_buffer` operate on `data` as described in [API](api.md) and [Buffer Views](buffer-views.md).

### Destruction: `Drop for SecureMemory`, then `Drop for PageBuf`

1. `wipe`: zeroize the first `dirty` bytes (volatile writes via the `zeroize` crate). The rest of the buffer was never written and is still zero from allocation.
2. If the region is non-empty: `unlock_memory` if `locked`, then `include_in_core_dumps`, so the allocator does not inherit the exclusion.
3. The `data` field drops: `PageBuf::drop` calls `os_free` (`munmap` / `VirtualFree`).

The order matters: the pages are wiped while still locked, and only then unlocked and returned.

## Operation flows

```
write(offset, data)
  extract `data` as a PyBuffer<u8>        -> error if not a byte buffer
  write_range(offset, data.item_count())
      exports != 0            -> BufferError (nothing written)
      offset + len overflows  -> ValueError
      offset + len > len      -> ValueError (nothing written)
      otherwise               -> dirty = max(dirty, offset + len)
  copy_to_slice(dest)         one copy: source buffer -> secure pages

with_buffer(callback)
  mv = memoryview(sm)                     exports += 1
  result = callback(mv)
  mv.release()                            always, even if callback raised
  if callback raised  -> re-raise it
  if release failed   -> raise that error
  if exports != 0     -> BufferError (a derived view was kept)
  return result
```

## Threading model

- The module does not declare free-threaded support (`gil_used = false`), so a free-threaded Python interpreter re-enables the GIL when it imports it. All methods run with the GIL held.
- PyO3's per-object borrow flag separates `&self` methods (`read`, `as_bytes`, `is_locked`, ...) from `&mut self` methods (`write`, `clear`).
- The GIL, together with that borrow flag, is what keeps `write()` and `clear()` from running while a new export is being created. The `try_borrow` calls in `__getbuffer__` and `exports_checked` are defensive: if the object were ever borrowed mutably at that moment, they raise `BufferError` instead of panicking.
- The real concurrency hazard is a consumer on another thread that has released the GIL while reading a view (for example `hashlib` on a large buffer, or socket and file I/O). `write()` refuses to run while any view exists for that reason. See [Buffer Views](buffer-views.md).
- `PageBuf` is `Send + Sync` because it uniquely owns its allocation, like `Box<[u8]>`. `exports` is an atomic (`AcqRel` on updates, `Acquire` on reads).
- `__releasebuffer__` takes `&self`, which needs a shared PyO3 borrow, and it must run for `exports` to stay correct. Under the GIL nothing holding `&mut self` can be running when a view is released, so that borrow cannot fail. On a free-threaded interpreter `clear()` could hold `&mut self` at that moment, the decrement would be lost, and `write()` would be refused for good. This is one of the reasons the module does not declare free-threaded support (see [Design Decisions](design-decisions.md), decision 13).
- `clear()` can run while a consumer that released the GIL is reading a view. Formally that is a data race on plain bytes; the observable outcome is limited to the reader seeing zeros appear (decision 8).

## Error handling

- Validate first, mutate second: `write` checks view state and bounds before it copies, so a failed write leaves the buffer untouched.
- Errors are ordinary Python exceptions: `MemoryError` (allocation), `OSError` (strict-mode lock failure), `ValueError` (bounds, empty strict buffer, re-entering an active view), `BufferError` (view conflicts, writable-buffer requests). The full table is in [API](api.md).
- OS error details are surfaced only where the caller asked for strictness (`require_lock=True`). In best-effort mode they are reduced to `is_locked() == False` or ignored (core-dump exclusion). On Unix the strict-mode `OSError` carries the OS `errno`; on Windows only the message is set.
- The code avoids panicking across the FFI boundary; where a borrow could fail it uses `try_borrow` and returns `BufferError`.

## Where `unsafe` is used

| Location | Why | What makes it sound |
|---|---|---|
| `PageBuf` slices (`from_raw_parts`, `from_raw_parts_mut`) | Build `&[u8]` / `&mut [u8]` over the region. | `ptr` is valid for `len` bytes, or `len == 0` and `ptr` is a non-null dangling pointer. `&mut self` is exclusive. |
| `unsafe impl Send/Sync for PageBuf` | Raw pointer field blocks the auto-impls. | The allocation is uniquely owned. |
| `os_alloc` / `os_free` | `mmap`/`munmap`, `VirtualAlloc`/`VirtualFree`, `alloc_zeroed`/`dealloc`. | Sizes are page multiples; `os_free` receives exactly what `os_alloc` returned. |
| `lock_memory`, `unlock_memory`, core-dump functions | `mlock`, `munlock`, `madvise`, `VirtualLock`, `VirtualUnlock`. | They act on a live region owned by the caller. |
| `query_page_size` | `sysconf`, `GetSystemInfo`. | No preconditions; `SYSTEM_INFO` is plain data. |
| `__getbuffer__` / `__releasebuffer__` | Fill in a CPython `Py_buffer`. | `data` never moves; the view owns a strong reference to the object. |

## Conditional compilation

| `cfg` | Used for |
|---|---|
| `unix` | `libc` dependency; `mmap`, `mlock`, `sysconf` paths |
| `windows` | `windows-sys` dependency; `VirtualAlloc`, `VirtualLock`, `GetSystemInfo` paths |
| `not(any(unix, windows))` | Portable fallbacks: `std::alloc`, a 4096-byte page size, lock reports `Unsupported` |
| `any(target_os = "linux", target_os = "android")` | `MADV_DONTDUMP` / `MADV_DODUMP`; no-op stubs elsewhere |
| `test` | `write_slice`, the `tests` module |
