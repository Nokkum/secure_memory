<h1 align="center">Secure Memory</h1>
<p align="center">

`secure_memory` is a Python extension implemented in Rust that provides a fixed-size memory buffer intended for temporarily storing sensitive byte data.
</p>

### It is designed for applications that need to handle values such as:
- passwords
- API keys
- authentication tokens
- encryption keys
- session secrets
- private key material
- other short-lived sensitive byte sequences

> [!IMPORTANT]
> `secure_memory` is a memory-handling primitive, not a complete secrets-management system. It cannot prevent every possible copy of a secret, nor can it guarantee that a secret never appears in swap/pagefile storage, crash dumps, interpreter internals, logs, registers, or other memory locations. Applications must still use appropriate operational and cryptographic security practices.

---

## What is secure_memory?

At its core, `secure_memory` exposes the following Python class:

```python
from secure_memory import SecureMemory
```

A `SecureMemory` instance owns a fixed-size Rust allocation:

```python
memory = SecureMemory(64)
```

The allocation initially contains zeroes.

You can then write sensitive bytes into it:

```python
memory.write(0, b"my-secret")
```

and later clear the entire allocation:

```python
memory.clear()
```

When the `SecureMemory` object is destroyed, its contents are automatically zeroized again.

The important design difference from ordinary Python byte strings is that the primary storage remains inside the Rust object. The API also provides read-only buffer access so an application can process the contents without automatically creating another Python `bytes` object.
