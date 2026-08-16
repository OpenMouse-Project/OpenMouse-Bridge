fn main() {
    println!("cargo:rerun-if-changed=assets/openmouse-windows.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let result = winresource::WindowsResource::new()
            .set_icon("assets/openmouse-windows.ico")
            .compile();
        if let Err(error) = result {
            if std::env::var("HOST").is_ok_and(|host| host.contains("windows")) {
                panic!("could not embed the OpenMouse icon in the Windows executable: {error}");
            }
            println!(
                "cargo:warning=skipping Windows icon embedding while cross-checking without a resource compiler: {error}"
            );
        }
    }
}
