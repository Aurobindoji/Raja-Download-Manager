use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=installer/raja.ico");
    println!("cargo:rerun-if-changed=installer/raja.rc");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let rc_file = manifest_dir.join("installer").join("raja.rc");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let res_file = out_dir.join("raja.res");

    let rc_candidates = [
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\rc.exe",
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.22621.0\x64\rc.exe",
        r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.19041.0\x64\rc.exe",
        "rc.exe",
    ];

    for rc_bin in rc_candidates {
        let status = Command::new(rc_bin)
            .args([
                "/nologo",
                "/fo",
                res_file.to_str().unwrap(),
                rc_file.to_str().unwrap(),
            ])
            .status();
        if let Ok(s) = status
            && s.success()
            && res_file.exists()
        {
            println!("cargo:rustc-link-arg-bins={}", res_file.display());
            break;
        }
    }
}
