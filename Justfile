default: check

# Run formatting, lint, and tests. Fail on any warning.
check: fmt-check clippy test

# Run all checks and apply formatting fixes (clippy --fix is not run here).
fix:
  cargo fmt --all
  cargo clippy --workspace --all-targets --fix --allow-dirty --allow-staged

fmt-check:
  cargo fmt --all -- --check

clippy:
  cargo clippy --workspace --all-targets -- -D warnings

test:
  cargo test --workspace

# Run Kani proof harnesses (cfg(kani) modules). Requires cargo-kani.
kani *ARGS:
  cargo kani -p seal-core {{ARGS}}

build:
  cargo build --release

install:
  cargo install --locked --force --path crates/seal-cli
