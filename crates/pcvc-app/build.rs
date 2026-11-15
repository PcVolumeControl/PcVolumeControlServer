//! Embeds the application icon and version information in the Windows
//! executable.

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    #[cfg(windows)]
    windows_resources();
}

/// Runs only on a Windows host, where the resource compiler is available.
#[cfg(windows)]
fn windows_resources() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/app.ico");
    let icon = icon.to_str().expect("icon path is UTF-8");
    println!("cargo::rerun-if-changed={icon}");

    winresource::WindowsResource::new()
        .set_icon(icon)
        // Task Manager and the firewall prompt show FileDescription as the
        // application's name.
        .set("FileDescription", "PcVolumeControl")
        .set("ProductName", "PcVolumeControl")
        .set("CompanyName", "PcVolumeControl")
        .set("LegalCopyright", "Copyright (c) 2017 Adam Brown")
        .set("OriginalFilename", "pcvolumecontrol.exe")
        .compile()
        .expect("compiling Windows resources");
}
