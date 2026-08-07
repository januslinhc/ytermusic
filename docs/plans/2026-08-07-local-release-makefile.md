# Local Release Makefile Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Add a safe, discoverable Makefile for debug development and local macOS/Homebrew release preparation without publishing anything externally.

**Architecture:** Keep orchestration in one root Makefile whose default target is read-only help. Compose Cargo verification and build targets into macOS universal packaging, then render a separate `dist/ytermusic.rb` from the tracked Homebrew template using validated variables and a computed checksum.

**Tech Stack:** GNU Make 3.81-compatible syntax, Cargo/Rust 1.97, rustup, macOS `lipo`, BSD `tar`, `shasum`, Ruby/Homebrew formula template, existing Rust documentation tests.

---

### Task 1: Specify the Makefile safety and target contract

**Files:**
- Modify: `tests/docs.rs:20-35`
- Modify: `tests/docs.rs:1280-1310`

**Step 1: Add the Makefile to required release artifacts**

Rename `TASK17_ARTIFACTS` to a non-task-specific name such as
`RELEASE_ARTIFACTS`, increase its length, and add `"Makefile"`. Update its
existing uses without changing their behavior.

**Step 2: Add a failing Makefile contract test**

Add a test named `local_release_makefile_is_debug_capable_dependency_aware_and_nonpublishing`.
Read `Makefile` with `read_artifact`, then require:

```rust
for target in [
    "help:",
    "build:",
    "build-debug:",
    "run:",
    "build-release:",
    "check:",
    "universal:",
    "package:",
    "checksum:",
    "formula:",
    "release-local:",
    "clean:",
] {
    assert!(makefile.contains(target), "Makefile must define {target}");
}
```

Also require the Makefile to contain:

- `cargo build --locked` for debug builds;
- `cargo build --release --locked` for native release builds;
- both `aarch64-apple-darwin` and `x86_64-apple-darwin`;
- `cargo fmt --all -- --check`;
- `cargo clippy --all-targets --all-features -- -D warnings`;
- `cargo test --all-targets --all-features --quiet`;
- all four Homebrew dependency names through the tracked template contract;
- `GITHUB_OWNER`, `packaging/homebrew/ytermusic.rb`, `lipo`, and `shasum`;
- an explicit `/dist/` entry in `.gitignore`.

Reject external publication strings:

```rust
for forbidden in ["gh release create", "git push", "git tag"] {
    assert!(
        !makefile.contains(forbidden),
        "Makefile must not publish through {forbidden:?}"
    );
}
```

**Step 3: Run the focused test to verify RED**

```bash
cargo test --test docs local_release_makefile_is_debug_capable_dependency_aware_and_nonpublishing -- --nocapture
```

Expected: FAIL because `Makefile` does not exist.

**Step 4: Commit the failing contract**

```bash
git add tests/docs.rs
git commit -m "test: specify local release Makefile contract"
```

### Task 2: Implement debug and local release targets

**Files:**
- Create: `Makefile`
- Modify: `.gitignore:1-20`

**Step 1: Ignore generated artifacts**

Add this repository-root entry near `/target/`:

```gitignore
/dist/
```

Do not ignore release templates or any broader directory.

**Step 2: Add stable variables and target declarations**

Create a GNU Make 3.81-compatible root `Makefile` beginning with:

```make
SHELL := /bin/bash
.DEFAULT_GOAL := help
.DELETE_ON_ERROR:

PACKAGE := ytermusic
VERSION ?= $(shell sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
GITHUB_OWNER ?=
REPO ?= ytermusic
DIST_DIR := dist
STAGE_DIR := $(DIST_DIR)/$(PACKAGE)-$(VERSION)
ARCHIVE := $(DIST_DIR)/$(PACKAGE)-$(VERSION)-macos-universal.tar.gz
FORMULA_TEMPLATE := packaging/homebrew/ytermusic.rb
FORMULA_OUT := $(DIST_DIR)/ytermusic.rb
ARM_TARGET := aarch64-apple-darwin
INTEL_TARGET := x86_64-apple-darwin

.PHONY: help build build-debug run build-release check targets universal package checksum formula release-local clean
```

Do not add publishing commands or implicit network upload behavior.

**Step 3: Implement development and verification targets**

Use exact commands:

```make
build: build-debug

build-debug:
	cargo build --locked

run:
	cargo run --locked

build-release:
	cargo build --release --locked

check:
	cargo fmt --all -- --check
	cargo clippy --all-targets --all-features -- -D warnings
	cargo test --all-targets --all-features --quiet
```

The `help` target prints every public target and documents
`GITHUB_OWNER`, `REPO`, and `VERSION`. It must not invoke Cargo or create files.

**Step 4: Implement universal packaging**

Use release builds with the lockfile:

