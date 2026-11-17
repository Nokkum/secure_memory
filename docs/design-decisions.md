# Design decisions

Each entry records what was decided, why, and what it costs.

## 1. One narrow primitive in one source file

**Decision.** A single fixed-size buffer type, implemented in `src/lib.rs`, with a small API.
**Why.** A security primitive is easier to audit and reason about when it is small. Every behaviour has one obvious place.
**Cost.** The file mixes the Python-facing layer and the platform layer; the source map in [Architecture](architecture.md) is the guide.

## 2. Fixed size, never resized or moved

**Decision.** The buffer's length and address are fixed for the life of the object.
**Why.** Views hold a raw pointer into the buffer. A resize or move would leave them dangling.
**Cost.** Callers choose the size up front.

## 3. Dedicated pages instead of the heap

**Decision.** Each buffer owns whole pages (`len` rounded up), not a slice of a heap allocation.
**Why.** Locks work per page and, on Linux and Windows, do not nest. A heap buffer shares pages with other data, so one buffer's `munlock` can unlock another's pages while its `is_locked()` still says `True`, and locking drags neighbouring data along.
**Cost.** At least one page per buffer against the lock limit. Many tiny buffers are expensive; pack small secrets into one buffer.

## 4. Pages from the OS, not `std::alloc`

**Decision.** `mmap` on Unix and `VirtualAlloc` on Windows; `std::alloc` only as a fallback for other targets.
**Why.** For page-aligned requests Rust's `alloc_zeroed` allocates and then writes zeros over the entire range, committing every page up front. A huge but allocatable size could exhaust memory instead of failing cleanly. OS pages are zero-filled and committed lazily, and the buffer gets a mapping of its own.
**Cost.** Per-platform allocation code.

## 5. Best-effort locking by default, strict on request

**Decision.** A failed lock yields an unlocked buffer and `is_locked() == False`; `require_lock=True` turns that into `OSError`.
**Why.** Locking depends on process limits the caller may not control. Failing by default would break existing users and many deployments; silently continuing must not be the only option, so strictness is available.
**Cost.** Callers who need the guarantee must opt in and handle the error.

## 6. `require_lock=True` with `size == 0` raises `ValueError`

**Decision.** An empty strict buffer is rejected.
**Why.** An empty buffer is never locked. Returning one would hand back exactly the unlocked buffer strict mode promises not to.
**Cost.** A caller computing a size from data must special-case the empty case.

## 7. Read-only exports; mutation only through `write()` and `clear()`

**Decision.** The buffer protocol export is read-only.
**Why.** Every mutation then passes through code that checks bounds and view state.
**Cost.** Consumers cannot fill the buffer through a writable view; they use `write()`.

## 8. `write()` refused while views exist; `clear()` never refused

**Decision.** `write()` raises `BufferError` if any view is alive. `clear()` always runs.
**Why.** A view is a raw pointer, and a consumer that released the GIL could read it mid-write and see torn data. A wipe, on the other hand, must always succeed: a leaked view keeps the object alive, so if it could block `clear()` the secret could not be erased. A reader can only ever see zeros appear, never mixed plaintext.
**Cost.** Callers must release views before `write()`.

## 9. Scoped access that detects leaked views

**Decision.** `with_buffer()` and `view()` release their view and check the export counter afterwards.
**Why.** Slices of a memoryview keep the export alive. Catching that at the end of the scope turns a silent, permanent write lock into an immediate error.
**Cost.** A `BufferError` after the scope when a derived view escapes.

## 10. Export counter plus strong reference

**Decision.** `exports` counts live exports, and each export holds a strong reference to the object.
**Why.** The counter makes the rules in decision 8 checkable. The reference guarantees the memory outlives every view.
**Cost.** An unreleased view delays the zeroize-on-drop until it is released; `clear()` remains available for that reason.

## 11. `write()` takes any byte buffer

**Decision.** The source is extracted as a `PyBuffer<u8>` and copied directly.
**Why.** The previous `&[u8]` argument accepted only `bytes`, forcing the secret to exist as an object that cannot be wiped. A `bytearray` can be overwritten by the caller, and the copy goes straight into locked pages.
**Cost.** Buffers with another item format (for example `array('i')`) are rejected.

## 12. `__len__` and `__bool__`

**Decision.** `len(sm)` works, and `bool(sm)` is always `True`.
**Why.** `len()` as a method alone made the built-in fail. Defining `__len__` would make an empty buffer falsy, silently changing the meaning of `if sm:` for existing code, so `__bool__` pins the old truthiness.
**Cost.** None beyond one small method.

## 13. GIL-based threading model with defensive `try_borrow`

**Decision.** The module does not opt in to free-threaded Python. Where a borrow could fail, `try_borrow` returns `BufferError` rather than panicking.
**Why.** The GIL is what keeps `write()`/`clear()` apart from new exports; the design has not been analysed for free-threaded execution. `__releasebuffer__` also needs a shared borrow of the object to keep `exports` correct: under the GIL no `&mut self` method can be running when a view is released, but on a free-threaded interpreter `clear()` could be, and the decrement would be lost. Panics must not cross the FFI boundary.
**Cost.** A free-threaded interpreter re-enables the GIL for this module.

## 14. Core-dump exclusion on Linux and Android only, best-effort

**Decision.** `MADV_DONTDUMP` on the region, undone on drop; nothing on other platforms.
**Why.** It is available, cheap and undoable on Linux and Android; the other platforms have no equivalent applied here. Undoing it prevents unrelated memory reusing the pages from being left out of dumps.
**Cost.** Failure is silent, and macOS and Windows dumps can contain the secret.

## 15. Error vocabulary

**Decision.** `MemoryError` for allocation, `OSError` for a refused lock in strict mode, `ValueError` for bad arguments and bounds, `BufferError` for view conflicts and writable-buffer requests.
**Why.** Each maps to the Python exception that already means that kind of failure, so callers can handle them without library-specific types. On Unix the `OSError` for a refused lock is built from the OS error code, so `errno` is set and callers can branch on it; on Windows only the message is set.

## 16. Wipe only the bytes that were written

**Decision.** `SecureMemory` records the end of the furthest range `write()` has handed out (`dirty`). `clear()` and `Drop` zeroize `[0, dirty)`, and `clear()` resets `dirty` to 0. `PageBuf` implements no `DerefMut`, so `write()` is the only way bytes can become non-zero.
**Why.** Pages come from the OS zero-filled and committed lazily (decision 4). Wiping all `len` bytes touched every page, so clearing or dropping a large, never-filled buffer whose lock had failed committed all of it, which could exhaust memory. Bytes above the mark are known to be zero, so skipping them changes nothing observable. `dirty` is raised before the copy, so a write that fails part-way is still wiped.
**Cost.** One more field, and an invariant (architecture, invariant 7) that every future write path must preserve.

## 17. No comments in `src/lib.rs`

**Decision.** `src/lib.rs` carries no comments except its header line, and no doc comments.
**Why.** Doc comments on `#[pyclass]` and `#[pymethods]` items become runtime docstrings, which would then have to stay in sync with the stub and these documents. Explanations live in `docs/`, and each `unsafe` block's invariant is listed in [Architecture](architecture.md#where-unsafe-is-used).
**Cost.** A reader of the source has to open the documents to learn why something is the way it is.
