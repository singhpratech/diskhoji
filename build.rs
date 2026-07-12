// Build script: embed the app icon and version metadata into diskhoji.exe on
// Windows. Without this the PE file shows the generic executable icon in
// Explorer / taskbar-before-launch and an empty Properties -> Details tab.
//
// The `#[cfg(windows)]` gate matches the `[target.'cfg(windows)'.build-dependencies]`
// gate in Cargo.toml: CI builds every target natively (host == target), so on the
// Windows runner this compiles and embeds the resource via the preinstalled SDK
// rc.exe, while the Linux/macOS builds compile it out and never pull winresource.
fn main() {
    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "Diskhoji");
        res.set("FileDescription", "Diskhoji — fast disk space analyzer");
        res.set("CompanyName", "Prateek Singh");
        res.set("LegalCopyright", "Diskhoji is MIT licensed");
        // FileVersion / ProductVersion are filled from CARGO_PKG_VERSION automatically.
        // Non-fatal: a resource-toolchain hiccup must not fail the whole Windows
        // build (the zip and MSI both depend on it) — worst case, no embedded icon.
        if let Err(e) = res.compile() {
            println!("cargo:warning=diskhoji: could not embed Windows resources: {e}");
        }
    }
}
