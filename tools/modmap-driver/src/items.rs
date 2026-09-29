//! Compiler-owned registration paths and original-file byte spans for a Clippy session.
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{DefId, LOCAL_CRATE, LocalDefId};
use rustc_middle::ty::{self, TyCtxt};
use rustc_span::{FileName, SourceFileHash, SourceFileHashAlgorithm};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::io;
use std::path::Path;

fn module_path(tcx: TyCtxt<'_>, def: DefId) -> Result<String, String> {
    if !def.is_local() {
        return Err(format!("external:{}", tcx.crate_name(def.krate)));
    }
    let mut parts = vec![tcx.item_name(def).to_string()];
    let mut cur = tcx.parent(def);
    while cur.index != rustc_hir::def_id::CRATE_DEF_INDEX {
        if tcx.def_kind(cur) != DefKind::Mod {
            return Err("in-function".into());
        }
        parts.push(tcx.item_name(cur).to_string());
        cur = tcx.parent(cur);
    }
    parts.push(tcx.crate_name(LOCAL_CRATE).to_string());
    parts.reverse();
    Ok(parts.join("::"))
}

fn nearest_fn(tcx: TyCtxt<'_>, def: LocalDefId) -> Option<LocalDefId> {
    let mut cur = tcx.opt_local_parent(def)?;
    loop {
        match tcx.def_kind(cur) {
            DefKind::Fn | DefKind::AssocFn => return Some(cur),
            DefKind::Mod => return None,
            _ => cur = tcx.opt_local_parent(cur)?,
        }
    }
}

fn registration(
    tcx: TyCtxt<'_>,
    def: LocalDefId,
) -> io::Result<(&'static str, Result<String, String>)> {
    let id = def.to_def_id();
    Ok(match tcx.def_kind(id) {
        DefKind::Fn => ("fn", module_path(tcx, id)),
        DefKind::AssocFn => {
            let name = tcx.item_name(id);
            let parent = tcx.parent(id);
            match tcx.def_kind(parent) {
                DefKind::Trait => (
                    "trait_method",
                    module_path(tcx, parent).map(|p| format!("{p}::{name}")),
                ),
                DefKind::Impl { of_trait: true } => {
                    let trait_ref = tcx.impl_trait_ref(parent).skip_binder();
                    let item = tcx
                        .associated_item(id)
                        .trait_item_def_id()
                        .ok_or_else(|| io::Error::other("trait impl method has no trait item"))?;
                    (
                        "trait_impl_method",
                        module_path(tcx, trait_ref.def_id)
                            .map(|p| format!("{p}::{}", tcx.item_name(item))),
                    )
                }
                DefKind::Impl { of_trait: false } => {
                    let reg = match tcx.type_of(parent).instantiate_identity().kind() {
                        ty::Adt(adt, _) => {
                            module_path(tcx, adt.did()).map(|p| format!("{p}::{name}"))
                        }
                        _ => Err("self-not-adt".into()),
                    };
                    ("inherent_method", reg)
                }
                _ => return Err(io::Error::other("method has no trait/impl container")),
            }
        }
        DefKind::Const | DefKind::AssocConst | DefKind::Static { .. } => {
            ("const", Err("const-item".into()))
        }
        DefKind::AnonConst | DefKind::InlineConst => ("const", Err("anon-const".into())),
        _ => ("header", Err("module-level".into())),
    })
}