```make
targets:
	rustup target add $(ARM_TARGET) $(INTEL_TARGET)

universal: targets
	cargo build --release --locked --target $(ARM_TARGET)
	cargo build --release --locked --target $(INTEL_TARGET)
	mkdir -p "$(STAGE_DIR)"
	lipo -create \
		target/$(ARM_TARGET)/release/$(PACKAGE) \
		target/$(INTEL_TARGET)/release/$(PACKAGE) \
		-output "$(STAGE_DIR)/$(PACKAGE)"
	chmod +x "$(STAGE_DIR)/$(PACKAGE)"
	lipo -info "$(STAGE_DIR)/$(PACKAGE)"

package: universal
	tar -C "$(STAGE_DIR)" -czf "$(ARCHIVE)" "$(PACKAGE)"

checksum: package
	shasum -a 256 "$(ARCHIVE)"
```

Quote all generated paths. `universal` and later targets are macOS-specific and
must fail normally when `lipo` is unavailable.

**Step 5: Render the formula without changing the template**

`formula` depends on `package`. Its recipe must:

1. reject an empty or unsafe `GITHUB_OWNER` (allow ASCII letters, digits, and
   hyphens);
2. reject an unsafe `REPO` (allow ASCII letters, digits, dots, underscores, and
   hyphens);
3. compute the archive SHA-256 with `shasum -a 256`;
4. use `sed` to remove the first `TEMPLATE ONLY` comment and replace homepage,
   URL, version, and SHA placeholders;
5. write a temporary file under `dist/`, confirm no `__...__` placeholder
   remains, then atomically rename it to `dist/ytermusic.rb`.

The rendered URL is:

```text
https://github.com/$(GITHUB_OWNER)/$(REPO)/releases/download/v$(VERSION)/$(notdir $(ARCHIVE))
```

Keep every `depends_on` line from the tracked template.

**Step 6: Compose and clean**

```make
release-local: check package checksum formula

clean:
	rm -rf -- "$(DIST_DIR)"
```

The `clean` target must name only `$(DIST_DIR)` and must not use unresolved
globs or broader paths.

**Step 7: Run focused tests and target smoke tests**

```bash
cargo fmt --all
cargo test --test docs local_release_makefile_is_debug_capable_dependency_aware_and_nonpublishing -- --nocapture
make help
test ! -e dist
make build-debug
make build-release
```

Expected: all commands pass; `make help` does not create `dist/`.

**Step 8: Commit the implementation**

```bash
git add Makefile .gitignore
git commit -m "build: add local release Makefile"
```

### Task 3: Validate packaging and document usage

**Files:**
- Modify: `README.md:20-60`
- Modify: `tests/docs.rs:1180-1280`

**Step 1: Add a failing README contract**

Extend the documentation tests to require the source-build/install section to
mention:

```text
make build
make build-release
make release-local GITHUB_OWNER=
```

It must state that `release-local` prepares local artifacts and does not create
tags or upload releases.

**Step 2: Run the README test to verify RED**

```bash
cargo test --test docs readme_documents_local_makefile_workflow -- --nocapture
```

Expected: FAIL because README does not yet document the Makefile.

**Step 3: Document the workflow**

Add a compact `Makefile workflow` subsection near source installation:

```text
make build
make run
make build-release
make check
make release-local GITHUB_OWNER=your-github-name
```

Explain that debug targets are native, universal packaging uses optimized ARM64
and Intel builds, output is written under `dist/`, and publication/tagging stays
manual.

**Step 4: Run documentation tests**

```bash
cargo fmt --all
cargo test --test docs -- --nocapture
```

Expected: all documentation and packaging tests pass.

**Step 5: Exercise the generated artifacts on macOS**

```bash
make clean
make package
lipo -info "dist/ytermusic-$(sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)/ytermusic"
make formula GITHUB_OWNER=example-owner
ruby -c dist/ytermusic.rb
! rg '__[A-Z0-9_]+__|TEMPLATE ONLY' dist/ytermusic.rb
for dependency in mpv yt-dlp ffmpeg deno; do rg "depends_on \"$dependency\"" dist/ytermusic.rb; done
```

Expected: the universal binary contains both architectures, the archive and
formula exist, Ruby syntax is valid, no release placeholder remains, and all
runtime dependencies remain declared.

**Step 6: Verify the complete project**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features --quiet
git diff --check 9add6cb..HEAD
git status --short
```

Generated `dist/` files must not appear in status. Expected: all commands pass
and the feature worktree is clean.

**Step 7: Commit documentation**

```bash
git add README.md tests/docs.rs
git commit -m "docs: explain local Makefile workflow"
```

**Step 8: Request independent review**

Review the full range from `9add6cb` through `HEAD` against
`docs/plans/2026-08-07-local-release-makefile-design.md`. Resolve every Critical
or Important finding before branch completion.
