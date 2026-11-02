# Security

## Threat model

`secure_memory` reduces the ways a secret held by a **running, unprivileged Python process** can leak through the operating system's memory management:

| Leak path | Mitigation |
|---|---|
| Swap or pagefile | Locking (`mlock` / `VirtualLock`) |
| Core dump (Linux, Android) | `MADV_DONTDUMP` |
| Lingering after use | Zeroization on `clear()` and drop |
| Stray copies made by the interpreter | Zero-copy views instead of `bytes`/`str` |
| Reuse of freed memory | Pages are wiped, then unmapped |
| Torn reads while a secret is being replaced | `write()` refused while views exist |

It is a building block for handling secrets carefully, not a sandbox.

## Guarantees (when the preconditions hold)

1. **Zeroized on drop.** The `len` bytes are overwritten with zeros before the pages are unlocked and unmapped, even if the object is dropped by the interpreter during an exception.
2. **Not swapped**, provided locking succeeded. With `require_lock=True` you know it did; otherwise check `is_locked()`.
3. **Not in Linux or Android core dumps**, unless `madvise` failed (the result is ignored).
4. **Memory isolation from other allocations.** The secret shares no page with unrelated heap data, so neither locking nor core-dump exclusion touches other memory and no other allocation can be placed beside it on a page.
5. **No accidental writes through views.** Exports are read-only, and `write()` cannot run while a view exists.

## Non-guarantees

| Not protected | Why |
|---|---|
| Copies you make: `read()`, `as_bytes()`, `bytes(mv)`, `mv.tobytes()` | They produce ordinary Python objects outside the zeroize boundary. |
| The object you pass to `write()` | If it is `bytes` it cannot be wiped. Use a `bytearray` and overwrite it. |
| Copies made by consumers of a view | A library may copy a key into its own context when handed a memoryview. |
| Core dumps on macOS and Windows | No exclusion is applied. |
| A child created with `fork()` | The child inherits a copy of the pages; memory locks are not inherited, so the child's copy can be swapped. |
| A privileged or same-user attacker | `ptrace`, `/proc/<pid>/mem`, `process_vm_readv`, debuggers, kernel or hypervisor access read the memory directly. |
| Hibernation and suspend-to-disk | The whole of RAM, locked pages included, can be written to the hibernation image. |
| Cold-boot and physical memory attacks | Out of scope. |
| Secrets in the interpreter: `str`/`bytes` literals, tracebacks, logging, REPL history | Outside this library. |
| Lock failure in default mode | The buffer still works, with `is_locked() == False`. |

## Residual risks and trade-offs

- **Lock budget.** Each buffer owns at least one whole page and counts against the locked-memory limit. Many small buffers can exhaust it; in default mode the excess buffers are silently unlocked (`is_locked()` is `False`), and in strict mode creation raises `OSError`.
- **Forked children.** Not handled. Setting `MADV_WIPEONFORK` would give a child zeroed pages instead of the secret, which prevents a leak but makes child code that reads the buffer silently see zeros (for example, a zero-filled key). It has not been added because it needs an explicit opt-in.
- **Interpreter-level exposure.** Every Python reference to a `bytes` object that held the secret is a copy you must reason about yourself.
- **Not constant-time.** The library does no secret-dependent computation, so this is not applicable here, but consumers of a view should use constant-time comparisons (`hmac.compare_digest`) where relevant.

## Using it safely

1. **Prefer strict mode** (`require_lock=True`) wherever running without a lock is unacceptable, and handle `OSError` deliberately.
2. **Keep the secret in a `bytearray`** while you load it, `write()` it, then overwrite the bytearray (`ba[:] = bytes(len(ba))`).
3. **Use views instead of copies.** `with_buffer()` or `with sm.view()` for hashing and similar operations; avoid `read()` and `as_bytes()`.
4. **Keep views short-lived and unretained.** Do not store a memoryview or a slice of it.
5. **Never convert to `str`** (immutable, interned, cannot be wiped).
6. **Hold related secrets in one buffer** at different offsets to stay within the locked-memory limit.
7. **Clear early.** Call `clear()` as soon as the secret is no longer needed; do not wait for garbage collection.
8. **Add defence in depth** around the library: disable core dumps globally where you can (`ulimit -c 0`, `prctl(PR_SET_DUMPABLE, 0)`), use encrypted or disabled swap, avoid forking after the secret is loaded, run unprivileged, and raise `RLIMIT_MEMLOCK` to what you actually need.

## Reporting an issue

Report suspected vulnerabilities in [issues](https://github.com/Nokkum/secure_memory/issue).
