# Python API reference

```python
from secure_memory import SecureMemory, SecureView
```

`SecureView` is only ever created by `SecureMemory.view()`; it cannot be constructed directly.

## `SecureMemory(size, *, require_lock=False)`

Creates a fixed-size buffer of `size` zero bytes.

| Parameter | Meaning |
|---|---|
| `size` | Buffer length in bytes. Bounds for `read` and `write` are checked against it. |
| `require_lock` | Keyword-only. If `True`, failing to lock the memory raises `OSError` instead of returning an unlocked buffer. |

Behaviour:

- The bytes live on whole pages of their own, so the allocation is rounded up to a multiple of the page size internally. `len()` still reports `size`.
- Locking is best-effort by default: if the OS refuses, the buffer still works and `is_locked()` returns `False`.
- On Linux and Android the pages are excluded from core dumps, best-effort.
- A zero-length buffer is allowed and allocates nothing.

Raises:

| Exception | When |
|---|---|
| `MemoryError` | The allocation fails or the size cannot be represented. |
| `OSError` | `require_lock=True` and the OS refused to lock the memory. The message includes the OS error. |
| `ValueError` | `require_lock=True` with `size == 0` (an empty buffer cannot be locked). |
| `TypeError` | `require_lock` passed positionally, or `size` not an integer. |
| `OverflowError` | `size` is negative or too large for the platform's `usize`. |

```python
sm = SecureMemory(64)
sm = SecureMemory(64, require_lock=True)
```

## `write(offset, data)`

Copies a byte buffer into the secure buffer at `offset`.

- `data` can be `bytes`, `bytearray`, a `memoryview` of bytes or `array('B')`. It is copied directly from the source's own buffer with no intermediate copy. Buffers with another item format, such as `array('i')`, are rejected.
- Prefer a `bytearray` or `memoryview` as the source: you can overwrite it after storing the secret, which an immutable `bytes` object does not allow.
- A failed write leaves the buffer unchanged.

Raises:

| Exception | When |
|---|---|
| `BufferError` | Any view of the buffer is alive (see [buffer-views.md](buffer-views.md)). |
| `ValueError` | `offset + len(data)` overflows or exceeds the buffer length. |
| `TypeError` / `BufferError` | `data` is not a byte buffer (raised while extracting the argument). |

```python
secret = bytearray(b"secret")
sm.write(0, secret)
secret[:] = bytes(len(secret))
```

## `read(offset, length)` -> `bytes`

Returns a copy of `length` bytes starting at `offset`. The returned `bytes` object is outside the zeroize boundary; prefer `with_buffer()` where possible. `read` is not affected by live views.

Raises `ValueError` if `offset + length` overflows or exceeds the buffer length, and `OverflowError` if `offset` or `length` is negative.

## `as_bytes()` -> `bytes`

Returns a copy of the entire buffer as `bytes`. Same caveat as `read`.

## `clear()`

Overwrites the whole buffer with zeros in place. The length is unchanged and the buffer stays usable.

`clear()` is never refused because of live views: a wipe must always succeed. The wipe is visible through any live view, which can at worst observe a partially wiped buffer, never new or inconsistent plaintext.

## `is_locked()` -> `bool`

`True` if the OS lock call succeeded for the whole region (`mlock` on Unix, `VirtualLock` on Windows). `False` if locking failed, is unsupported, or the buffer is empty (there is nothing to lock).

## `len()`, `len(sm)`, `bool(sm)`

- `sm.len()` and `len(sm)` return the buffer length.
- `bool(sm)` is always `True`, including for an empty buffer. The object is a handle, not a container to test for emptiness; use `len(sm)` for that.

## `with_buffer(callback)`

Calls `callback(view)` with a read-only `memoryview` over the buffer (no copy) and returns the callback's result.

- The view is always released afterwards, even if the callback raises.
- If the callback raises, that exception propagates.
- If the callback kept a derived view alive (for example `mv[1:4]` stored somewhere), `BufferError` is raised after the call.
- Copying out of the view (`bytes(mv)`) is the caller's explicit choice and is not prevented.

```python
digest = sm.with_buffer(lambda mv: hashlib.sha256(mv).hexdigest())
```

## `view()` -> `SecureView`

Returns a context manager that yields the same kind of read-only `memoryview`:

```python
with sm.view() as mv:
    digest = hashlib.sha256(mv).hexdigest()
```

| `SecureView` method | Behaviour |
|---|---|
| `__enter__()` | Returns a read-only `memoryview` over the buffer. Raises `ValueError` if this `SecureView` is already active. |
| `__exit__(...)` | Releases the view. If no exception is propagating, raises `BufferError` when the release failed or a derived view is still alive. Never suppresses an exception (returns `False`). |

A `SecureView` can be entered again after it has been exited. It holds a strong reference to its `SecureMemory`.

## Buffer protocol

`SecureMemory` itself implements the Python buffer protocol, so these work directly:

```python
mv = memoryview(sm)
hashlib.sha256(sm).hexdigest()
bytes(sm)
```

The export is read-only, one-dimensional, `itemsize` 1, and reports format `B` when asked. A request for a writable buffer raises `BufferError` (`ctypes` `from_buffer` is an example of such a request). Every export holds a strong reference to the object, so the memory cannot be freed while a view exists. Full rules: [Buffer Views](buffer-views.md).

## Exceptions

| Exception | Raised by | Message or meaning |
|---|---|---|
| `MemoryError` | `SecureMemory()` | `cannot allocate a secure buffer of N bytes` |
| `OSError` | `SecureMemory(require_lock=True)` | `could not lock N bytes of secure memory into RAM: <os error>` |
| `ValueError` | `SecureMemory(0, require_lock=True)` | `cannot lock an empty buffer` |
| `ValueError` | `write` | `Offset overflow`, `Write would exceed buffer size` |
| `ValueError` | `read` | `Offset overflow`, `Read would exceed buffer size` |
| `ValueError` | `SecureView.__enter__` | `view is already active` |
| `BufferError` | `write` | `cannot write to the secure buffer while a view of it is alive (release the memoryview, and any slice or copy of it, first)` |
| `BufferError` | `with_buffer`, `SecureView.__exit__` | `a view of the secure buffer is still alive after the access scope ended (a slice or copy of the memoryview was kept)` |
| `BufferError` | buffer protocol | `SecureMemory exposes read-only buffers; use write() to modify it` |
| `BufferError` | buffer protocol, `with_buffer`, `SecureView.__exit__` | `secure buffer is being modified; try again` (a defensive guard; not expected while the GIL is held) |

## Not supported

- Resizing a buffer.
- Writable views or writable buffer exports.
- Indexing, slicing or iterating the object itself. Use `read()` or a view.
- Pickling and copying: no state-serialisation hooks are defined, so the default mechanisms are expected to fail with `TypeError`. This is intentional; a secret should not be serialised by accident.
