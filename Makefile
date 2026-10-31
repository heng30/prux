#!/usr/bin/env bash

app-name = prux
app-name-gui = ${app-name}-gui
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
	cp -f ~/.local/bin/${app-name} ~/.local/bin/${app-name}.bak 2>/dev/null || true
	cp -f ./target/release/${app-name} ~/.local/bin/

build-gui:
	$(build-env) cargo build --bin ${app-name-gui} --features=gui

release-gui:
	$(build-env) cargo build --release --bin ${app-name-gui} --features=gui

debug-gui:
	$(build-env) $(run-env) cargo run --bin ${app-name-gui} --features=gui

install-linux-gui: release-gui
	cp -f ~/.local/bin/${app-name-gui} ~/.local/bin/${app-name-gui}.bak 2>/dev/null || true
	cp -f ./target/release/${app-name-gui} ~/.local/bin/

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

publish:
	cargo publish --allow-dirty

publish-test:
	cargo publish --dry-run --registry crates-io --allow-dirty
