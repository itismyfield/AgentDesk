//! RUSTC_WORKSPACE_WRAPPER for the H2 R-O module map: `modmap-driver <rustc> <args...>`.
//! The package's own `src/lib.rs` compile runs in-process and stops after expansion; anything else runs the real rustc.
#![feature(rustc_private)]
extern crate rustc_ast;
extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

mod modmap;

use rustc_span::ExpnId;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;

struct MapCallbacks {
    root: PathBuf,
    out: PathBuf,
}

impl rustc_driver::Callbacks for MapCallbacks {
    fn after_expansion<'tcx>(
        &mut self,
        _compiler: &rustc_interface::interface::Compiler,
        tcx: rustc_middle::ty::TyCtxt<'tcx>,
    ) -> rustc_driver::Compilation {
        let walked = {
            let resolver = tcx.resolver_for_lowering().borrow();
            let defs = &resolver.0.node_id_to_def_id;
            let expn = |n| defs.get(&n).map(|&d| tcx.expn_that_defined(d)) != Some(ExpnId::root());
            modmap::walk(tcx.sess, &resolver.1, &self.root, &expn)
        };
        match walked {
            Ok(rows) => {
                if let Err(err) = modmap::write(&self.out, &rows) {
                    tcx.dcx().err(format!(
                        "modmap: cannot write {}: {err}",
                        self.out.display()
                    ));
                }
            }
            Err(shape) => {
                for (span, message) in shape {
                    tcx.dcx().span_err(span, format!("modmap: {message}"));
                }
            }
        }
        // No analysis means no rmeta, so cargo never replays this unit and every run rewrites the map.
        rustc_driver::Compilation::Stop
    }
}

/// The package root when `args` compile its own `src/lib.rs` outside `--test`, whatever the crate's name or type.
fn root_lib(args: &[String]) -> Option<PathBuf> {
    let root = std::fs::canonicalize(std::env::var_os("CARGO_MANIFEST_DIR")?).ok()?;
    let lib = std::fs::canonicalize(root.join("src/lib.rs")).ok()?;
    let compiles_lib = args
        .iter()
        .any(|arg| std::fs::canonicalize(arg).is_ok_and(|path| path == lib));
    (compiles_lib && !args.iter().any(|arg| arg == "--test")).then_some(root)
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 2 {
        eprintln!("usage: modmap-driver <rustc> <args...> (as RUSTC_WORKSPACE_WRAPPER)");
        std::process::exit(2);
    }
    let rest = &argv[2..];
    let Some(root) = root_lib(rest) else {
        let err = std::process::Command::new(&argv[1]).args(rest).exec();
        eprintln!("modmap-driver: exec {}: {err}", argv[1]);
        std::process::exit(2);
    };
    let Some(out) = std::env::var_os("MODMAP_OUT") else {
        eprintln!(
            "modmap-driver: MODMAP_OUT is not set; refusing to compile {} without a map",
            root.display()
        );
        std::process::exit(2);
    };
    let args: Vec<String> = std::iter::once(argv[0].clone())
        .chain(rest.iter().cloned())
        .collect();
    let mut callbacks = MapCallbacks {
        root,
        out: PathBuf::from(out),
    };
    rustc_driver::install_ice_hook("modmap-driver", |_| ());
    std::process::exit(rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&args, &mut callbacks)
    }));
}
