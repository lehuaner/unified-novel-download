fn main() {
    // Only embed Windows icon when targeting Windows (not cross-compiling to Android etc.)
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        #[cfg(windows)]
        {
            use winres::WindowsResource;
            WindowsResource::new()
                .set_icon("img/Tomato-downloader-ico.ico")
                .compile()
                .expect("failed to embed Windows icon");
        }
    }
}
