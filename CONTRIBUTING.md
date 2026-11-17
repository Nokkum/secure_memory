# Contributing to `secure_memory`

Contributions to this project are released to the public under the [project's license](LICENSE).

By participating in this project you agree to abide by its terms whether  you want to submit a bug report, give suggestions or help with development, all we ask is that you follow the guidelines below.

# Code of Conduct

You can read our [Code of Conduct](CODE_OF_CONDUCT.md)

## Reporting Problems

- **Bugs and questions:** open an issue. Include the `secure_memory` version, your Python version and operating system, and a short snippet that reproduces the problem.
- **Security vulnerabilities:** do not open a public issue. Follow [Reporting security vulnerabilities](#reporting-security-vulnerabilities).

Never paste real secrets, keys, tokens or passwords into an issue, pull request, log, test or example.

### Reporting security vulnerabilities

If you think you have found a vulnerability, for example a way a secret can leak, memory that is not wiped when it should be, or a memory-safety bug, report it privately:

- use GitHub's private vulnerability reporting for this repository (the Security tab, then "Report a vulnerability") if it is enabled, or
- email <nokkum.github@gmail.com>.

Tell us what happens, how to reproduce it, and what platform you used. We will acknowledge your report and keep you updated. Please give us a reasonable amount of time to fix the problem before you disclose it publicly.

## Submitting a Pull Request

1. Fork and clone the repository.
2. Create a new branch: `git checkout -b my-branch-name`
3. Read the page of the [documentation](docs/README.md) that covers the area you are changing, for example [architecture](docs/architecture.md) or [security](docs/security.md).
4. Make your change, with tests.
5. Run the checks:

   ```bash
   cargo fmt --all --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test
   maturin develop
   python -m pytest
   ```

6. Push to your fork and submit a pull request. Fill in the pull request template.
