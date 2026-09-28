use serde_json::{Value, json};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::path::PathBuf;
use std::process::Command;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn required(key: &str) -> Result<String> {
    Ok(std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| format!("{key} is not set"))?)
}

fn with_suffix(suffix: &str) -> Result<PathBuf> {
    Ok(PathBuf::from(format!(
        "{}{suffix}",
        required("MODMAP_SESSION_OUT")?
    )))
}

fn values(args: &[String], flag: &str) -> Vec<String> {
    args.iter()
        .enumerate()
        .filter_map(|(i, arg)| {
            if arg == flag {
                args.get(i + 1).cloned()
            } else {
                arg.strip_prefix(&format!("{flag}=")).map(str::to_owned)
            }
        })
        .collect()
}

fn requested_unit(args: &[String]) -> Result<Option<Value>> {
    let manifest = fs::canonicalize(required("MODMAP_EXPECT_MANIFEST")?)?;
    let package = required("MODMAP_EXPECT_PACKAGE")?;
    let lib = fs::canonicalize(required("MODMAP_EXPECT_LIB")?)?;
    if args.iter().any(|a| {
        matches!(a.as_str(), "-vV" | "-V" | "--version" | "--test") || a.starts_with("--print")
    }) {
        return Ok(None);
    }
    let root = std::env::var_os("CARGO_MANIFEST_DIR").and_then(|v| fs::canonicalize(v).ok());
    let Some(root) = root else { return Ok(None) };
    if fs::canonicalize(root.join("Cargo.toml")).ok().as_ref() != Some(&manifest)
        || std::env::var("CARGO_PKG_NAME").ok().as_ref() != Some(&package)
        || !args
            .iter()
            .any(|a| !a.starts_with('-') && fs::canonicalize(a).is_ok_and(|p| p == lib))
    {
        return Ok(None);
    }
    let mut types: Vec<String> = values(args, "--crate-type")
        .iter()
        .flat_map(|v| v.split(',').map(str::to_owned))
        .collect();
    if types.is_empty()
        || types
            .iter()
            .any(|v| matches!(v.as_str(), "bin" | "proc-macro"))
    {
        return Ok(None);
    }
    types.sort();
    types.dedup();
    let name = values(args, "--crate-name")
        .pop()
        .ok_or("requested unit has no crate name")?;
    let metadata = values(args, "-C")
        .into_iter()
        .chain(args.iter().filter_map(|a| {
            a.strip_prefix("-Cmetadata=")
                .map(|v| format!("metadata={v}"))
        }))
        .find_map(|v| v.strip_prefix("metadata=").map(str::to_owned))
        .unwrap_or_default();
    Ok(Some(
        json!({"manifest": manifest, "package": package, "lib": lib, "root": root,
        "crate_name": name, "crate_types": types, "metadata": metadata, "test": false}),
    ))
}

struct ItemsCallbacks {
    cfg: PathBuf,
}

impl rustc_driver::Callbacks for ItemsCallbacks {
    fn after_expansion<'tcx>(
        &mut self,
        _: &rustc_interface::interface::Compiler,
        tcx: rustc_middle::ty::TyCtxt<'tcx>,
    ) -> rustc_driver::Compilation {
        let sess = tcx.sess;
        let mut cfg: Vec<String> = sess
            .psess
            .config
            .iter()
            .filter(|&&(name, _)| {
                sess.is_nightly_build() || rustc_feature::find_gated_cfg(|s| s == name).is_none()
            })
            .map(|&(name, value)| match value {
                Some(value) => format!("{name}=\"{value}\""),
                None => name.to_string(),
            })
            .collect();
        cfg.sort();
        if let Err(err) = fs::write(&self.cfg, cfg.join("\n") + "\n") {
            tcx.dcx()
                .err(format!("modmap: cannot write session cfg: {err}"));
        }
        rustc_driver::Compilation::Stop
    }
}

fn fail(err: impl std::fmt::Display) -> ! {
    eprintln!("modmap-driver (clippy session): {err}");
    std::process::exit(101)
}

