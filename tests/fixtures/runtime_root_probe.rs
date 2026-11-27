#[path = "../../src/config/runtime_root_guard.rs"]
mod runtime_root_guard;

fn home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

fn main() {
    let before = std::env::var_os("AGENTDESK_ROOT_DIR");
    let root = runtime_root_guard::resolve(home);
    assert_eq!(root, runtime_root_guard::resolve(home));
    for thread in (0..4).map(|_| std::thread::spawn(|| runtime_root_guard::resolve(home))) {
        assert_eq!(root, thread.join().unwrap());
    }
    assert_eq!(before, std::env::var_os("AGENTDESK_ROOT_DIR"));
    println!("{}", root.unwrap().display());
}
