fn main() {
    // Иконка нужна только в Windows-ресурсах exe;
    // на macOS иконку несёт .app-бандл (см. make_app.sh).
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico");
        res.set("ProductName", "SoundTransfer");
        res.set("FileDescription", "SoundTransfer — передача звука между устройствами");
        if let Err(e) = res.compile() {
            println!("cargo:warning=не удалось вшить иконку: {e}");
        }
    }
}
