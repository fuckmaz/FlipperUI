# Releasing FlipperUI

FlipperUI checks GitHub for the latest **published stable release** and tells
users when its semantic version is newer than the installed application. The
app does not download or install updates; its update button opens the release
page in the user's browser.

The GitHub Actions workflow only builds downloadable artifacts. It never
creates a tag, release draft, or published release.

## Prepare the version

1. Start from the exact commit that should be released and choose a semantic
   version:

   ```sh
   npm run release:prepare -- 0.4.7
   ```

   This synchronizes `package.json`, `package-lock.json`, `Cargo.toml`,
   `Cargo.lock`, and `tauri.conf.json`. It also creates
   `release-texts/v0.4.7.md` if that file does not exist.

2. Complete the generated release notes.

3. Run the local checks:

   ```sh
   npm ci
   npm run lint
   npm run typecheck
   npm run build
   cargo test --manifest-path src-tauri/Cargo.toml
   ```

4. Commit and push the application, version, and release-note changes when
   they are ready. Creating or pushing a tag does not run the bundle workflow;
   that step is intentionally manual.

## Build installers without creating a release

1. Open the repository's **Actions** page.
2. Select **Build release bundles**.
3. Choose **Run workflow** and the branch/commit to build.
4. Wait for the macOS Apple Silicon, macOS Intel, Linux, and Windows jobs.
5. Download their artifacts from the completed workflow run and smoke-test the
   packages you intend to publish.

These are ordinary GitHub Actions workflow artifacts. Running the workflow
does not create or modify anything on the repository's Releases page.

## Publish the release manually

1. Open <https://github.com/fuckmaz/FlipperUI/releases/new>.
2. Choose or create the matching tag, for example `v0.4.7`.
3. Use `FlipperUI v0.4.7` as the title and paste the completed Markdown release
   notes into the description.
4. Upload the tested installers from the workflow artifacts.
5. Leave **Set as a pre-release** off for a normal update.
6. Review everything and click **Publish release**.

Drafts and prereleases are ignored by FlipperUI. Once a stable release is
published, GitHub's `/releases/latest` endpoint exposes it and older installed
versions can notify the user.

FlipperUI v0.4.6 is the first version containing this checker, so v0.4.5
cannot announce v0.4.6. Users make that one upgrade from GitHub manually. The
notification flow can be exercised normally when v0.4.7 (or any later stable
version) is published and checked from v0.4.6.

## Test the in-app notification

1. Open an installed FlipperUI version older than the published release.
2. Choose **Help → Check for Updates…**, use the tray menu item, or go to
   **Settings → Application Updates → Check now**.
3. Confirm that the update toast shows both versions and that its orange timer
   bar counts down before the toast disappears.
4. Run the check again and click **View release**. Confirm that the default
   browser opens the correct GitHub Release page.
5. Restart the older app and confirm that the same release is not announced
   automatically a second time. Manual checks should still show it.
