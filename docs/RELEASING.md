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

- A build picks its channel from its own version: a prerelease version
  (`-beta.N`) polls the beta feed, anything else the stable feed.
- Stable feed: GitHub's "latest release" is the newest published release that
  is not a prerelease, so beta releases (published as prereleases) never reach
  stable users.
- Beta feed: `beta-channel` is a rolling prerelease that holds a single
  `latest.json`. Publishing any release, beta or stable, copies that release's
  `latest.json` there, so beta users also get stable releases. Do not delete
  the `beta-channel` release or tag; the workflow recreates it if missing.
- The app only installs versions newer than its own. Publishing a stable
  hotfix older than the current beta points the beta feed at the hotfix; beta
  users stay on their newer beta until the next beta is published.
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

3. The workflow builds Windows (NSIS installer), macOS (universal `.app` /
   `.dmg`) and Linux (AppImage), signs the updater artifacts and uploads them
   with `latest.json` to a **draft** release. Beta tags create a prerelease.
4. Check the draft: all three platforms present in `latest.json`, release
   notes edited.
5. Publish the draft. Users only see a release once it is published. This
   also updates the beta feed.

A beta stays in beta for at least three days before the same code is tagged
stable.

To test the build without releasing, run the workflow manually
(`gh workflow run release.yml --ref <branch>`); nothing is uploaded.

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
  `env/kuulu/TAURI_SIGNING_PRIVATE_KEY_PASSWORD`, and as GitHub Actions
  secrets of the same names. Never commit them or print them in logs.
- Offline backups in two places are the board's responsibility.
- A leaked key lets anyone push code to every installation. A lost key means
  existing installations can never be updated again. Rotating the key requires
  shipping a release signed with the old key that contains the new public key.

## OS code signing

Not set up yet; the workflow signs only when these secrets exist.

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
