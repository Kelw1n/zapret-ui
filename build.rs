use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=app.manifest");
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=icon.rc");
    println!("cargo:rerun-if-changed=assets/ZapretUI.ico");
    println!("cargo:rerun-if-env-changed=WINDRES");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        let manifest =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
        embed_resource::compile("icon.rc", embed_resource::NONE)
            .manifest_optional()
            .expect("could not compile application icon");
        println!("cargo:rustc-link-arg=/MANIFEST:EMBED");
        println!("cargo:rustc-link-arg=/MANIFESTUAC:NO");
        println!("cargo:rustc-link-arg=/MANIFESTINPUT:{}", manifest.display());
    } else {
        let tool = env::var_os("WINDRES").unwrap_or_else(|| "x86_64-w64-mingw32-windres".into());
        let resource = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("app.o");
        let status = Command::new(tool)
            .args(["-i", "app.rc", "-O", "coff", "-o"])
            .arg(&resource)
            .status()
            .expect("could not run windres");
        assert!(status.success(), "windres could not compile app.rc");
        println!("cargo:rustc-link-arg={}", resource.display());
    }
}
