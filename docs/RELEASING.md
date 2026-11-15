# Releasing

## Versions

The release version is `version` under `[workspace.package]` in the root
`Cargo.toml`; every crate inherits it. From it come `pcvolumecontrol
--version`, the Status page, the Windows executable's version resource, the
`.deb` version, the installer version and the artifact names.

`APPLICATION_VERSION` in `crates/pcvc-protocol/src/lib.rs` is a separate
string sent to phones in every full state (`applicationVersion`). It continues
the C# server's numbering (`v10`), the apps treat it as informational, and the
Status page shows it next to the package version. It does not need to change
for a release.

## Steps

1. Bump `version` in the root `Cargo.toml` and run `cargo check` so
   `Cargo.lock` picks up the new workspace versions.
2. Commit, push to `main` and wait for CI to pass. The release workflow runs
   the same checks again and builds nothing if they fail.
3. Tag that commit and push the tag:

   ```sh
   git tag v1.2.3
   git push origin v1.2.3
   ```

   The tag must be `v` plus the package version; the release workflow fails
   otherwise. A tag with a pre-release suffix (`v1.2.3-rc.1`) makes a
   pre-release.
4. The `Release` workflow builds the artifacts and attaches them to a draft
   GitHub release with generated notes. Smoke test them (below), edit the
   notes, then publish the draft.

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
