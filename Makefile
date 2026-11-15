# Merge gate (AGENTS.md): fmt + clippy + test + release build must pass before every merge.
check:
	cargo fmt --all --check
	cargo clippy --workspace --all-targets -- -D warnings
	cargo test --workspace
	cargo build --release --workspace


# Supply-chain gates — local mirrors of the
# ci.yml workflow jobs. They fail loudly when the tool is missing instead of
# silently skipping the gate.

deny:
	@command -v cargo-deny >/dev/null 2>&1 || { echo "cargo-deny not installed (cargo install cargo-deny --locked)"; exit 1; }
	cargo deny --all-features --workspace check advisories licenses

# Windows cfg-hygiene gate: cross-target check +
# clippy at -D warnings for every crate and test. Fails loudly when the
# target is missing instead of silently skipping the gate.
windows-cross:
	@rustup target list --installed | grep -q x86_64-pc-windows-gnu || { echo "x86_64-pc-windows-gnu target not installed (rustup target add x86_64-pc-windows-gnu)"; exit 1; }
	cargo check --workspace --target x86_64-pc-windows-gnu --all-targets
	cargo clippy --workspace --target x86_64-pc-windows-gnu --all-targets -- -D warnings

# Builds the Docker image (Dockerfile): the agent compiled from the pinned
# toolchain, kernel venv pre-built, non-root user. Fails loudly when docker
# is missing instead of silently skipping.
docker-image:
	@command -v docker >/dev/null 2>&1 || { echo "docker not installed (see docs.docker.com/get-docker)"; exit 1; }
	docker build -t unharnessed .

# Lints the live workflow file (.github/workflows/).
actionlint:
	@command -v actionlint >/dev/null 2>&1 || { echo "actionlint not installed (see rhysd/actionlint releases)"; exit 1; }
	actionlint .github/workflows/ci.yml

.PHONY: check deny windows-cross actionlint docker-image