pub fn digest(bytes: &[u8]) -> String {
    SourceFileHash::new_in_memory(SourceFileHashAlgorithm::Sha256, bytes)
        .hash_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn publish(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let partial = path.with_file_name(format!(
        "{}.partial",
        path.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&partial, bytes)?;
    std::fs::rename(partial, path)
}

pub fn write(
    tcx: TyCtxt<'_>,
    root: &Path,
    run: &Path,
    nonce: &str,
    run_id: &str,
) -> io::Result<()> {
    let sm = tcx.sess.source_map();
    let items = tcx.hir_crate_items(());
    let mut defs: Vec<LocalDefId> = items
        .free_items()
        .map(|i| i.owner_id.def_id)
        .chain(items.impl_items().map(|i| i.owner_id.def_id))
        .chain(items.trait_items().map(|i| i.owner_id.def_id))
        .chain(tcx.hir_body_owners().filter(|&d| {
            matches!(tcx.def_kind(d), DefKind::AnonConst | DefKind::InlineConst)
                && nearest_fn(tcx, d).is_none()
        }))
        .collect();
    defs.sort_by_key(|d| d.local_def_index.as_u32());
    defs.dedup();
    // Every containing function of an impl can let a nested function escape its caller.
    let mut escapes = HashSet::new();
    for &def in &defs {
        if matches!(tcx.def_kind(def), DefKind::Impl { .. }) {
            let mut cur = tcx.opt_local_parent(def);
            while let Some(parent) = cur {
                if matches!(tcx.def_kind(parent), DefKind::Fn | DefKind::AssocFn) {
                    escapes.insert(parent);
                }
                cur = tcx.opt_local_parent(parent);
            }
        }
    }
    let mut rows = Vec::new();
    let mut indices = HashMap::new();
    let mut headers = BTreeSet::new();
    let mut folds = Vec::new();
    for def in defs {
        let def_kind = tcx.def_kind(def);
        if matches!(def_kind, DefKind::Mod | DefKind::ExternCrate) {
            continue;
        }
        let span = tcx.hir_span_with_body(tcx.local_def_id_to_hir_id(def));
        let site = span.source_callsite();
        let (mut kind, mut reg) = registration(tcx, def)?;
        if kind == "header" && site.is_dummy() {
            continue;
        }
        let file = match sm.span_to_filename(site) {
            FileName::Real(real) => real.local_path().map(std::fs::canonicalize).transpose()?,
            _ => None,
        };
        let Some(file) = file else {
            if kind == "header" {
                continue;
            }
            return Err(io::Error::other(format!(
                "executable item {def:?} has no real file"
            )));
        };
        let file = file
            .strip_prefix(root)
            .unwrap_or(&file)
            .to_string_lossy()
            .into_owned();
        let sf = sm.lookup_source_file(site.lo());
        if !std::sync::Arc::ptr_eq(&sf, &sm.lookup_source_file(site.hi())) {
            return Err(io::Error::other("item span crosses files"));
        }
        let (lo, hi) = (
            sf.original_relative_byte_pos(site.lo()).0,
            sf.original_relative_byte_pos(site.hi()).0,
        );
        if lo >= hi || hi > sf.unnormalized_source_len {
            return Err(io::Error::other(format!(
                "item {def:?} span {lo}..{hi} outside original source"
            )));
        }
        if kind == "header"
            && !matches!(def_kind, DefKind::Impl { .. } | DefKind::Trait)
            && !headers.insert((file.clone(), lo, hi, format!("{def_kind:?}")))
        {
            continue;
        }
        let mut fold = None;
        let mut escape = false;
        if def_kind == DefKind::Fn
            && let Some(outer) = nearest_fn(tcx, def)
        {
            kind = "nested_fn";
            fold = Some(outer);
            let mut target = outer;
            loop {
                escape |= escapes.contains(&target);
                if tcx.def_kind(target) != DefKind::Fn {
                    break;
                }
                match nearest_fn(tcx, target) {
                    Some(next) => target = next,
                    None => break,
                }
            }
            reg = if escape {
                Err("fold-escape".into())
            } else {
                registration(tcx, target)?.1
            };
        }
        let (path, reason) = match reg {
            Ok(path) => (Some(path), None),
            Err(why) => (None, Some(why)),
        };
        let index = rows.len();
        indices.insert(def, index);
        let mut row = json!({"file": file, "lo": lo, "hi": hi, "line": sm.lookup_char_pos(site.lo()).line,
            "def": def.local_def_index.as_u32(), "parent": tcx.opt_local_parent(def).map(|p| p.local_def_index.as_u32()),
            "macro": !span.ctxt().is_root(), "def_kind": format!("{def_kind:?}"), "kind": kind,
            "display": tcx.def_path_str(def), "path": path, "unregistrable": reason});
        if let Some(outer) = fold {
            row["escape"] = json!(escape);
            folds.push((index, outer));
        }
        rows.push(row);
    }
    for (index, outer) in folds {
        rows[index]["fold"] = json!(
            indices
                .get(&outer)
                .ok_or_else(|| io::Error::other("missing fold owner"))?
        );
    }
    if rows.is_empty() {
        return Err(io::Error::other("items has zero records"));
    }
    let header = json!({"schema": 1, "kind": "canary-items", "root": root, "crate": tcx.crate_name(LOCAL_CRATE).to_string(),
        "run_id": run_id, "nonce": nonce, "cfg_clippy": tcx.sess.psess.config.contains(&(rustc_span::sym::clippy, None))});
    let mut bytes = Vec::new();
    serde_json::to_writer(&mut bytes, &header)?;
    bytes.push(b'\n');
    for row in &rows {
        let mut record: Vec<&Value> = [
            "file",
            "lo",
            "hi",
            "kind",
            "path",
            "unregistrable",
            "display",
            "line",
            "def",
            "parent",
            "def_kind",
            "macro",
        ]
        .iter()
        .map(|key| &row[*key])
        .collect();
        if row["kind"] == "nested_fn" {
            record.extend([&row["fold"], &row["escape"]]);
        }
        serde_json::to_writer(&mut bytes, &record)?;
        bytes.push(b'\n');
    }
    let receipt: Value = json!({"sha256": digest(&bytes), "records": rows.len()});
    publish(&run.join("items.jsonl"), &bytes)?;
    publish(
        &run.join("items.jsonl.sha256"),
        &serde_json::to_vec(&receipt)?,
    )
}
