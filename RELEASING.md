# Releasing

Releases are built by a GitHub Actions workflow and published as GitHub Releases.

## Prerequisites

- Push access to the repository
- A changelog entry for the version being released (stable releases only)
- `cargo-about` installed (for license generation): `cargo install cargo-about`

## Steps

### 1. Update the changelog

Move items from `## Unreleased` into a new version section in `CHANGELOG.md`:

```markdown
## v0.1.0

### Added
- ...
```

Commit this to the main branch and push.

### 2. Trigger the release workflow

Go to **Actions > Release > Run workflow** on GitHub and enter the version. Accepted formats:

- `v0.1.0` — stable release (requires changelog entry)
- `v0.1.0-alpha.1`, `v0.1.0-rc.1` — pre-release (changelog not required)

The workflow will:

1. Validate the version format
2. Verify a matching changelog entry exists (stable releases only)
3. Create and push a git tag
4. Run the full CI pipeline (`mise run ci`) on Linux, macOS, and Windows:
   - Install npm dependencies (`npm ci`)
   - Lint and type-check (cargo fmt, clippy, svelte-check)
   - Run tests (cargo test, vitest)
   - Build the frontend SPA
   - Regenerate third-party license notices
   - Build the release binary
5. Package binaries into platform-specific archives
6. Extract release notes from the changelog (or generate a placeholder for pre-releases)
7. Create a GitHub Release with the archives attached

### 3. Review

Check the [Releases page](https://github.com/alphaleonis/erinra-mcp/releases) to verify the release was created correctly.

## What's in the archives

Each release archive contains a single `erinra` binary (or `erinra.exe` on Windows). Third-party license notices are embedded in the binary and accessible via `erinra licenses`.

## Platforms

| Archive | Target |
|---------|--------|
| `erinra-vX.Y.Z-x86_64-linux.tar.gz` | Linux x86_64 |
| `erinra-vX.Y.Z-aarch64-darwin.tar.gz` | macOS Apple Silicon |
| `erinra-vX.Y.Z-x86_64-windows.zip` | Windows x86_64 |

## Local testing

To test the release build locally:

```bash
# Full CI pipeline (install, check, test, build release)
mise run ci

# Or just the release build (skip checks/tests)
mise run build:release

# Regenerate license file after dependency changes
mise run licenses
```
