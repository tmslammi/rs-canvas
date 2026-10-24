# Release process

rs-canvas ships alpha builds as GitHub pre-releases. The release assets are the
installable distribution; a source checkout runs the app with `npm run dev`.

## Versioning

Versions follow SemVer with an explicit alpha counter:

```text
0.1.0-alpha.1
0.1.0-alpha.2
0.2.0-alpha.1
```

- While the project is `0.x`, a breaking change bumps the minor version.
- A fix or rebuild of the same feature set bumps the alpha number.
- A release that changes the document schema must include a migration and pass
  the old-document tests. Alpha can break features; it should not silently
  break saved documents.

The tag is the version with a `v` prefix: `v0.1.0-alpha.1`. The tag, the Rust
workspace version and `web/package.json` must match; the release workflow
refuses to publish when they do not.

## Cutting a release

1. Bump the version in `Cargo.toml` (`[workspace.package]`) and `web/package.json`
   (and its lockfile):
   ```sh
   npm --prefix web version 0.1.0-alpha.2 --no-git-tag-version
   ```
   Update `Cargo.lock` with `cargo check --workspace`.
2. Commit the bump, merge it to `master`, and wait for CI to pass.
3. Tag the green commit and push the tag:
   ```sh
   git tag -a v0.1.0-alpha.2 -m "rs-canvas v0.1.0-alpha.2"
   git push origin v0.1.0-alpha.2
   ```
4. The `Release` workflow runs CI, builds the macOS binaries, smoke-tests the
   packaged executable, and publishes a GitHub pre-release with:
   - `rs-canvas-aarch64-apple-darwin.tar.gz`
   - `rs-canvas-x86_64-apple-darwin.tar.gz`
   - `SHA256SUMS`
   - `install.sh`
   The build job runs `scripts/test-install.sh` against each tarball with a
   local mock release before uploading.
5. Verify the install from a clean directory:
   ```sh
   curl -fsSL https://raw.githubusercontent.com/tmslammi/rs-canvas/master/install.sh | bash
   ~/.local/bin/rs-canvas --version
   ```
   The installer adds `~/.local/bin` to the profile; use the full path in the
   current shell until it is reloaded.

The workflow can also be run manually with an existing tag
(`Actions -> Release -> Run workflow`). A re-run replaces the assets of an
existing draft; a published release is immutable, so bump the alpha number
instead.

The `install.sh` attached to a release has that release's version baked in, so
it is a stable rollback target. The copy on `master` resolves the newest
release for the one-line install.

## What is in the binary

The macOS release build uses `--features embed-web`. It compiles `web/dist`
(including the wasm bundle and fonts) into the executable, so there is no
checkout beside it and no Node or Rust toolchain on the installing machine.
The binary still accepts `--web <dir>` to serve an unpacked front end during
development.

Release builds:

- pin the Rust toolchain in `rust-toolchain.toml`;
- pin `wasm-bindgen-cli` to the version in `Cargo.lock`;
- build with `--locked`;
- default to `~/.rs-canvas/current.rsc`, with workspaces beside it;
- open the browser unless `--no-open` is passed.

## Installing an older release

The installer accepts a version, so rolling back needs no extra tooling:

```sh
curl -fsSL https://raw.githubusercontent.com/tmslammi/rs-canvas/master/install.sh \
  | bash -s -- --version 0.1.0-alpha.1
```

GitHub's `releases/latest` endpoint ignores pre-releases, so the installer
looks at the newest entry in the releases list. When the project starts
publishing stable releases, add a channel flag instead of relying on that
behaviour.

## Platforms

macOS 12 or newer on Apple Silicon and Intel are the supported alpha targets.
Linux and Windows need their own build matrix rows and platform smoke tests
before they are advertised. Homebrew, auto-update, notarisation and code
signing are intentionally out of scope until more than one person installs the
build.
