fn main() {
    println!("cargo:rustc-check-cfg=cfg(h2_items_bs_clippy)");
    if cfg!(clippy) {
        println!("cargo:rustc-cfg=h2_items_bs_clippy");
    }
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-cfg=h2_probe_pair");
    for (name, value) in [
        ("h2_probe_pair", ""),
        ("h2_probe_multi", "first"),
        ("h2_probe_multi", "second"),
        ("h2_probe_escape", "quote=\" slash=\\ newline=\n한글"),
    ] {
        println!("cargo:rustc-cfg={name}={value:?}");
    }
}
