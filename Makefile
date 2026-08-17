.PHONY: web fmt check test e2e release

web:
	cd travelagent/web && npm ci && npm run build

fmt:
	cargo fmt --all

check: web
	cargo fmt --all -- --check
	cargo check --workspace --all-targets
	cargo clippy --workspace --all-targets -- -D warnings

test: web
	cargo test --workspace --all-targets

e2e:
	./tests/e2e/run.sh

release:
	./scripts/build-release.sh
