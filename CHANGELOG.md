<h1 align="center"> Changelog </h1>

# v0.3.0
- **Allocation**: every buffer now sits on whole pages of its own instead of in a `Vec`. `mlock`/`munlock` work per page and do not nest, so dropping one buffer could unlock pages another buffer (or unrelated heap data) shared with it, while `is_locked()` still said `True`. The length you ask for is unchanged. Each buffer now uses at least one page of `RLIMIT_MEMLOCK` (or the Windows working-set quota).
- **`SecureMemory(size)`**: an impossible or failed allocation raises `MemoryError` instead of aborting the process.
- **`require_lock=True`** (keyword-only): raises `OSError` instead of silently returning an unlocked buffer. The default is still best effort, so existing callers are unaffected.
- **Core dumps**: on Linux and Android the pages are marked `MADV_DONTDUMP` while the buffer lives. macOS and Windows have no equivalent applied.
- **`write()`**: accepts any bytes-like object (`bytearray`, `memoryview`, ...), not just `bytes`, so a secret can be stored from a buffer the caller then overwrites.
- **`len(sm)`** now works (`__len__`); `len()` is kept.
- **Borrow handling**: `with_buffer()` and `view()` use `try_borrow`, like `__getbuffer__`, so a concurrent borrow is a `BufferError`, never a panic.
- **Packaging**: added type stubs (`secure_memory.pyi`), a pytest suite. Removed the unused `tempfile` dev-dependency and zeroize's `derive` feature.

# v0.2.0
- **`write()`**: it now raises `BufferError` while any view is alive, including a slice or copy of a memoryview that was kept. Before, a consumer that releases the GIL could read mid-write and see a torn mix of old and new bytes. The check is race-free because PyO3's borrow flag already keeps `write()` apart from `read()` and from new views.
- **`__getbuffer__`**: it uses `try_borrow`, so a view requested while the object is being modified gives a `BufferError` instead of a panic.
- **Tests and docs**: there are three new tests (write refused with a live view, write refused with a retained derived view, `clear()` still working with a live view).
- Deliberately left `clear()` unguarded. A wipe that can be blocked would leave a leaked view's secret impossible to erase, since that view also keeps `Drop` from running. The cost is that a reader racing a `clear()` can see a partially wiped buffer, but never new plaintext.