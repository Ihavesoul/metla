.PHONY: build release check test fmt clippy run scan

build:
	cargo build

release:
	cargo build --release

check:
	cargo fmt
	cargo clippy --all-targets -- -D warnings
	cargo test

test:
	cargo test

fmt:
	cargo fmt

clippy:
	cargo clippy --all-targets -- -D warnings

run:
	cargo run --release --

scan:
	cargo run --release -- --scan-only
