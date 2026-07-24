fn main() {
    #[cfg(windows)]
    {
        // Embed the app icon + version resource into the Windows exe.
        // Uses windres (works with both MSVC and mingw cross builds).
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/singularity.ico");
        res.set("ProductName", "Singularity");
        res.set("FileDescription", "A drifting black hole over your desktop");
        res.set("LegalCopyright", "MIT License");
        if let Err(e) = res.compile() {
            // Don't fail the build over a missing windres - just skip the icon.
            println!("cargo:warning=icon resource skipped: {e}");
        }
    }
}
