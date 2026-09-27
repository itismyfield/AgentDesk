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
        publish(&self.out, &serde_json::json!(atoms))?;
        let mut invocation = self.out.as_os_str().to_owned();
        invocation.push(".invocation.json");
        publish(
            Path::new(&invocation),
            &serde_json::json!({"nonce": self.nonce, "argv": self.argv, "root": root}),
        )
    }
}
