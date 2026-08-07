# Local Release Makefile Design

## Problem

Building and preparing a Homebrew release currently requires a sequence of
manual Cargo, `lipo`, archive, checksum, and template-substitution commands.
That sequence is easy to mistype and does not provide a single discoverable
entry point for normal debug development or local release preparation.

The repository needs a small Makefile that standardizes these local operations
without creating tags, pushing commits, or publishing GitHub releases.

## Goals

- Provide discoverable debug, release, verification, and packaging targets.
- Build a macOS universal binary from ARM64 and Intel release binaries.
- Create a versioned release archive under `dist/` with a fixed,
  version-derived name and single-binary content layout.
- Generate a publishable Homebrew formula from the existing template using an
  explicit GitHub owner and the computed archive checksum.
- Leave source templates unchanged and confine generated artifacts to `dist/`.
- Never perform external publishing actions.

## Approaches Considered

### Release-focused Makefile with development targets (recommended)

Provide debug build/run commands alongside verification, universal release
packaging, checksum, and formula generation. This covers the repetitive local
workflow while keeping each operation explicit and composable.

### Minimal build-only Makefile

Only wrap Cargo debug and release commands. This has less code but leaves the
error-prone universal packaging and formula rendering manual.

### Full publisher Makefile

Also create Git tags and GitHub releases. This reduces commands further but
mixes reversible local preparation with external state changes. Publishing is
intentionally outside this design.

## Design

The default target is `help`, which documents every supported target and the
variables required for formula generation.

Development targets:

- `build` and `build-debug` build the native debug executable.
- `run` launches the debug executable through Cargo.
- `build-release` builds the native optimized executable.
- `check` runs formatting verification, strict Clippy, and the complete
  all-target/all-feature test suite.

Release targets:

- `universal` ensures both macOS Rust targets are installed, builds locked
  release binaries for `aarch64-apple-darwin` and `x86_64-apple-darwin`, and
  combines them with `lipo` into `dist/ytermusic-<version>/ytermusic`.
- `package` archives that universal executable as
  `dist/ytermusic-<version>-macos-universal.tar.gz`. The archive has a fixed,
  version-derived name and contains only the top-level `ytermusic` executable.
  This does not promise byte-for-byte reproducible gzip output.
- `checksum` prints the archive SHA-256.
- `formula` requires `GITHUB_OWNER`, computes the archive checksum, and renders
  `dist/ytermusic.rb` from `packaging/homebrew/ytermusic.rb`. It replaces the
  homepage, release URL, checksum, and template warning while retaining all
  declared runtime dependencies. The formula hashes the archive produced by
  the packaging run.
- `release-local` composes `check`, `package`, `checksum`, and `formula`.
- `clean` removes only the repository-local `dist/` directory.

`VERSION` defaults to the package version in `Cargo.toml`, while callers may
override it explicitly. `REPO` defaults to `ytermusic`. Formula generation
fails with a clear message when `GITHUB_OWNER` is absent. Packaging targets fail
normally if required macOS tools such as `rustup`, `lipo`, or `tar` are missing;
they do not silently produce partial artifacts.

## Safety and Error Handling

- No target invokes `git tag`, `git push`, `gh`, or any release-upload API.
- The default target performs no build or mutation.
- Formula output is separate from the tracked template.
- Commands use Cargo's locked dependency graph for release artifacts.
- Targets stop on the first failing command.
- Cleanup has one explicit, repository-scoped target: `dist/`.

## Testing

- Add documentation/contract tests that assert the Makefile exists, contains
  every promised target, retains all Homebrew runtime dependencies, and omits
  publishing commands.
- Run `make help` and verify it performs no build.
- Run debug and release build targets.
- Run `make check`.
- Run universal packaging on macOS and inspect the binary with `lipo -info`.
- Generate a formula with a fixture owner, verify that no placeholders remain,
  and run Ruby syntax checking plus Homebrew audit where practical.
- Verify `git status` contains no generated files outside ignored `dist/`.
