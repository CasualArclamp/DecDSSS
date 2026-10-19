// made by CasualArclamp
//! Puts the DecDSSS icon (assets/decdsss.ico, drawn by assets/make_icon.py) into
//! the Windows executable, so Explorer, the taskbar and Alt-Tab show it. It is
//! compiled with the Windows SDK's resource compiler (rc.exe), which comes with
//! the MSVC build tools.
fn main() {
    const ICON: &str = "../../assets/decdsss.ico";
    println!("cargo:rerun-if-changed={ICON}");
    // A build script runs on the build machine, so `cfg!(windows)` would describe
    // that machine; CARGO_CFG_TARGET_OS is the platform being built for.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon(ICON);
        if let Err(e) = res.compile() {
            // An exe without an icon still works: warn rather than fail the build.
            println!("cargo:warning=no icon in the executable: {e}");
        }
    }
}
