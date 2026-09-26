//! One pass over the expanded crate: a TSV row per module, `include!`d file and macro-made module holding hand-written
//! items, judged by scripts/ci/h2_depinfo.py; shapes no row can carry (unloaded, leftover macro call) are errors.
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rustc_ast::visit::{self, AssocCtxt, Visitor};
use rustc_ast::{self as ast, Inline, ItemKind, ModKind};
use rustc_session::Session;
use rustc_span::{FileName, Ident, Span};

const HEADER: &str =
    "file\tmodpath\titem_ctx\tident_ctx\tnested\tparent_file\tdecl_span\tattrs\tkind";

// `body`: the file a hand-written body is read from (`None` when a macro wrote its braces), so an item from another
// file there came by `include!`; `made`: any expansion defined the inline module, even from call-site tokens only.
enum Frame {
    Mod {
        name: String,
        file: Option<String>,
        body: Option<String>,
        made: bool,
    },
    Item {
        kind: &'static str,
        name: String,
        body: Option<String>,
    },
}

struct Walk<'a> {
    sess: &'a Session,
    expn: &'a dyn Fn(ast::NodeId) -> bool,
    root: &'a Path,
    crate_file: String,
    stack: Vec<Frame>,
    // True only while visiting an item written directly in a module body.
    direct_next: bool,
    rows: Vec<String>,
    marked: HashSet<String>,
    shape: Vec<(Span, String)>,
}

fn item_kind(kind: &ItemKind) -> &'static str {
    match kind {
        ItemKind::Fn(..) => "fn",
        ItemKind::Const(..) => "const",
        ItemKind::Static(..) => "static",
        ItemKind::Impl(..) => "impl",
        ItemKind::Trait(..) => "trait",
        ItemKind::Enum(..) => "enum",
        ItemKind::Struct(..) => "struct",
        ItemKind::Union(..) => "union",
        ItemKind::MacroDef(..) => "macrodef",
        _ => "item",
    }
}

// SyntaxContext::as_u32 is crate-private; its Debug form is `#N`, and anything unparsable counts as a macro context.
fn ctx(span: Span) -> u32 {
    let ctxt = span.ctxt();
    if ctxt.is_root() {
        return 0;
    }
    format!("{ctxt:?}")
        .trim_start_matches('#')
        .parse()
        .unwrap_or(u32::MAX)
}

// Repo-relative realpath, so symlinks and `..` name the file rustc read; a file outside the root stays absolute.
fn rel(root: &Path, file: &Path) -> String {
    let real = std::fs::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
    real.strip_prefix(root)
        .unwrap_or(&real)
        .to_string_lossy()
        .into_owned()
}

impl Walk<'_> {
    fn file_of(&self, span: Span) -> Option<String> {
        match self.sess.source_map().span_to_filename(span) {
            FileName::Real(real) => real.local_path().map(|path| rel(self.root, path)),
            _ => None,
        }
    }

    fn items(&mut self, items: &[Box<ast::Item>]) {
        for item in items {
            self.direct_next = true;
            self.visit_item(item);
        }
    }

    fn push_row(&mut self, span: Span, cells: [String; 9]) {
        if cells.iter().any(|cell| cell.chars().any(char::is_control)) {
            self.shape
                .push((span, "a module row has a control character".into()));
        } else {
            self.rows.push(cells.join("\t"));
        }
    }

    /// (module path, nearest enclosing file module's file, how the next item nests) at the current frame.
    fn place(&self, direct: bool) -> (Vec<String>, String, String) {
        let mut path = vec!["crate".to_string()];
        let mut parent = self.crate_file.clone();
        for frame in &self.stack {
            match frame {
                Frame::Mod { name, file, .. } => {
                    path.push(name.clone());
                    if let Some(file) = file {
                        parent = file.clone();
                    }
                }
                Frame::Item { kind, name, .. } => path.push(format!("{{{kind} {name}}}")),
            }
        }
        let nested = match (direct, self.stack.last()) {
            (true, _) => "module".to_string(),
            (false, Some(Frame::Item { kind, name, .. })) => format!("{kind}:{name}"),
            (false, _) => "block".to_string(),
        };
        (path, parent, nested)
    }

    /// Rows for a hand-written item in a body read from another file (`include`) or whose nearest module a macro
    /// made (`wrapped`), where the text walker cannot place it; one per kind, module path and file.
    fn mark(&mut self, span: Span, direct: bool) {
        let Some(file) = self.file_of(span).filter(|_| ctx(span) == 0) else {
            return;
        };
        let body = match self.stack.last() {
            None => Some(&self.crate_file),
            Some(Frame::Mod { body, .. } | Frame::Item { body, .. }) => body.as_ref(),
        };
        let spliced = body.is_some_and(|body| *body != file);
        let wrapped = self.stack.iter().rev().find_map(|frame| match frame {
            Frame::Mod { made, .. } => Some(*made),
            Frame::Item { .. } => None,
        }) == Some(true);
        if !spliced && !wrapped {
            return;
        }
        let (path, parent, nested) = self.place(direct);
        let (path, decl) = (path.join("::"), format!("{span:?}"));
        for kind in [("include", spliced), ("wrapped", wrapped)]
            .into_iter()
            .filter_map(|(kind, on)| on.then_some(kind))
        {
            if self.marked.insert(format!("{kind}\t{path}\t{file}")) {
                let cells = [
                    &*file, &*path, "#0", "#0", &*nested, &*parent, &*decl, "-", kind,
                ];
                self.push_row(span, cells.map(String::from));
            }
        }
    }

    fn record(
        &mut self,
        item: &ast::Item,
        ident: Ident,
        spans: &ast::ModSpans,
        direct: bool,
        kind: &str,
    ) {
        let file = match self.file_of(spans.inner_span) {
            Some(file) => file,
            None if kind == "inline" => "-".into(),
            None => {
                self.shape.push((
                    item.span,
                    format!("file module `{ident}` has no real source file"),
                ));
                return;
            }
        };
        let (mut path, parent, nested) = self.place(direct);
        path.push(ident.name.to_string());
        let attrs: Vec<String> = item
            .attrs
            .iter()
            .map(|attr| {
                let name = if attr.is_doc_comment() {
                    "doc".to_string()
                } else {
                    let segments = &attr.get_normal_item().path.segments;
                    segments
                        .first()
                        .map_or_else(|| "?".into(), |segment| segment.ident.name.to_string())
                };
                let value = if name == "path" {
                    attr.value_str().map(|v| format!("[{:?}]", v.as_str()))
                } else {
                    None
                };
                format!("{name}#{}{}", ctx(attr.span), value.unwrap_or_default())
            })
            .collect();
        let cells = [
            file,
            path.join("::"),
            format!("#{}", ctx(item.span)),
            format!("#{}", ctx(ident.span)),
            nested,
            parent,
            format!("{:?}", item.span),
            if attrs.is_empty() {
                "-".into()
            } else {
                attrs.join(",")
            },
            kind.into(),
        ];
        self.push_row(item.span, cells);
    }
}