pub fn child(argv: &[String]) -> ! {
    let setup = || -> Result<_> {
        let rest = argv
            .get(3..)
            .ok_or("items child needs compiler arguments")?;
        requested_unit(rest)?.ok_or("items child outside requested unit")?;
        let args: Vec<String> = std::iter::once(argv[0].clone())
            .chain(rest.iter().cloned())
            .chain(["--cfg".into(), "clippy".into()])
            .collect();
        Ok((
            args,
            ItemsCallbacks {
                cfg: with_suffix(".items-cfg.txt")?,
            },
        ))
    };
    let (args, mut callbacks) = setup().unwrap_or_else(|e| fail(e));
    rustc_driver::install_ice_hook("modmap-driver", |_| ());
    std::process::exit(rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&args, &mut callbacks)
    }))
}

fn captured(command: &mut Command, label: &str) -> Result<()> {
    let status = command
        .stdout(File::create(with_suffix(&format!(".{label}.stdout"))?)?)
        .stderr(File::create(with_suffix(&format!(".{label}.stderr"))?)?)
        .status()?;
    if !status.success() {
        return Err(format!("{label} child failed ({status}); see session logs").into());
    }
    Ok(())
}

fn prepare(argv: &[String], clippy: &OsStr) -> Result<()> {
    let Some(unit) = requested_unit(&argv[2..])? else {
        return Ok(());
    };
    let proof = with_suffix("")?;
    let nonce = required("MODMAP_CFG_NONCE")?;
    let run_id = required("MODMAP_RUN_ID")?;
    if proof.exists() {
        return Err("proof already exists: second producer in this session".into());
    }
    // A claim is permanent for this run, including failed or interrupted producers.
    let mut claim = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(with_suffix(".claim")?)?;
    claim.write_all(&serde_json::to_vec(
        &json!({"pid": std::process::id(), "unit": unit}),
    )?)?;
    claim.sync_all()?;
    captured(
        Command::new(std::env::current_exe()?)
            .arg("__modmap_items_child")
            .args(&argv[1..]),
        "items",
    )?;
    let printed = with_suffix(".clippy-cfg.txt")?;
    let flags = std::env::var("CLIPPY_ARGS").unwrap_or_default();
    // Passing --print in argv disables Clippy; only this root probe receives the trailing argument.
    captured(
        Command::new(clippy).args(&argv[1..]).env(
            "CLIPPY_ARGS",
            format!("{flags}--print=cfg={}__CLIPPY_HACKERY__", printed.display()),
        ),
        "probe",
    )?;
    let cfg = fs::read_to_string(with_suffix(".items-cfg.txt")?)?;
    if cfg.is_empty() || cfg != fs::read_to_string(printed)? {
        return Err("items cfg != clippy-driver cfg".into());
    }
    let env: std::collections::BTreeMap<_, _> = std::env::vars_os()
        .map(|(k, v)| (k.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
    let bytes = serde_json::to_vec(&env.into_iter().collect::<Vec<_>>())?;
    let hash = rustc_span::SourceFileHash::new_in_memory(
        rustc_span::SourceFileHashAlgorithm::Sha256,
        &bytes,
    );
    let digest: String = hash
        .hash_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let value = json!({"schema": "h2-session/1-cfg", "unit": unit, "pid": std::process::id(),
        "nonce": nonce, "run_id": run_id, "argv": &argv[1..], "env_sha256": digest,
        "cfg": cfg.split_terminator('\n').collect::<Vec<_>>(), "driver_rustc": rustc_interface::util::rustc_version_str()});
    let partial = with_suffix(".partial")?;
    fs::write(&partial, serde_json::to_vec(&value)?)?;
    fs::rename(partial, proof)?;
    Ok(())
}

pub fn run(argv: &[String], clippy: &OsStr) -> ! {
    prepare(argv, clippy).unwrap_or_else(|e| fail(e));
    fail(Command::new(clippy).args(&argv[1..]).exec())
}
