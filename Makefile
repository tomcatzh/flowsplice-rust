.PHONY: web fmt check test e2e release openwrt-check openwrt-ipk

web:
	cd travelagent/web && npm ci && npm run build

fmt:
	cargo fmt --all

check: web openwrt-check
	cargo fmt --all -- --check
	cargo check --workspace --all-targets
	cargo clippy --workspace --all-targets -- -D warnings

test: web openwrt-check
	cargo test --workspace --all-targets

e2e:
	./tests/e2e/run.sh

release:
	./scripts/build-release.sh

openwrt-check:
	./tests/openwrt/check.sh

openwrt-ipk:
	python3 scripts/build-openwrt-ipk.py \
		--server dist/linux-arm64/flowsplice-server \
		--relay dist/linux-arm64/flowsplice-relay \
		--architecture aarch64_generic \
		--version 0.1.0 \
		--output-dir dist/openwrt
