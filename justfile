build:
    cargo build --release --locked
check:
    cargo fmt --all --check
    cargo check --locked --all-targets
    cargo clippy --locked --all-targets -- -D warnings
test:
    cargo test --locked
clean:
    cargo clean
