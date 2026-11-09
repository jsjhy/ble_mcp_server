use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=app.ico");
    println!("cargo:rerun-if-changed=app.rc");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("gnu") {
        let object = out.join("app-icon.o");
        let status = Command::new("windres")
            .args(["--input", "app.rc", "--output-format=coff", "--output"])
            .arg(&object)
            .status()
            .expect("windres is required to embed the application icon");
        assert!(status.success(), "icon resource compilation failed");
        println!("cargo:rustc-link-arg={}", object.display());
    } else {
        let resource = out.join("app-icon.res");
        let status = Command::new("rc")
            .arg("/nologo")
            .arg("/fo")
            .arg(&resource)
            .arg("app.rc")
            .status()
            .expect("Windows SDK rc.exe is required to embed the application icon");
        assert!(status.success(), "icon resource compilation failed");
        println!("cargo:rustc-link-arg={}", resource.display());
    }
}
