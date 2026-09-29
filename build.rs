fn main() {
    // Only embed Windows icon when targeting Windows (not cross-compiling to Android etc.)
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os == "windows" {
        #[cfg(windows)]
        {
            use winres::WindowsResource;
            let mut res = WindowsResource::new();
            res.set_icon("img/app.ico");
            // 补齐 exe 文件属性（否则“详细信息”里显示的是 Cargo 包名）。
            res.set("ProductName", "Unified Novel Downloader");
            res.set("FileDescription", "Unified Novel Downloader");
            res.set("OriginalFilename", "unified-novel-downloader.exe");
            res.set("LegalCopyright", "MIT License");
            res.compile().expect("failed to embed Windows resources");
        }
    }
}
