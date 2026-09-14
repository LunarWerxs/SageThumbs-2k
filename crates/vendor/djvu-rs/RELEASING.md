# Releasing djvu-rs

Releases are cut by **pushing a version tag**. The tag — not release-please — is what
triggers publication to crates.io. This is deliberate: it does **not** depend on
`RELEASE_PLEASE_TOKEN` (a PAT that expires and has silently broken a release before),
only on `CARGO_REGISTRY_TOKEN`, which crates.io requires and which should be issued
**without an expiry**.

## Standard release procedure

1. **Land a version-bump commit on `main`** — a commit titled `chore(main): release X.Y.Z`
   that bumps the workspace `version` in `Cargo.toml`, updates `CHANGELOG.md`, and sets
   `.release-please-manifest.json` to `X.Y.Z`. This normally comes from the release-please
   PR (see below), but you can also write it by hand.

2. **Push the tag** — pointing at that release commit:

   ```sh
   git tag -a vX.Y.Z <release-commit-sha> -m "Release X.Y.Z"
   git push --no-verify origin vX.Y.Z
   ```

   `--no-verify` skips the pre-push hook (a full `make check`); the commit is already on
   `main` and green, so re-running it on a tag push is wasted minutes.

3. **CI publishes** — `.github/workflows/publish.yml` fires on `push: tags: ['v*']`,
   runs the release validation, then `cargo publish` for every workspace crate. It skips
   any crate whose version already exists on crates.io, so re-pushing a tag is safe.

4. **Create the GitHub Release** (optional but recommended, since a manual tag does not
   create one):

   ```sh
   gh release create vX.Y.Z --title "vX.Y.Z" --notes-file <changelog-section>.md
   ```

## Where the version-bump commit comes from

release-please still does the tedious part — it opens a `chore(main): release X.Y.Z` PR
that accumulates the `Cargo.toml` bump and the `CHANGELOG.md` section from Conventional
Commits since the last release. Merge that PR to get the release commit on `main`, **then
push the tag yourself** (step 2 above).

> **Do not rely on release-please to create the tag / GitHub Release.** That step runs
> under `RELEASE_PLEASE_TOKEN`; when the PAT is expired the release-please workflow fails
> with `Bad credentials` and nothing gets tagged or published. The manual tag push in
> step 2 bypasses that path entirely.

If `RELEASE_PLEASE_TOKEN` is expired and you don't want to rotate it, you can also write
the release commit by hand (bump `Cargo.toml` + `.release-please-manifest.json`, edit
`CHANGELOG.md`), merge it, and proceed to step 2 — the tag flow is identical.

## Conventional Commits

Every commit message must start with a type prefix. release-please reads these to decide
the version bump when it prepares the release PR:

| Commit prefix | Version bump | Example |
|---------------|-------------|---------|
| `fix:` | patch | `fix: clamp overflow in IW44 normalize` |
| `perf:` | patch | `perf(iw44): SIMD YCbCr→RGB` |
| `docs:` | patch | `docs: add Rotation variants` |
| `chore:` | none | `chore: update CI cache` |
| `feat:` | minor | `feat: async render API` |
| `feat!:` or `BREAKING CHANGE:` in footer | major | `feat!: remove deprecated render_to_size` |

**While version is `0.x`:** `feat!` bumps minor (not major) — configured via
`bump-minor-pre-major: true` in `release-please-config.json`.

Full spec: [conventionalcommits.org](https://www.conventionalcommits.org/en/v1.0.0/)

## Version policy

Follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html):

| Change | Version bump |
|--------|-------------|
| Breaking public API change | MAJOR (`feat!` / `BREAKING CHANGE`) |
| New public API, backward-compatible | MINOR (`feat`) |
| Bug fix, performance, docs, internal | PATCH (`fix`, `perf`, `docs`, `refactor`) |

While version is `0.x`, minor bumps may include breaking changes per SemVer §4.

## Tokens

| Secret | Used by | Notes |
|--------|---------|-------|
| `CARGO_REGISTRY_TOKEN` | `publish.yml` (`cargo publish`) | **Required** — crates.io cannot publish without it. Issue it with **no expiry** at <https://crates.io/settings/tokens> so it never becomes a release blocker. |
| `RELEASE_PLEASE_TOKEN` | `release-please.yml` | Only prepares the changelog/version PR and (if you let it) the tag. **Not** on the critical path for the tag-push release flow above. If it expires, releases still go out via the manual tag. |

## Python wheels and npm packages

Tag pushes also trigger [`.github/workflows/publish-packages.yml`](.github/workflows/publish-packages.yml),
which builds version-matched Python wheels/sdists and the dual wasm npm
package, runs install-time smoke tests, writes `SHA256SUMS`, and attests
artifacts. Publishing to PyPI/npm is gated on repository variables
`PUBLISH_PYPI` / `PUBLISH_NPM` (set to `true`) plus the `pypi` / `npm`
environments — if a wheel, sdist, npm tarball, or smoke test fails, the
package gate fails and nothing is published. Full contract:
[`docs/packaging.md`](docs/packaging.md).

