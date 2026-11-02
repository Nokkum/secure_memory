# Buffer views

Zero-copy access lets a consumer such as `hashlib` read the secret without it being copied into a new Python object. This document describes how views work and the rules that keep them safe.

## What is exported

`SecureMemory` implements the Python buffer protocol. Any `memoryview(sm)`, `with_buffer()` or `view()` goes through it. The export is:

- **read-only**: a request for a writable buffer raises `BufferError`;
- **one-dimensional**, with `itemsize` 1;
- `format` `B` when the consumer asks for a format, otherwise unspecified;
- `shape` and `strides` filled in only when the consumer asks for them;
- pointing straight at the backing store (no copy).

Each successful export:

1. increments the `exports` counter;
2. stores a strong reference to the `SecureMemory` in the `Py_buffer`;

and each release decrements the counter. Because of the strong reference the object, and therefore its `Drop` (the zeroize and unlock), cannot run while any view exists.

## Rules while views exist

| Operation | While a view is alive |
|---|---|
| New views / `memoryview(sm)` | Allowed (the counter goes up) |
| `read()`, `as_bytes()`, `len()`, `is_locked()` | Allowed |
| `write()` | **Refused** with `BufferError`; the buffer is untouched |
| `clear()` | **Allowed**; the wipe is visible through the views |
| Dropping the last reference to the object | Deferred until the views are released |

### Why `write()` is refused

A view points directly at the memory. A consumer running on another thread may have released the GIL (CPython's `hashlib` does so for large inputs, and socket or file I/O does too) and be reading the view while `write()` copies new bytes in. It would see a torn mix of old and new data. Refusing `write()` while any view exists removes the possibility. Release the views, then write.

### Why `clear()` is always allowed

A wipe must always succeed. If a leaked view could block `clear()`, and the same leaked view keeps the object (and its `Drop`) alive, the secret would become impossible to erase. `clear()` only ever writes zeros, so a reader racing with it can at worst see a partially wiped buffer, never new or inconsistent plaintext.

## Scoped access

Two helpers make the lifetime of a view explicit.

### `with_buffer(callback)`

1. Creates `memoryview(sm)` (counter +1).
2. Calls `callback(view)`.
3. Releases the view, always, even if the callback raised.
4. If the callback raised, that exception propagates. Otherwise, if the release itself failed, that error is raised.
5. If the counter is still non-zero, a derived view was kept alive: raises `BufferError`.
6. Returns the callback's result.

### `view()`

`with sm.view() as mv:` behaves the same way: `__enter__` creates the view, `__exit__` releases it. If an exception is already propagating out of the `with` body it is never masked: release problems and leaked views are reported only on a clean exit. `__exit__` always returns `False`.

## Derived views and leak detection

Slicing a `memoryview` (`mv[1:4]`) or wrapping it (`memoryview(mv)`) creates a new memoryview that shares the underlying export. Releasing the original memoryview does not end the export while a derived one is alive; the counter stays above zero until the last derived view is released or garbage-collected.

That is what the scoped helpers detect. After releasing their own view they check the counter, and a non-zero value means the caller kept a derived view:

```python
kept = []
sm.with_buffer(lambda mv: kept.append(mv[1:3]))   # BufferError
sm.write(0, b"x")                                 # still refused: view alive
kept.clear()                                      # drop the derived view
sm.write(0, b"x")                                 # works again
```

If `release()` itself fails, it is because a consumer holds a buffer export taken directly from that memoryview (for example a NumPy array built over it); the error is `BufferError: memoryview has N exported buffer(s)`.

## Copying out is the caller's choice

`bytes(mv)`, `mv.tobytes()` and similar calls create copies outside the zeroize boundary. They are not prevented. Likewise, a consumer may copy internally: a cryptographic library may copy a key into its own context when you pass it a memoryview. Zero-copy access removes the copies this library would otherwise create; it does not control what a consumer does with the bytes. See [Security](security.md).

## Patterns

```python
# Hash without creating a bytes copy of the secret
digest = sm.with_buffer(lambda mv: hashlib.sha256(mv).hexdigest())

# Same, as a statement
with sm.view() as mv:
    digest = hashlib.sha256(mv).hexdigest()

# Update the secret: views must be gone first
sm.write(0, new_secret_bytearray)

# Rotate: wipe, then store the new value
sm.clear()
sm.write(0, new_secret_bytearray)
```

Avoid storing the memoryview, or a slice of it, in a longer-lived object or passing it to code  that might. Do the work inside the `with` block or the callback.

## Errors

| Situation | Exception |
|---|---|
| `write()` while a view is alive | `BufferError` (write-while-viewed message) |
| Derived view kept past `with_buffer()` / `view()` | `BufferError` (leaked-view message) |
| Consumer requests a writable buffer | `BufferError` (read-only message) |
| Object is mutably borrowed when a view is requested | `BufferError` (`secure buffer is being modified; try again`); a defensive guard that is not expected while the GIL is held |
| Entering an already-active `SecureView` | `ValueError` |
