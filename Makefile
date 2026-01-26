.PHONY: build run check test fmt lint

build:
	cargo build

run:
	cargo run

check:
	cargo check

test:
	cargo test

fmt:
	cargo fmt --all

lint:
	cargo clippy --all-targets -- -D warnings
