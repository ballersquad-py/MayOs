//! Compile the SIMD-friendly C inner loops (csrc/). For the MayOS kernel
//! target they use the kernel's code model but, unlike the rest of the
//! kernel, may use SSE2.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let target = std::env::var("TARGET").unwrap();
    let src = "csrc/h264dsp.c";
    println!("cargo:rerun-if-changed={src}");
    println!("cargo:rerun-if-changed=build.rs");
    let obj = out.join("h264dsp.o");
    let mut cmd = Command::new("clang");
    if target == "x86_64-unknown-none" {
        cmd.args(["--target=x86_64-unknown-none-elf", "-ffreestanding", "-fno-stack-protector", "-mno-red-zone", "-mcmodel=kernel", "-fno-pic"]);
    } else {
        cmd.args(["-fPIC"]);
    }
    let ok = cmd
        .args(["-O3", "-fno-strict-aliasing", "-Wall", "-c", src, "-o"])
        .arg(&obj)
        .status()
        .expect("clang is needed to build the media library");
    assert!(ok.success(), "compiling {src} failed");
    let lib = out.join("libh264dsp.a");
    let _ = std::fs::remove_file(&lib);
    assert!(Command::new("ar").arg("crs").arg(&lib).arg(&obj).status().expect("ar").success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=h264dsp");
}
