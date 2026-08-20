fn main() {
    let object = std::env::var("HONK_EBPF_OBJECT")
        .expect("HONK_EBPF_OBJECT must point to the built eBPF object");
    println!("cargo:rerun-if-env-changed=HONK_EBPF_OBJECT");
    println!("cargo:rustc-env=HONK_EBPF_OBJECT={object}");
}
