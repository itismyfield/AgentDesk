use std::path::{Path, PathBuf};

pub struct Snapshot {
    pub out: PathBuf,
    pub nonce: String,
    pub argv: Vec<String>,
}

fn publish(out: &Path, value: &serde_json::Value) -> std::io::Result<()> {
    let mut partial = out.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    std::fs::write(&partial, serde_json::to_vec(value)?)?;
    std::fs::rename(partial, out)
}

impl Snapshot {
    pub fn write(
        &self,
        root: &Path,
        config: impl Iterator<Item = (String, Option<String>)>,
    ) -> std::io::Result<()> {
        let mut atoms: Vec<Vec<String>> = config
            .map(|(name, value)| std::iter::once(name).chain(value).collect())
            .collect();
        atoms.sort();
        atoms.dedup();
        let run_id = std::env::var("MODMAP_RUN_ID").unwrap_or_default();
        let bound = !run_id.is_empty();
        let cfg = if bound {
            serde_json::json!({"schema": 1, "run_id": run_id, "nonce": self.nonce, "atoms": atoms})
        } else {
            serde_json::json!(atoms)
        };
        publish(&self.out, &cfg)?;
        let mut invocation = self.out.as_os_str().to_owned();
        invocation.push(".invocation.json");
        let mut proof = serde_json::json!({"nonce": self.nonce, "argv": self.argv, "root": root});
        if bound {
            let out = std::env::var("MODMAP_OUT").unwrap_or_default();
            proof["schema"] = serde_json::json!(1);
            proof["run_id"] = serde_json::json!(run_id);
            proof["kind"] = serde_json::json!(std::env::var("MODMAP_KIND").unwrap_or_default());
            proof["tsv"] = serde_json::json!(std::fs::read_to_string(&out)?);
            proof["out"] = serde_json::json!(out);
            proof["env"] = serde_json::json!(
                std::env::vars()
                    .filter(|(key, _)| {
                        key.starts_with("CARGO_FEATURE_")
                            || matches!(
                                key.as_str(),
                                "MODMAP_RUN_ID"
                                    | "RUSTFLAGS"
                                    | "CARGO_ENCODED_RUSTFLAGS"
                                    | "CARGO_BUILD_TARGET"
                                    | "RUSTC_BOOTSTRAP"
                                    | "OUT_DIR"
                            )
                    })
                    .collect::<std::collections::BTreeMap<_, _>>()
            );
        }
        publish(Path::new(&invocation), &proof)
    }
}
