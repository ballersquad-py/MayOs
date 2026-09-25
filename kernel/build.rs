//! Compile the vendored QuickJS JavaScript engine (C) for the kernel:
//! freestanding, no red zone, kernel code model, soft float (the same
//! calling convention as Rust's x86_64-unknown-none target).
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-arg-bins=-T{manifest}/linker.ld");
    println!("cargo:rerun-if-changed=linker.ld");

    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dir = PathBuf::from("vendor/quickjs");
    let files = ["cutils", "dtoa", "libunicode", "libregexp", "quickjs", "mayos_libc", "mayos_js"];
    let mut objs = Vec::new();
    for f in files {
        let src = dir.join(format!("{}.c", f));
        println!("cargo:rerun-if-changed={}", src.display());
        let obj = out.join(format!("{}.o", f));
        let ok = Command::new("clang")
            .args([
                "--target=x86_64-unknown-none-elf", "-ffreestanding", "-fno-stack-protector", "-mno-red-zone",
                "-mcmodel=kernel", "-fno-pic", "-mno-sse", "-mno-sse2", "-mno-mmx",
                "-Xclang", "-target-feature", "-Xclang", "+soft-float",
                "-O2", "-g", "-DMAYOS", "-D_GNU_SOURCE", "-DCONFIG_VERSION=\"2026-06-04\"",
                "-fno-strict-aliasing", "-w",
            ])
            .arg("-I").arg(dir.join("include"))
            .arg("-c").arg(&src).arg("-o").arg(&obj)
            .status()
            .expect("clang is needed to build the JavaScript engine");
        assert!(ok.success(), "compiling {} failed", src.display());
        objs.push(obj);
    }
    let lib = out.join("libquickjs.a");
    let _ = std::fs::remove_file(&lib);
    let ok = Command::new("ar").arg("crs").arg(&lib).args(&objs).status().expect("ar");
    assert!(ok.success());
    println!("cargo:rerun-if-changed=vendor/quickjs/include");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=quickjs");
}
