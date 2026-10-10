use std::{env, path::PathBuf};

fn main() {
    println!("cargo:rerun-if-changed=assets/pixel-art-logo.ico");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let icon = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("Cargo manifest directory"))
        .join("assets/pixel-art-logo.ico");
    let version = env::var("CARGO_PKG_VERSION").expect("Cargo package version");
    winres::WindowsResource::new()
        .set_icon(icon.to_str().expect("icon path must be UTF-8"))
        .set("ProductName", "Kagantic Voice Recorder")
        .set("FileDescription", "Kagantic Voice Recorder")
        .set("FileVersion", &version)
        .set("ProductVersion", &version)
        .compile()
        .expect("Windows icon/version resources must compile successfully");
}
