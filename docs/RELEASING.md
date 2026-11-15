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

## Icons

`crates/pcvc-app/assets/app.svg` is the source. `packaging/linux/` holds a
copy for packages and `crates/pcvc-app/assets/app.ico` (Windows executable
and installer) is rendered from it; tests fail when either drifts. Regenerate
the `.ico` with:

```sh
cargo test -p pcvolumecontrol write_windows_icon -- --ignored
```
