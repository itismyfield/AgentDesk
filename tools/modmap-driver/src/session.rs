use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::{ffi::OsStrExt, process::CommandExt};
use std::path::{Path, PathBuf};
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
    if args.iter().any(|arg| arg.starts_with('@')) {
        return Err("response-file compiler arguments are unsupported for a producer".into());
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
    root: PathBuf,
    nonce: String,
    run_id: String,
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
        let written = crate::items::write(
            tcx,
            &self.root,
            self.cfg.parent().unwrap(),
            &self.nonce,
            &self.run_id,
        )
        .and_then(|()| fs::write(&self.cfg, cfg.join("\n") + "\n"));
        if let Err(err) = written {
            tcx.dcx()
                .err(format!("modmap: cannot write session items/cfg: {err}"));
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
            .chain(["--cap-lints".into(), "warn".into()])
            .collect();
        Ok((
            args,
            ItemsCallbacks {
                cfg: with_suffix(".items-cfg.txt")?,
                root: fs::canonicalize(required("CARGO_MANIFEST_DIR")?)?,
                nonce: required("MODMAP_CFG_NONCE")?,
                run_id: required("MODMAP_RUN_ID")?,
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

fn clippy_version(clippy: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new(clippy).args(args).output()?;
    if !output.status.success() {
        return Err("clippy identity query failed".into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn protected_env(request: &Value) -> Result<BTreeMap<String, String>> {
    let mut actual = BTreeMap::new();
    for key in [
        "CLIPPY_ARGS",
        "CLIPPY_CONF_DIR",
        "CLIPPY_TERMINAL_WIDTH",
        "MODMAP_SESSION_OUT",
        "MODMAP_CFG_NONCE",
        "MODMAP_RUN_ID",
        "MODMAP_EXPECT_MANIFEST",
        "MODMAP_EXPECT_PACKAGE",
        "MODMAP_EXPECT_LIB",
    ] {
        let expected = request["protected_env"][key]
            .as_str()
            .ok_or_else(|| format!("request has no protected env {key}"))?;
        let value =
            std::env::var(key).map_err(|_| format!("protected env {key} is missing or invalid"))?;
        if value.as_bytes() != expected.as_bytes() {
            return Err(format!("protected env {key} differs from request").into());
        }
        actual.insert(key.to_owned(), value);
    }
    Ok(actual)
}

fn prepare(argv: &[String], clippy: &OsStr) -> Result<PathBuf> {
    let Some(unit) = requested_unit(&argv[2..])? else {
        return Ok(PathBuf::from(clippy));
    };
    let proof = with_suffix("")?;
    let nonce = required("MODMAP_CFG_NONCE")?;
    let run_id = required("MODMAP_RUN_ID")?;
    if proof.exists() {
        return Err("proof already exists: second producer in this session".into());
    }
    let request: Value = serde_json::from_slice(&fs::read(
        proof
            .parent()
            .ok_or("session output has no parent")?
            .join("request.json"),
    )?)?;
    let clippy = fs::canonicalize(clippy)?;
    let expected = request["toolchain"]["clippy_driver"]
        .as_str()
        .ok_or("request has no approved clippy path")?;
    if clippy != Path::new(expected) {
        return Err("clippy path differs from request".into());
    }
    let protected_env = protected_env(&request)?;
    // A claim is permanent for this run, including failed or interrupted producers.
    let mut claim = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(with_suffix(".claim")?)?;
    claim.write_all(&serde_json::to_vec(
        &json!({"pid": std::process::id(), "unit": unit}),
    )?)?;
    claim.sync_all()?;
    let version = clippy_version(&clippy, &["--version"])?;
    let compiler = clippy_version(&clippy, &["--rustc", "-vV"])?;
    if Some(version.as_str()) != request["toolchain"]["clippy"].as_str()
        || Some(compiler.as_str()) != request["toolchain"]["clippy_rustc"].as_str()
    {
        return Err("clippy identity differs from request".into());
    }
    captured(
        Command::new(std::env::current_exe()?)
            .arg("__modmap_items_child")
            .args(&argv[1..]),
        "items",
    )?;
    let printed = with_suffix(".clippy-cfg.txt")?;
    let flags = &protected_env["CLIPPY_ARGS"];
    // Passing --print in argv disables Clippy; only this root probe receives the trailing argument.
    captured(
        Command::new(&clippy).args(&argv[1..]).env(
            "CLIPPY_ARGS",
            format!("{flags}--print=cfg={}__CLIPPY_HACKERY__", printed.display()),
        ),
        "probe",
    )?;
    let cfg = fs::read_to_string(with_suffix(".items-cfg.txt")?)?;
    if cfg.is_empty() || cfg != fs::read_to_string(printed)? {
        return Err("items cfg != clippy-driver cfg".into());
    }
    let env: BTreeMap<_, _> = std::env::vars_os()
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
    let run = proof.parent().ok_or("session output has no parent")?;
    let items = fs::read(run.join("items.jsonl"))?;
    let items_sha256 = crate::items::digest(&items);
    let records = items
        .split(|&b| b == b'\n')
        .filter(|line| !line.is_empty())
        .count()
        .saturating_sub(1);
    let receipt: Value = serde_json::from_slice(&fs::read(run.join("items.jsonl.sha256"))?)?;
    if records == 0 || receipt != json!({"sha256": items_sha256, "records": records}) {
        return Err("items changed since callback".into());
    }
    let value = json!({"schema": "h2-session/2", "unit": unit, "pid": std::process::id(),
        "items_sha256": items_sha256, "items_records": records,
        "nonce": nonce, "run_id": run_id, "argv": &argv[1..], "env_sha256": digest, "protected_env": protected_env,
        "clippy_driver": clippy, "clippy": version, "clippy_rustc": compiler,
        "cfg": cfg.split_terminator('\n').collect::<Vec<_>>(), "driver_rustc": rustc_interface::util::rustc_version_str()});
    let partial = with_suffix(".partial")?;
    fs::write(&partial, serde_json::to_vec(&value)?)?;
    fs::rename(partial, proof)?;
    Ok(clippy)
}

pub fn run(argv: &[String], clippy: &OsStr) -> ! {
    let clippy = prepare(argv, clippy).unwrap_or_else(|e| fail(e));
    fail(Command::new(clippy).args(&argv[1..]).exec())
}
