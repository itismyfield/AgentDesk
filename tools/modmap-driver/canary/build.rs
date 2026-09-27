fn main() {
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
