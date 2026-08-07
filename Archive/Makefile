SHELL := /bin/bash
.DEFAULT_GOAL := help
.DELETE_ON_ERROR:

override PACKAGE := ytermusic
ifeq ($(origin VERSION),undefined)
VERSION := $(shell sed -n 's/^version = "\([^"]*\)"/\1/p' Cargo.toml | head -n 1)
else
override VERSION := $(value VERSION)
endif
ifeq ($(origin GITHUB_OWNER),undefined)
GITHUB_OWNER :=
else
override GITHUB_OWNER := $(value GITHUB_OWNER)
endif
ifeq ($(origin REPO),undefined)
REPO := ytermusic
else
override REPO := $(value REPO)
endif
override DIST_DIR := dist
override STAGE_DIR := $(DIST_DIR)/$(PACKAGE)-$(VERSION)
override ARCHIVE := $(DIST_DIR)/$(PACKAGE)-$(VERSION)-macos-universal.tar.gz
override FORMULA_TEMPLATE := packaging/homebrew/ytermusic.rb
override FORMULA_OUT := $(DIST_DIR)/ytermusic.rb
override ARM_TARGET := aarch64-apple-darwin
override INTEL_TARGET := x86_64-apple-darwin

export PACKAGE VERSION GITHUB_OWNER REPO
export DIST_DIR STAGE_DIR ARCHIVE FORMULA_TEMPLATE FORMULA_OUT
export ARM_TARGET INTEL_TARGET

.PHONY: help build build-debug run build-release check targets universal package checksum formula release-local clean _validate-version

help:
	@printf '%s\n' \
		'ytermusic development and local release targets:' \
		'  help           Show this help.' \
		'  build          Build the debug executable (alias for build-debug).' \
		'  build-debug    Build the debug executable with the lockfile.' \
		'  run            Run the debug executable with the lockfile.' \
		'  build-release  Build the native release executable with the lockfile.' \
		'  check          Check formatting, Clippy, and tests.' \
		'  targets        Install the ARM64 and Intel macOS Rust targets.' \
		'  universal      Build and stage a universal macOS executable.' \
		'  package        Archive the staged universal executable.' \
		'  checksum       Print the archive SHA-256 checksum.' \
		'  formula        Render a Homebrew formula in dist/.' \
		'  release-local  Check, package, checksum, and render the formula locally.' \
		'  clean          Remove only generated dist/ artifacts.' \
		'' \
		'Variables:' \
		'  GITHUB_OWNER   GitHub owner used by formula (required for formula).' \
		'  REPO           GitHub repository name (default: ytermusic).' \
		'  VERSION        Package version (default: first package version in Cargo.toml).'

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

_validate-version:
	@export LC_ALL=C; \
	if [[ ! "$$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?(\+[0-9A-Za-z]+([.-][0-9A-Za-z]+)*)?$$ ]]; then \
		printf '%s\n' 'error: VERSION must be a path-safe, SemVer-like version' >&2; \
		exit 2; \
	fi

targets: _validate-version
	@set -eu; \
	toolchain="$$(rustup show active-toolchain | awk '{print $$1}')"; \
	if [[ -z "$$toolchain" ]]; then printf '%s\n' 'error: rustup has no active toolchain' >&2; exit 1; fi; \
	rustup component add --toolchain "$$toolchain" cargo; \
	rustup target add --toolchain "$$toolchain" "$$ARM_TARGET" "$$INTEL_TARGET"

universal: targets
	@set -eu; \
	toolchain="$$(rustup show active-toolchain | awk '{print $$1}')"; \
	if [[ -z "$$toolchain" ]]; then printf '%s\n' 'error: rustup has no active toolchain' >&2; exit 1; fi; \
	rustc="$$(rustup which rustc --toolchain "$$toolchain")"; \
	RUSTC="$$rustc" rustup run "$$toolchain" cargo build --release --locked --target "$$ARM_TARGET"; \
	RUSTC="$$rustc" rustup run "$$toolchain" cargo build --release --locked --target "$$INTEL_TARGET"
	mkdir -p "$$STAGE_DIR"
	lipo -create \
		"target/$${ARM_TARGET}/release/$${PACKAGE}" \
		"target/$${INTEL_TARGET}/release/$${PACKAGE}" \
		-output "$$STAGE_DIR/$$PACKAGE"
	chmod +x "$$STAGE_DIR/$$PACKAGE"
	lipo -info "$$STAGE_DIR/$$PACKAGE"

package: universal
	@set -eu; \
	tmp="$${ARCHIVE}.tmp.$$$$"; \
	trap 'rm -f -- "$$tmp"' EXIT; \
	tar -C "$$STAGE_DIR" -czf "$$tmp" "$$PACKAGE"; \
	mv -f "$$tmp" "$$ARCHIVE"; \
	trap - EXIT

checksum: package
	shasum -a 256 "$$ARCHIVE"

formula: package
	@set -euo pipefail; \
	export LC_ALL=C; \
	owner="$${GITHUB_OWNER}"; \
	repo="$${REPO}"; \
	case "$$owner" in \
		''|*[!A-Za-z0-9-]*) printf '%s\n' 'error: GITHUB_OWNER must contain only ASCII letters, digits, and hyphens' >&2; exit 2 ;; \
	esac; \
	case "$$repo" in \
		''|*[!A-Za-z0-9._-]*) printf '%s\n' 'error: REPO must contain only ASCII letters, digits, dots, underscores, and hyphens' >&2; exit 2 ;; \
	esac; \
	sha="$$(shasum -a 256 "$$ARCHIVE" | awk '{print $$1}')"; \
	tmp="$${FORMULA_OUT}.tmp.$$$$"; \
	trap 'rm -f -- "$$tmp"' EXIT; \
	archive_basename="$${ARCHIVE##*/}"; \
	homepage="https://github.com/$${owner}/$${repo}"; \
	release_url="https://github.com/$${owner}/$${repo}/releases/download/v$${VERSION}/$${archive_basename}"; \
	escape_sed_replacement() { printf '%s' "$$1" | sed 's/[\\&|]/\\&/g'; }; \
	homepage_escaped="$$(escape_sed_replacement "$$homepage")"; \
	release_url_escaped="$$(escape_sed_replacement "$$release_url")"; \
	version_escaped="$$(escape_sed_replacement "$$VERSION")"; \
	sed \
		-e '/^# TEMPLATE ONLY:/d' \
		-e "s|^  homepage \".*\"|  homepage \"$${homepage_escaped}\"|" \
		-e "s|__RELEASE_URL_MACOS_UNIVERSAL__|$${release_url_escaped}|" \
		-e "s|^  version \".*\"|  version \"$${version_escaped}\"|" \
		-e "s|__SHA256_MACOS_UNIVERSAL__|$${sha}|" \
		"$$FORMULA_TEMPLATE" > "$$tmp"; \
	if grep -Eq '__[^[:space:]]+__' "$$tmp"; then \
		printf '%s\n' 'error: rendered formula still contains a template placeholder' >&2; \
		exit 1; \
	fi; \
	while IFS= read -r dependency; do \
		grep -Fqx "$$dependency" "$$tmp" || { printf 'error: rendered formula lost dependency: %s\n' "$$dependency" >&2; exit 1; }; \
	done < <(grep '^[[:space:]]*depends_on ' "$$FORMULA_TEMPLATE"); \
	mv -f "$$tmp" "$$FORMULA_OUT"; \
	trap - EXIT

release-local: check package checksum formula

clean:
	rm -rf -- "$$DIST_DIR"
