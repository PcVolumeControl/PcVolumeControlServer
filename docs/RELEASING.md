# Releasing

## Versions

A release takes its version from its tag: `v1.2.3` builds version `1.2.3`.
The release workflow writes that version into `[workspace.package]` in the
root `Cargo.toml` before it builds, so it reaches `pcvolumecontrol
--version`, the Status page, the Windows executable's version resource, the
`.deb` version, the installer version and the artifact names. The version
committed in `Cargo.toml` is what builds from source report; it does not have
to match the tag.

`APPLICATION_VERSION` in `crates/pcvc-protocol/src/lib.rs` is a separate
string sent to phones in every full state (`applicationVersion`). It continues
the C# server's numbering (`v10`), the apps treat it as informational, and the
Status page shows it next to the package version. It does not need to change
for a release.

## Steps

1. Make sure CI passed on the commit to release.
2. Tag it and push the tag:

   ```sh
   git tag v1.2.3
   git push origin v1.2.3
   ```

   The tag must look like `v1.2.3` or, for a pre-release, `v1.2.3-rc.1`.
3. The `Release` workflow runs the CI checks again, builds the artifacts and
   publishes a GitHub release for the tag with generated notes and the
   artifacts attached. A tag with a pre-release suffix is published as a
   pre-release. Nothing is published if a check or a build fails.

To see what a release would contain without publishing one, run the workflow
by hand with a version: the artifacts are attached to the workflow run.

```sh
gh workflow run release.yml -f version=1.2.3-test
```

## Artifacts

| File | Built on | Contents |
| --- | --- | --- |
| `pcvolumecontrol_<version>-1_amd64.deb` | ubuntu-22.04 | `cargo deb`; binary, desktop entry, icon, README, copyright |
| `pcvolumecontrol-<version>-linux-x86_64.tar.gz` | ubuntu-22.04 | binary, desktop entry, SVG icon, LICENSE, README |
| `PcVolumeControl-<version>-setup.exe` | windows-latest | Inno Setup installer (`packaging/windows/pcvolumecontrol.iss`) |
| `pcvolumecontrol-<version>-windows-x86_64.zip` | windows-latest | portable exe, LICENSE.txt, README |
| `SHA256SUMS` | | checksums of the above |

The Linux binary needs glibc 2.35 (Ubuntu 22.04, Debian 12) or newer, set by
the build runner. Windows builds are not code signed.

No AppImage is built: the app loads Wayland, X11, EGL and Vulkan libraries at
run time, and bundling them reliably needs more work and testing on several
distributions than a release recipe can promise. Flatpak is the likelier next
format.

To build the packages locally:

```sh
cargo build --release -p pcvolumecontrol
cargo install cargo-deb --version 3.8.0 --locked   # as in release.yml
cargo deb -p pcvolumecontrol --no-build     # target/debian/
iscc /DAppVersion=1.2.3 packaging\windows\pcvolumecontrol.iss   # Windows, target\installer\
```

## Smoke test

Linux (a fresh Ubuntu 22.04 or Debian 12 VM is best):

- `sudo apt install ./pcvolumecontrol_*.deb` pulls in its dependencies.
- PcVolumeControl appears in the application menu with its icon; launching it
  shows the tray icon and the window.
- A phone on the same network discovers the server and changes the master
  and an application's volume; the Mixer agrees.
- Start on login, then log out and in: the app starts hidden in the tray.
- Quit from the tray menu; `sudo apt remove pcvolumecontrol` leaves no files
  under `/usr`.

Windows (10 or 11):

- The installer runs without an administrator prompt and installs to
  `%LOCALAPPDATA%\Programs\PcVolumeControl`; the Start menu entry opens the
  window.
- The exe's Properties > Details show the version and the icon; Task Manager
  shows "PcVolumeControl".
- First run triggers the Windows Firewall prompt; after allowing private
  networks a phone discovers and drives the server.
- Enable start on login, sign out and in: the app starts in the tray.
- Run the next version's installer while the app runs: the upgrade replaces
  it without a "file in use" error. With `/VERYSILENT` the app restarts
  afterwards.
- Uninstall from Settings > Apps: the app stops, its folder and the
  `PcVolumeControl` value under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` are gone.
- The portable zip runs from an arbitrary folder.

## Signing updates (optional)

The app verifies an installer it downloads against `SHA256SUMS` from the
same release. That proves the download is intact, not who published it: an
attacker able to upload release assets could replace both. A minisign
signature over `SHA256SUMS` closes that gap. It is off until you create a
key; once `crates/pcvc-app/assets/update-key.pub` holds a public key, builds
refuse to install an update without a valid `SHA256SUMS.minisig`.

1. Install minisign (`apt install minisign`, `brew install minisign` or
   `scoop install minisign`) and create a key pair without a password (the
   GitHub secret store protects it), somewhere outside the repository:

   ```sh
   minisign -G -W -p minisign.pub -s minisign.key
   ```

   Keep `minisign.key` offline as well, for example in a password manager.
2. Add the secret key to the repository as an Actions secret named
   `MINISIGN_SECRET_KEY` (Settings > Secrets and variables > Actions), with
   the whole content of `minisign.key` as its value:

   ```sh
   gh secret set MINISIGN_SECRET_KEY < minisign.key
   ```

3. Copy `minisign.pub` over `crates/pcvc-app/assets/update-key.pub` and
   commit it. Builds from that commit on require signed releases.

The `publish` job signs `SHA256SUMS` when the secret is set, checks the
signature against `update-key.pub` and attaches `SHA256SUMS.minisig` to the
release. It fails when `update-key.pub` has a key but the secret is missing,
since those builds could not install the release.

Versions released before the key was added keep installing on checksums
alone; the first signed release reaches them that way. Replacing the key is
a manual job: the release that carries a new public key must still be signed
with the old secret, which the workflow's check against `update-key.pub`
does not allow, so that one release needs a one-off change to the workflow.
If the secret key is lost, everyone on a version with its public key has to
install the next release by hand.

## Icons

`crates/pcvc-app/assets/app.svg` is the source. `packaging/linux/` holds a
copy for packages and `crates/pcvc-app/assets/app.ico` (Windows executable
and installer) is rendered from it; tests fail when either drifts. Regenerate
the `.ico` with:

```sh
cargo test -p pcvolumecontrol write_windows_icon -- --ignored
```
