- [ ] New or changed behaviour has Rust unit tests and, if users can see it, Python tests.
- [ ] Any new or changed `unsafe` block relies on an invariant listed in [Architecture](https://github.com/Nokkum/secure_memory/tree/main/docs/architecture.md), or adds one there.
- [ ] The pull request says which platforms (Linux, macOS, Windows) the change affects.
- [ ] No real secrets, keys, or tokens appear in code, tests, logs, or examples.
- [ ] Described how the change affects the guarantees in [Security](https://github.com/Nokkum/secure_memory/tree/main/docs/security.md), or confirmed that it does not.
- [ ] Failure paths (lock refused, allocation failed, views still alive) behave as documented and are covered by tests.

**Please note: we will close your PR without comment if you do not check the boxes above and provide ALL requested information.**
