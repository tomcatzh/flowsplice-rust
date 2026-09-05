.PHONY: web fmt check test review-regressions e2e release apple-products home2-macos-package travel-macos-package openwrt-check openwrt-ipk policy-check

web:
	cd travelagent/web && npm ci && npm run build
	cd homeagent/web && npm ci && npm run build

fmt:
	cargo fmt --all

check: web openwrt-check policy-check
	cargo fmt --all -- --check
	cargo check --workspace --all-targets
	cargo clippy --workspace --all-targets -- -D warnings

test: web openwrt-check policy-check
	cargo test --workspace --all-targets

review-regressions:
	CARGO_TARGET_DIR="$(CURDIR)/target" bash ./tests/check-travel-review-regressions.sh

e2e:
	./tests/e2e/run.sh

release:
	./scripts/build-release.sh

apple-products:
	./scripts/build-apple-products.sh

home2-macos-package:
	./scripts/build-home2-macos-package.sh

travel-macos-package:
	./scripts/build-travel-macos-package.sh

openwrt-check:
	./tests/openwrt/check.sh

policy-check:
	bash ./tests/check-docker-pull-policy.sh
	bash ./tests/check-home2-macos-package.sh
	bash ./tests/check-travel-macos-package.sh
	bash ./tests/check-apple-products.sh
	bash ./tests/check-release-feature-gates.sh
	bash ./tests/check-runtime-configuration-boundary.sh
	python3 ./tests/test_package_privacy.py
	python3 ./tests/test_private_travel_trust.py

openwrt-ipk:
	python3 scripts/build-openwrt-ipk.py \
		--server dist/linux-arm64/flowsplice-server \
		--relay dist/linux-arm64/flowsplice-relay \
		--architecture aarch64_generic \
		--version 0.3.1 \
		--output-dir dist/openwrt
