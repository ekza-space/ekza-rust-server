.PHONY: build run dev check test fmt lint install-watch

build:
	cargo build

run:
	cargo run


# Hot-reload dev server (restarts on code changes). Requires cargo-watch.
install-watch:
	cargo install cargo-watch

dev:
	@command -v cargo-watch >/dev/null 2>&1 || (echo "cargo-watch not installed. Run: make install-watch"; exit 1)
	cargo watch -w src -w Cargo.toml -w Cargo.lock -x run

check:
	cargo check

test:
	cargo test

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings
