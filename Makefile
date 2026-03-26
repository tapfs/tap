.PHONY: build check test clean mount unmount

export PKG_CONFIG_PATH := $(CURDIR)/.pkgconfig

build:
	cargo build

check:
	cargo check

test:
	cargo test

clean:
	cargo clean

mount:
	cargo run -- mount rest -s connectors/rest.yaml

unmount:
	cargo run -- unmount /tmp/tap
