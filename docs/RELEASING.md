# Releasing Kuulu

Releases are built by `.github/workflows/release.yml` from a git tag. The tag
is the only place the version is set: the workflow writes it into
`apps/desktop/package.json`, which `tauri.conf.json` reads. The version in the
repository stays `0.0.0`.

## Channels

| Channel | Tags | Update feed polled by the app |
| --- | --- | --- |
| stable | `vX.Y.Z` | `https://github.com/latenssi/kuulu/releases/latest/download/latest.json` |
| beta | `vX.Y.Z-beta.N` | `https://github.com/latenssi/kuulu/releases/download/beta-channel/latest.json` |

- The channel is the `beta_updates` setting in `settings.json` in the app's
  config dir. The first run sets it from the installed version: a beta
  (`-beta.N`) installer means beta, anything else stable. It persists, so a
  beta user stays on beta after updating to a stable release. Until the
  settings view exists (M5), switch channel by editing that file.
- Stable feed: GitHub's "latest release" is the newest published release that
  is not a prerelease, so beta releases (published as prereleases) never reach
  stable users.
- Beta feed: `beta-channel` is a rolling prerelease that holds a single
  `latest.json`. Publishing a release drafted by the workflow, beta or stable,
  copies its `latest.json` there if its version is newer than the one already
  there, so beta users also get stable releases and an older stable hotfix
  never replaces a newer beta. Do not delete the `beta-channel` release or
  tag; the workflow recreates it if missing.
- The app only installs versions newer than its own, and only artifacts whose
  signature names the version announced in `latest.json`
  (`requireSignedVersion`), so a tampered feed cannot downgrade users.
- The app checks on startup and every 6 hours, downloads in the background and
  installs and restarts when the user is not in a call
  (`apps/desktop/src-tauri/src/updater.rs`). Debug builds never update.

## Cutting a release

1. Make sure `main` is green in CI.
2. Tag and push from `main`:

   ```sh
   git tag v0.2.0-beta.1    # beta
   git tag v0.2.0           # stable
   git push origin <tag>
   ```

3. The workflow builds Windows (NSIS installer), macOS (universal `.dmg` and
   `.app.tar.gz`) and Linux (AppImage) without the updater key. A separate
   `sign` job in the `release` environment then signs the updater artifacts
   with the Tauri CLI from `apps/desktop/package-lock.json`
   (`tauri signer sign --app-version`), writes `latest.json` and uploads
   everything to a **draft** release. Beta tags create a prerelease.
   In parallel, the `server` job builds the server image for linux/amd64 and
   linux/arm64 (`deploy/Dockerfile`), pushes it as
   `ghcr.io/latenssi/kuulu-server:X.Y.Z[-beta.N]` and adds Linux server
   tarballs to the draft. It has `packages: write` and no secrets. The first
   push creates the package as private: make it public once in its settings.
4. Check the draft: all three platforms present in `latest.json`, release
   notes edited.
5. Publish the draft. Users only see a release once it is published. This
   also updates the beta feed and points the server image's `latest`
   (stable) or `beta` tag at the release.

A beta stays in beta for at least three days before the same code is tagged
stable.

To test the build without releasing, run the workflow manually
(`gh workflow run release.yml --ref <branch>`). It builds unsigned bundles as
workflow artifacts; the key is not used and nothing is released.

## Fix forward, never roll back

A bad release is fixed by publishing a newer version, never by deleting or
unpublishing the bad one or re-pointing the feed to an older version. The
updater only installs newer versions, so users already on the bad release
can only leave it through a newer one, and deleting a release breaks the feed
for everyone who has not updated yet.

## Updater signing key

Tauri verifies every update against the public key in
`apps/desktop/src-tauri/tauri.conf.json` before installing it.

- The private key and its password live in `pass` at
  `env/kuulu/TAURI_SIGNING_PRIVATE_KEY` and
  `env/kuulu/TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and as secrets of the same
  names in the GitHub environment `release`, which only `v*` tags can deploy
  to. Only the `sign` job uses that environment; build scripts and npm
  packages never run with the key. Never commit the key or print it in logs.
- Local `tauri build` produces no updater artifacts
  (`createUpdaterArtifacts: false`), so it needs no key.
- Offline backups in two places are the board's responsibility.
- A leaked key lets anyone push code to every installation. A lost key means
  existing installations can never be updated again. Rotating the key requires
  shipping a release signed with the old key that contains the new public key.

## OS code signing

Not set up yet; the workflow signs only when these secrets exist. They are
used during the build, so decide where they live (repository secrets or an
environment) when they are added.

- macOS: `APPLE_CERTIFICATE` (base64 `.p12`), `APPLE_CERTIFICATE_PASSWORD`,
  `APPLE_SIGNING_IDENTITY`, and for notarization `APPLE_ID`, `APPLE_PASSWORD`
  (app-specific password), `APPLE_TEAM_ID`. Without them the app is ad-hoc
  signed: users must allow the first install in System Settings > Privacy &
  Security; later updates install without prompts.
- Windows: `WINDOWS_CERTIFICATE` (base64 `.pfx`) and
  `WINDOWS_CERTIFICATE_PASSWORD`. Without them SmartScreen warns on first
  install. Certificates issued after June 2023 live on hardware or cloud HSMs
  and cannot be exported as `.pfx`; such a setup needs a `signCommand`
  instead (see the Tauri Windows signing guide).

## Known limitations

- **The repository is private.** GitHub release downloads need
  authentication, so the update feeds return 404 to installed apps and
  auto-update does not work until the repository is public. Testers must
  download releases manually until then.
- Linux updates only work for the AppImage.
