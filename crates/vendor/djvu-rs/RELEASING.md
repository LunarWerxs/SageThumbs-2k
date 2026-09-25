# Releasing djvu-rs

Releases are cut by **merging the release-please PR**. Everything after that merge is
automatic and runs on the built-in `GITHUB_TOKEN` — no personal access token is on the
release path.

## Standard release procedure

1. **Review the release PR** — release-please keeps one open, titled
   `chore(main): release X.Y.Z`. It bumps the workspace `version` in `Cargo.toml`,
   the Python and npm package versions, `.release-please-manifest.json`, and adds the
   `CHANGELOG.md` section built from Conventional Commits since the last release.

2. **Wait for the checks, then merge normally.** The PR is opened by `GITHUB_TOKEN`, and
   GitHub does not start `pull_request` workflows for such events. The `dispatch-ci` job
   in [`.github/workflows/release-please.yml`](.github/workflows/release-please.yml)
   works around that: it starts `ci.yml` on the release branch through
   `workflow_dispatch`, which is exempt from the same guard. Those check runs land on the
   PR head commit, so the required checks turn green and an admin bypass is not needed.

3. **The rest happens on its own.** Merging pushes a `chore(main): release X.Y.Z` commit
   to `main`, which starts `release-please.yml` again. That run:
   - creates the `vX.Y.Z` tag and the GitHub Release;
   - publishes all eight crates to crates.io in dependency order, skipping any version
     already there, then waits for each to go live;
   - starts [`.github/workflows/publish-packages.yml`](.github/workflows/publish-packages.yml)
     through `workflow_dispatch` for the Python wheels and the npm package. The tag push
     itself cannot start it, because that push is also authored by `GITHUB_TOKEN`.

4. **Check the result** — the tag, the release notes, the eight crates on crates.io, and
   the package run:

   ```sh
   gh release view vX.Y.Z
   gh run list --workflow=publish-packages.yml --limit 1
   ```

## Emergency release without release-please

`publish.yml` still listens for `push: tags: ['v*']` and for a manual
`workflow_dispatch` with a `tag` input. Write the version-bump commit by hand, merge it,
then:

```sh
git tag -a vX.Y.Z <release-commit-sha> -m "Release X.Y.Z"
git push --no-verify origin vX.Y.Z
```

`--no-verify` skips the pre-push hook (a full `make check`); the commit is already on
`main` and green, so re-running it on a tag push is wasted minutes. A tag pushed by a
person — not by `GITHUB_TOKEN` — does start both publish workflows.

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
| `CARGO_REGISTRY_TOKEN` | `release-please.yml` and `publish.yml` (`cargo publish`) | **Required** — crates.io cannot publish without it. Issue it with **no expiry** at <https://crates.io/settings/tokens> so it never becomes a release blocker. |
| `GITHUB_TOKEN` | `release-please.yml` | Built in, nothing to rotate. It opens the release PR, tags, releases, publishes to crates.io, and dispatches the other two workflows. A `RELEASE_PLEASE_TOKEN` PAT was used once, expired, and silently broke the 0.25.0 release with `Bad credentials`; it is no longer used. |

## Python wheels and npm packages

[`.github/workflows/publish-packages.yml`](.github/workflows/publish-packages.yml),
started by the release run (see step 3), builds version-matched Python wheels/sdists and the dual wasm npm
package, runs install-time smoke tests, writes `SHA256SUMS`, and attests
artifacts. Publishing to PyPI/npm is gated on repository variables
`PUBLISH_PYPI` / `PUBLISH_NPM` (set to `true`) plus the `pypi` / `npm`
environments — if a wheel, sdist, npm tarball, or smoke test fails, the
package gate fails and nothing is published. Full contract:
[`docs/packaging.md`](docs/packaging.md).