impl<'ast> Visitor<'ast> for Walk<'_> {
    fn visit_item(&mut self, item: &'ast ast::Item) {
        let direct = std::mem::replace(&mut self.direct_next, false);
        self.mark(item.span, direct);
        match &item.kind {
            ItemKind::Mod(_, ident, ModKind::Loaded(items, inline, spans)) => {
                let kind = if let Inline::Yes = inline {
                    "inline"
                } else {
                    "file"
                };
                self.record(item, *ident, spans, direct, kind);
                let file = self.file_of(spans.inner_span);
                // a body in macro-written braces reads from the macro's file, so `include!` cannot be told apart there
                let body = file.clone().filter(|_| ctx(spans.inner_span) == 0);
                let by_macro = (self.expn)(item.id) || ctx(item.span) != 0 || ctx(ident.span) != 0;
                let made = kind == "inline" && (body.is_none() || by_macro);
                self.stack.push(Frame::Mod {
                    name: ident.name.to_string(),
                    file,
                    body,
                    made,
                });
                self.items(items);
                self.stack.pop();
            }
            ItemKind::Mod(_, ident, ModKind::Unloaded) => {
                self.shape.push((
                    item.span,
                    format!("module `{ident}` is still unloaded after expansion"),
                ));
            }
            ItemKind::MacCall(_) => self.shape.push((
                item.span,
                "an item macro call is left after expansion".into(),
            )),
            kind => {
                let name = kind
                    .ident()
                    .map_or_else(|| "?".into(), |ident| ident.name.to_string());
                let body = self.file_of(item.span).filter(|_| ctx(item.span) == 0);
                self.stack.push(Frame::Item {
                    kind: item_kind(kind),
                    name,
                    body,
                });
                visit::walk_item(self, item);
                self.stack.pop();
            }
        }
    }

    fn visit_assoc_item(&mut self, item: &'ast ast::AssocItem, ctxt: AssocCtxt) {
        self.direct_next = false;
        self.mark(item.span, false);
        let kind = match &item.kind {
            ast::AssocItemKind::Fn(..) => "fn",
            ast::AssocItemKind::Const(..) => "const",
            ast::AssocItemKind::Type(..) => "type",
            _ => "assoc",
        };
        let name = item
            .kind
            .ident()
            .map_or_else(|| "?".into(), |ident| ident.name.to_string());
        let body = self.file_of(item.span).filter(|_| ctx(item.span) == 0);
        self.stack.push(Frame::Item { kind, name, body });
        visit::walk_assoc_item(self, item, ctxt);
        self.stack.pop();
    }
}

/// The root lib row, then one row per module (`file` or `inline`) and per marked item (`include`, `wrapped`); `Err`
/// lists shapes no row can represent.
pub fn walk(
    sess: &Session,
    krate: &ast::Crate,
    root: &Path,
    expn: &dyn Fn(ast::NodeId) -> bool,
) -> Result<Vec<String>, Vec<(Span, String)>> {
    let mut walk = Walk {
        sess,
        expn,
        root,
        crate_file: String::new(),
        stack: vec![],
        direct_next: false,
        rows: vec![],
        marked: HashSet::new(),
        shape: vec![],
    };
    let span = krate.spans.inner_span;
    let Some(crate_file) = walk.file_of(span) else {
        return Err(vec![(
            span,
            "the crate root has no real source file".into(),
        )]);
    };
    let root_row = [
        crate_file.as_str(),
        "crate",
        "#0",
        "#0",
        "root",
        "-",
        "-",
        "-",
        "file",
    ]
    .map(String::from);
    walk.crate_file = crate_file;
    walk.push_row(span, root_row);
    walk.items(&krate.items);
    if walk.shape.is_empty() {
        Ok(walk.rows)
    } else {
        Err(walk.shape)
    }
}

/// Write the map through a sibling temp file, so a reader never sees a partial one under `out`.
pub fn write(out: &Path, rows: &[String]) -> std::io::Result<()> {
    let mut partial = out.as_os_str().to_owned();
    partial.push(".partial");
    let partial = PathBuf::from(partial);
    let text: String = std::iter::once(HEADER)
        .chain(rows.iter().map(String::as_str))
        .map(|line| format!("{line}\n"))
        .collect();
    std::fs::write(&partial, text)?;
    std::fs::rename(&partial, out)
}
