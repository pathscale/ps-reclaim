# Local checks before publishing

Publishing is a manual action from `master`. Before dispatching it, review the
exact commit, run `cargo publish --dry-run`, and attest that the applicable
checks below passed. The manual Actions workflow remains available for the
feature, platform, Miri, and Loom jobs.

```sh
cargo fmt --all -- --check
cargo clippy --lib --bins --tests -- -D warnings
cargo test --verbose
cargo check --lib --no-default-features
cargo test --no-default-features
cargo test --no-default-features --features libc,spin
cargo test --no-default-features --features libc,spin,spin-garbage
cargo test --features spin-garbage
cargo test --no-default-features --features libc,spin,inline-tsd
RUSTFLAGS="--cfg ps_loom" cargo test --test loom_model --test loom_protocol
cargo publish --dry-run
```

Run the Loom models for changes to pinning, retirement, scanning, or their
ordering contracts. Run the feature configurations relevant to supported
consumer builds; `inline-tsd` must also build and test on targets where it
falls back to the libc TLS call. The manual workflow retains the nightly TLS,
Miri, and Windows/macOS runtime checks for changes that affect those paths.
