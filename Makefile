#!/usr/bin/env bash

app-name = prux
run-env = RUST_LOG=
build-env =

all: release

build:
	$(build-env) cargo build --bin ${app-name}

release:
	$(build-env) cargo build --release --bin ${app-name}

debug:
	$(build-env) $(run-env) cargo run --bin ${app-name}

install-linux: release
	cp -f ./target/release/${app-name} ~/.local/bin/

check:
	$(build-env) cargo check

# 从本地 pi 安装目录（@earendil-works/pi-ai）同步模型数据到 assets/models
sync-models:
	python3 scripts/sync-models.py

sync-models-check:
	python3 scripts/sync-models.py --check

clean:
	cargo clean

app-name:
	echo "$(app-name)" > target/app-name
