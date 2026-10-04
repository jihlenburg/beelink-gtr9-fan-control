# Repository Guidelines

## Project Structure & Module Organization

This project is a dependency-free Rust 2024 daemon for Beelink GTR9 Pro fan control. `src/main.rs` contains configuration parsing, hardware discovery, control logic, signal handling, CLI commands, and inline unit tests. `gtr9-fan-control.conf` provides the calibrated example configuration. `systemd/gtr9-fan-control.service` defines service startup and restoration. `install.sh` and `uninstall.sh` manage installation; `README.md` documents operation. Build artifacts belong in ignored `target/`.

## Build, Test, and Development Commands

Use Rust 1.85 or newer.

- `cargo build --release`: build `target/release/gtr9-fan-control`.
- `cargo test`: run unit tests without accessing real fan hardware.
- `cargo run -- validate gtr9-fan-control.conf`: validate the local configuration.
- `cargo clippy --all-targets -- -D warnings`: require clean lint checks.
- `cargo fmt --check`: check default formatting.
- `sudo ./install.sh`: install the release binary, configuration, and systemd unit; it does not start the service.

On matching hardware, inspect readings with `gtr9-fan-control status`. Start only after calibration: `sudo systemctl enable --now gtr9-fan-control`.

## Coding Style & Naming Conventions

Follow applicable [Linux kernel Rust coding guidelines](https://docs.kernel.org/rust/coding-guidelines.html). Use default `rustfmt`, four-space indentation, and vertical grouped imports; trailing `//` preserves their layout. Use standard Rust naming conventions. Write sentence-style Markdown comments and `///` item documentation. Precede every unsafe block with a `// SAFETY:` justification. Prefer fallible validation, checked conversions, and narrowly scoped `#[expect(...)]` over `#[allow(...)]`. Preserve temperature/duty types and centralized hardware writes. This userspace daemon uses `std` and Cargo; kernel APIs and kernel-specific FFI aliases do not apply. Shell scripts use POSIX `sh` and `set -eu`.

## Testing Guidelines

Tests use Rust's built-in `#[test]` framework in `src/main.rs`; no coverage threshold is configured. Name tests descriptively, such as `rejects_decreasing_duty`. Add regression tests for parsing, threshold boundaries, sensor failures, fan-stop transitions, and restoration behavior. Simulate sysfs files in temporary directories and clean them up. Run tests, formatting, strict Clippy, release build, and configuration validation before submitting changes.

## Commit & Pull Request Guidelines

Existing commits use concise imperative subjects, such as `Add read-only sensor status and JSON output`. Follow that style. Describe the problem, behavior changes, validation performed, and relevant issues in pull requests. For hardware changes, include hardware, configuration, and temperature/RPM observations.

## Hardware Safety & Configuration

Preserve paired `pwm2`/`pwm3` writes, full-duty fail-safe behavior, and firmware restoration on exit. Only fan3 has a usable tachometer. Calibrated duty values and automatic start PWM are machine-specific. Keep `sudo gtr9-fan-control restore` available during hardware testing; update configuration documentation when defaults change.
