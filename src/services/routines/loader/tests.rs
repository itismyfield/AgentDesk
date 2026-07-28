use super::*;

fn fixture_routines_root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("routines")
}

mod automation_recommender;
mod cache;
mod loading;
mod tick;
