# Development

## Prerequisites

- Rust 1.83 or newer (stable). 1.83 is the minimum PyO3 0.29 requires, and it is the `rust-version` in `Cargo.toml`.
- Python 3.9 or newer, with `pip`.
- [maturin](https://www.maturin.rs) (`pip install maturin`) and `pytest` (`pip install pytest`).

## Dependencies

| Crate | Version | Used for |
|---|---|---|
| `pyo3` | 0.29 | Python bindings, buffer protocol, `PyBuffer` |
| `zeroize` | 1.7 | Volatile wiping of the buffer |
| `libc` (Unix only) | 0.2 | `mmap`, `munmap`, `mlock`, `munlock`, `madvise`, `sysconf` |
| `windows-sys` (Windows only) | 0.61 | `VirtualAlloc`, `VirtualFree`, `VirtualLock`, `VirtualUnlock`, `GetSystemInfo` (features `Win32_System_Memory`, `Win32_System_SystemInformation`) |
| `pyo3` with `auto-initialize` (dev) | 0.29 | Embedding an interpreter for the Rust unit tests |

The crate is both a `cdylib` (what Python imports) and an `rlib` (so `cargo test` can link the unit tests).

## Build

```bash
python -m venv .venv && source .venv/bin/activate
pip install maturin pytest

maturin develop                          # debug build installed into the venv
maturin build --release --out dist       # release wheel(s) in dist/
```

Wheels are built per Python version rather than with the stable ABI: the buffer protocol is only part of the limited API from Python 3.11, so `abi3` would raise the minimum Python version.

## Test

### Rust unit tests

```bash
cargo test
```

### Python tests

```bash
maturin develop
python -m pytest
```

These exercise the real Python surface: `len()`/`bool()`, keyword-only `require_lock`, `hashlib` over a view, writing from `bytearray`/`memoryview`, rejection of non-byte buffers, `ctypes` writable-buffer requests, view and `with_buffer` release semantics, and `MemoryError` for absurd sizes.

Behaviour that depends on the OS (whether locking is permitted) is tested for coherence between `require_lock` and `is_locked()` rather than for a fixed outcome.

## Lint and format

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
```

## Conventions

- `src/lib.rs` has no comments except its header line. Put explanations in `docs/`, and keep the stub's docstrings current for Python users. Doc comments on the `#[pyclass]`/`#[pymethods]` items would become runtime docstrings; their absence is deliberate (see [Design Decisions](design-decisions.md), decision 17).
- Keep `rustfmt` clean. Mind the defaults: 100-column lines, and call arguments longer than 60 columns are laid out one per line.
- Every `unsafe` block must rely on an invariant listed in [Architecture](architecture.md). If a change needs a new one, add it there.
- Validate before mutating, so a failing call leaves the buffer unchanged.
