//! Build script for honk-core, replacing upstream's `crates/honk-core/build.rs`
//! in the Nix package.
//!
//! Upstream's build script locates — or shells out to build — the eBPF object
//! itself, which the Nix sandbox cannot do. This package builds the object in a
//! separate derivation (`honk-ebpf`) and passes its store path in
//! `HONK_EBPF_OBJECT`, so this script only forwards that path; `honk-core`
//! embeds it with `include_bytes!(env!("HONK_EBPF_OBJECT"))`.
//!
//! It must also emit `HONK_VERSION`: since 0.0.1-alpha the crate reads it with
//! `env!` in `src/lib.rs` and `src/clash_api.rs`, and upstream emits it from
//! this same build script (release tag, else git description, else the crate
//! version). The sandbox has no git checkout to describe, so the crate version
//! is the fallback; a release build can still pass `GITHUB_REF`.

fn main() {
    let object = std::env::var("HONK_EBPF_OBJECT")
        .expect("HONK_EBPF_OBJECT must point to the built eBPF object");
    println!("cargo:rerun-if-env-changed=HONK_EBPF_OBJECT");
    println!("cargo:rustc-env=HONK_EBPF_OBJECT={object}");

    println!("cargo:rerun-if-env-changed=GITHUB_REF");
    let version = std::env::var("GITHUB_REF")
        .ok()
        .and_then(|reference| reference.strip_prefix("refs/tags/").map(str::to_string))
        .filter(|tag| !tag.is_empty())
        .unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
    println!("cargo:rustc-env=HONK_VERSION={version}");
}
