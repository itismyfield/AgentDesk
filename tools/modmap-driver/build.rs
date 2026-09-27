// librustc_driver lives in the toolchain sysroot, off the loader path; bake that directory into the rpath.
fn main() {
    let rustc = std::env::var("RUSTC").unwrap_or_else(|_| "rustc".into());
    let out = std::process::Command::new(rustc)
        .args(["--print", "sysroot"])
        .output()
        .expect("rustc --print sysroot");
    let sysroot = String::from_utf8(out.stdout).expect("utf-8 sysroot");
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}/lib", sysroot.trim());
}
