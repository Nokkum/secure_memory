# secure_memory documentation

`secure_memory` is a small Rust (PyO3) extension that gives Python a fixed-size buffer for secrets. The buffer lives on memory pages of its own, is locked into RAM where the operating system allows it, is kept out of Linux/Android core dumps, and is zeroized before it is released.

| Document | Read it for |
|---|---|
| [Overview](overview.md) | What the library is for, what it does and does not do, key terms |
| [Architecture](architecture.md) | Components, source map, invariants, object lifecycle, threading, error handling |
| [API](api.md) | Python API reference: every class, method and exception |
| [Memory Protection](memory-protection.md) | Allocation, locking, core dumps, zeroization, platform behaviour and limits |
| [Buffer Views](buffer-views.md) | Zero-copy access: the buffer protocol, `with_buffer()`, `view()`, and the rules that keep views safe |
| [Security](security.md) | Threat model, guarantees, non-guarantees, residual risks, usage guidance |
| [Design Decisions](design-decisions.md) | Why it is built this way, and the trade-offs |
| [Development](development.md) | Building, testing, CI, packaging, contributor checklist |
