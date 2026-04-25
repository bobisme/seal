# Releasing seal

seal uses tag-driven release automation. Pushing a `vX.Y.Z` tag fires `.github/workflows/release.yml`, which:

1. Runs `just check` (fmt, clippy, tests) as a gate
2. Cross-compiles the `seal` binary for `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, and `aarch64-apple-darwin`
3. Publishes crates to crates.io in dependency order: `seal-core` → `seal-tui` → `seal-cli`, with retry + a brief sleep between each so the index has time to update
4. Creates a GitHub Release with the binary tarballs + sha256s and an auto-generated changelog from `git log <prev-tag>..<tag>`

The publish step intentionally skips a `cargo publish --dry-run` preflight for `seal-tui` and `seal-cli` — that dry-run fails before the upstream internal crate (`seal-core`) at the same version exists on crates.io.

## Required GitHub secret

- `CARGO_REGISTRY_TOKEN` — crates.io API token, scoped to `publish-update` for `seal-core`, `seal-tui`, `seal-cli`. Add via `gh secret set CARGO_REGISTRY_TOKEN --repo bobisme/seal`.

## Release checklist

1. Bump `[workspace.package].version` in the root `Cargo.toml`
2. Bump the matching `version = "..."` on the internal `seal-core` and `seal-tui` workspace deps in the same file
3. Run `just check` (regenerates `Cargo.lock` if needed and verifies fmt/clippy/tests)
4. Add a `## [X.Y.Z] - YYYY-MM-DD` entry to `CHANGELOG.md` and update the link references at the bottom
5. Commit (`chore: bump version to X.Y.Z`) and merge to `main`
6. Tag and push with `maw release vX.Y.Z`

Once the tag is pushed, the workflow handles binaries, crates.io publishes, and the GitHub Release. You can also fire `workflow_dispatch` manually from the Actions tab to exercise the test + build matrix without publishing.

## Manual fallback

If CI is broken and you need to publish from a workstation:

```bash
cargo publish -p seal-core   # wait for it to land
cargo publish -p seal-tui    # wait
cargo publish -p seal-cli
```

Each crate must be at least 30s apart so the registry index has time to refresh — otherwise the next `cargo publish` will fail to resolve the freshly-uploaded sibling crate.
