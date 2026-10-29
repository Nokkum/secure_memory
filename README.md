<h1 align="center">Secure Memory</h1>
<p align="center">

`secure_memory` is a Rust/PyO3 Python extension for fixed-size sensitive-memory buffers.
</p>

### `secure_memory` provides:

- Zeroization on drop.
- Explicit `clear()` support.
- Best-effort OS memory locking.
- Read-only zero-copy `memoryview` access.
- Scoped access with `with_buffer()` and `view()`.
- Fixed-size storage to keep exported pointers stable.
- Bounds checks for reads and writes.
- Python 3.9+ support.
- CPython support.

> [!IMPORTANT]
> `secure_memory` protects its own allocation only. It cannot prevent other Python objects, libraries, logs, crash dumps, or operating-system mechanisms from creating copies of sensitive data.

---

## Requirements

- Python 3.9+
- CPython
- Rust 1.83+
- `maturin` for building the extension
- A supported OS/toolchain

Supported platforms declared by the project:

- Linux
- macOS
- Windows

---

## Installation

### From PyPI

If a compatible release is published:

```bash
python -m pip install secure_memory
```

Using a virtual environment is recommended:

```bash
python -m venv .venv
```

Activate it, then install:

```bash
python -m pip install secure_memory
```

### From source

Install the build backend:

```bash
python -m pip install -U maturin
```

Install the package into the active environment:

```bash
maturin develop --release
```

This is primarily intended for local development.

## Build distributions

The project uses `maturin` as its PEP 517 build backend.

Install the standard Python build frontend:

```bash
python -m pip install -U build
```

Build the project:

```bash
python -m build
```

The distributions are written to:

```text
dist/
```

You should normally get:

```text
dist/
├── secure_memory-0.2.0-*.whl
└── secure_memory-0.2.0.tar.gz
```

The files are:

- `.whl` — built distribution.
- `.tar.gz` — source distribution.

Because this package contains a compiled Rust extension, wheels are platform-specific.

---

## Basic usage

```python
from secure_memory import SecureMemory

memory = SecureMemory(32)

memory.write(0, b"secret")

with memory.view() as view:
    process_secret(view)

memory.clear()
```

---

## API

### `SecureMemory(size)`

Creates a fixed-size, zero-initialized buffer.

```python
memory = SecureMemory(64)
```

### `write(offset, data)`

Writes bytes into the buffer.

```python
memory.write(0, b"secret")
```

- Raises `ValueError` for invalid ranges.
- Raises `BufferError` while a view is alive.

### `read(offset, length)`

Returns a Python `bytes` copy.

```python
data = memory.read(0, 6)
```

Use zero-copy access when a copy is undesirable.

### `clear()`

Zeroizes the buffer without changing its size.

```python
memory.clear()
```

### `len()`

Returns the buffer size.

```python
size = memory.len()
```

### `is_locked()`

Reports whether OS memory locking succeeded.

```python
memory.is_locked()
```

### `as_bytes()`

Returns the complete buffer as a Python `bytes` copy.

```python
data = memory.as_bytes()
```

### `with_buffer(callback)`

Provides a temporary read-only `memoryview`.

```python
result = memory.with_buffer(lambda view: process_secret(view))
```

The view is released automatically.

### `view()`

Provides scoped read-only access:

```python
with memory.view() as view:
    process_secret(view)
```

The view is released when the block exits.

## Zero-copy access

Python's buffer protocol can expose the buffer without copying:

```python
view = memoryview(memory)
```

The view is read-only:

```python
view.readonly
```

Writing through the view is not supported.

Release it when finished:

```python
view.release()
```

Operations such as `bytes(view)` and `view.tobytes()` create copies.

## Memory locking

The extension attempts to lock its allocation into RAM:

- Unix: `mlock()`
- Windows: `VirtualLock()`

Check the result with:

```python
memory.is_locked()
```

Locking is best-effort and may fail because of operating-system limits or permissions.

## Zeroization

The buffer is wiped in two ways:

```python
memory.clear()
```

and automatically when the Rust object is dropped.

Zeroization applies only to the Rust-owned allocation.

Python-level copies are outside that boundary.

## Safe access model

The buffer is intentionally fixed-size.

This prevents its backing allocation from being resized while Python holds a pointer to it.

While a view is alive:

```python
memory.write(...)
```

is rejected.

`clear()` is still allowed so a secret can always be wiped.

---

## Security considerations

`secure_memory` is a memory-handling primitive, not a complete secrets-management system.

Avoid unnecessary copies:

```python
bytes(view)
view.tobytes()
memory.as_bytes()
```

when a zero-copy operation is possible.

Also avoid:

- Logging secrets.
- Converting secrets to `str` unnecessarily.
- Keeping temporary secret objects alive longer than needed.
- Assuming `is_locked()` is always `True`.
- Treating memory locking as encryption.

For persistent secrets, use an appropriate secrets manager, OS credential store, HSM, or encrypted storage instead.

---

## Development

Build the extension:

```bash
maturin develop
```

Build an optimized version:

```bash
maturin develop --release
```

Build the Rust crate:

```bash
cargo build
```

Run Rust tests:

```bash
cargo test
```

Run formatting checks:

```bash
cargo fmt --all --check
```

Run Clippy:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

---

### TestPyPI

Install Twine:

```bash
python -m pip install -U twine
```

Upload to TestPyPI:

```bash
python -m twine upload --repository testpypi dist/*
```

Test installation:

```bash
python -m pip install --index-url https://test.pypi.org/simple/ secure_memory
```

### PyPI

After testing the distributions:

```bash
python -m twine upload dist/*
```

Then users can install the package with:

```bash
python -m pip install secure_memory
```

---

## License

Licensed under the [Apache License 2.0](http://www.apache.org/licenses/LICENSE-2.0).

See the repository's license file for the complete terms.

---

## Python packaging references
- A PEP 517 build backend.
- Source distributions and wheels.
- `python -m build` for creating distributions.
- `pip` for installation.
- TestPyPI for release testing.
- PyPI for production distribution.
- Twine or trusted publishing for uploads.
