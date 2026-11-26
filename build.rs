use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/Kuldetesek/s3_hip.hip");
    println!("cargo:rerun-if-env-changed=ROCM_PATH");

    if env::var_os("CARGO_FEATURE_HIP").is_none() {
        return;
    }

    let rocm_path = env::var_os("ROCM_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/rocm"));
    let hipcc = rocm_path.join("bin/hipcc");
    if !hipcc.is_file() || !rocm_path.join("include/hip/hip_runtime.h").is_file() {
        panic!(
            "The `hip` feature requires ROCm; could not find {}. Set ROCM_PATH to the ROCm installation.",
            hipcc.display()
        );
    }

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let object = out_dir.join("s3_hip.o");
    let archive = out_dir.join("libs3_hip.a");

    let status = Command::new(&hipcc)
        .args(["-O3", "-fPIC", "-c"])
        .arg("src/Kuldetesek/s3_hip.hip")
        .arg("-o")
        .arg(&object)
        .status()
        .unwrap_or_else(|error| panic!("Could not run {}: {error}", hipcc.display()));
    if !status.success() {
        panic!("hipcc failed to compile the S3 mining kernel");
    }

    let status = Command::new("ar")
        .arg("crs")
        .arg(&archive)
        .arg(&object)
        .status()
        .expect("Could not run ar to archive the S3 HIP kernel");
    if !status.success() {
        panic!("ar failed to archive the S3 HIP kernel");
    }

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=s3_hip");
    println!("cargo:rustc-link-lib=dylib=amdhip64");
    println!("cargo:rustc-link-lib=dylib=stdc++");
}
