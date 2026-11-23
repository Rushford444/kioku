//! Analyse syntaxique des sources Rust (tree-sitter) : déclarations `mod x;`, `#[path]`,
//! macros `include*!`, chaînes littérales, lancement du binaire CLI ; arbre de modules
//! d'un crate et résolution « chemin de module → fichier ».
//!
//! Tout est une sur-approximation : les `#[cfg]` ne retirent rien, un fichier de module
//! ambigu relie tous les candidats existants. Une erreur de syntaxe est une erreur
//! (le graphe n'est alors pas utilisable : l'appelant retombe sur la suite complète).

use std::collections::{BTreeMap, BTreeSet};

use tree_sitter::{Node, Parser};

use crate::select::{RelPath, SelectError};

/// Taille maximale d'un fichier source analysé.
pub const MAX_SOURCE_BYTES: usize = 16 * 1024 * 1024;
/// Profondeur maximale de l'arbre syntaxique parcouru.
const MAX_DEPTH: usize = 400;
/// Taille maximale d'une chaîne littérale retenue.
const MAX_LITERAL_BYTES: usize = 64 * 1024;
/// Nombre maximal de nœuds visités dans un arbre de modules.
const MAX_TREE_NODES: usize = 200_000;
/// Profondeur maximale d'imbrication de modules suivie.
const MAX_MODULE_DEPTH: usize = 64;

/// Déclaration `mod x;` (sans corps).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModDecl {
    /// Nom du module.
    pub name: String,
    /// Valeurs des `#[path = "…"]` (ou `cfg_attr(…, path = "…")`) attachées.
    pub path_attrs: Vec<String>,
    /// Modules inline englobants (`mod a { mod b; }` → `["a"]`).
    pub inline: Vec<String>,
}

/// Famille de macro d'inclusion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum IncludeKind {
    /// `include!` (fragment Rust).
    Include,
    /// `include_str!`.
    Str,
    /// `include_bytes!`.
    Bytes,
}

/// Invocation d'une macro d'inclusion et chaînes littérales de ses arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncludeRef {
    /// Macro.
    pub kind: IncludeKind,
    /// Littéraux trouvés dans les arguments (`concat!(env!("CARGO_MANIFEST_DIR"), "/x")`
    /// donne `["CARGO_MANIFEST_DIR", "/x"]` : l'appelant essaie chaque candidat).
    pub literals: Vec<String>,
}

/// Résultat de l'analyse d'un fichier.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileScan {
    /// Modules déclarés hors ligne.
    pub mods: Vec<ModDecl>,
    /// Chemins des modules inline (`["a"]`, `["a", "b"]`) : portés par ce fichier.
    pub inline_mods: Vec<Vec<String>>,
    /// Macros d'inclusion.
    pub includes: Vec<IncludeRef>,
    /// Toutes les chaînes littérales (décodées).
    pub literals: BTreeSet<String>,
    /// Le fichier lance le binaire du paquet (`CARGO_BIN_EXE_*`, `cargo_bin(…)`).
    pub launches_cli: bool,
}

fn invalid(message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: "rust source".to_string(),
        message: message.into(),
    }
}

/// Analyse un fichier source Rust.
///
/// # Errors
/// Source trop grande, grammaire indisponible, arbre trop profond ou erreur de syntaxe.
pub fn scan_source(source: &[u8]) -> Result<FileScan, SelectError> {
    if source.len() > MAX_SOURCE_BYTES {
        return Err(invalid(format!(
            "source larger than {MAX_SOURCE_BYTES} bytes"
        )));
    }
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_rust::LANGUAGE.into())
        .map_err(|error| invalid(format!("grammar unavailable: {error}")))?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| invalid("parser produced no tree"))?;
    let root = tree.root_node();
    if root.has_error() {
        return Err(invalid("syntax error"));
    }
    let mut out = FileScan::default();
    let mut inline = Vec::new();
    visit(root, source, &mut inline, 0, &mut out)?;
    Ok(out)
}

/// Préfixe des variables d'environnement qui donnent le chemin d'un binaire du paquet.
const CLI_ENV_PREFIX: &str = "CARGO_BIN_EXE_";

/// Fonctions et macros qui lancent un binaire du paquet (`assert_cmd`).
fn is_cli_function(name: &str) -> bool {
    matches!(name, "cargo_bin" | "cargo_bin_cmd")
}

fn node_text(node: Node<'_>, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).into_owned()
}

fn visit(
    node: Node<'_>,
    source: &[u8],
    inline: &mut Vec<String>,
    depth: usize,
    out: &mut FileScan,
) -> Result<(), SelectError> {
    if depth > MAX_DEPTH {
        return Err(invalid("syntax tree too deep"));
    }
    match node.kind() {
        "mod_item" => {
            if let Some(name) = node.child_by_field_name("name") {
                let name = node_text(name, source);
                if node.child_by_field_name("body").is_some() {
                    inline.push(name);
                    out.inline_mods.push(inline.clone());
                    let walked = visit_children(node, source, inline, depth, out);
                    inline.pop();
                    return walked;
                }
                out.mods.push(ModDecl {
                    name,
                    path_attrs: path_attributes_before(node, source),
                    inline: inline.clone(),
                });
                return Ok(());
            }
        }
        "macro_invocation" => {
            if let Some(kind) = include_kind(node, source) {
                let mut literals = Vec::new();
                collect_literals(node, source, 0, &mut literals)?;
                out.includes.push(IncludeRef { kind, literals });
            }
        }
        "identifier" | "field_identifier" => {
            if is_cli_function(&node_text(node, source)) {
                out.launches_cli = true;
            }
            return Ok(());
        }
        "string_literal" | "raw_string_literal" => {
            if let Some(text) = decode_string_literal(&node_text(node, source)) {
                // `env!("CARGO_BIN_EXE_kioku")` : le littéral porte le nom du binaire (le préfixe
                // seul, sans nom, n'est pas un lancement).
                if text.len() > CLI_ENV_PREFIX.len() && text.starts_with(CLI_ENV_PREFIX) {
                    out.launches_cli = true;
                }
                if text.len() <= MAX_LITERAL_BYTES {
                    out.literals.insert(text);
                }
            }
            return Ok(());
        }
        _ => {}
    }
    visit_children(node, source, inline, depth, out)
}

fn visit_children(
    node: Node<'_>,
    source: &[u8],
    inline: &mut Vec<String>,
    depth: usize,
    out: &mut FileScan,
) -> Result<(), SelectError> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        visit(child, source, inline, depth + 1, out)?;
    }
    Ok(())
}

fn include_kind(node: Node<'_>, source: &[u8]) -> Option<IncludeKind> {
    let name = node_text(node.child_by_field_name("macro")?, source);
    match name.rsplit("::").next()?.trim() {
        "include" => Some(IncludeKind::Include),
        "include_str" => Some(IncludeKind::Str),
        "include_bytes" => Some(IncludeKind::Bytes),
        _ => None,
    }
}

fn collect_literals(
    node: Node<'_>,
    source: &[u8],
    depth: usize,
    out: &mut Vec<String>,
) -> Result<(), SelectError> {
    if depth > MAX_DEPTH {
        return Err(invalid("syntax tree too deep"));
    }
    if matches!(node.kind(), "string_literal" | "raw_string_literal") {
        if let Some(text) = decode_string_literal(&node_text(node, source)) {
            out.push(text);
        }
        return Ok(());
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_literals(child, source, depth + 1, out)?;
    }
    Ok(())
}

/// Valeurs `path = "…"` des attributs qui précèdent immédiatement `node`.
fn path_attributes_before(node: Node<'_>, source: &[u8]) -> Vec<String> {
    let mut values = Vec::new();
    let mut sibling = node.prev_sibling();
    while let Some(current) = sibling {
        match current.kind() {
            "attribute_item" => {
                values.extend(path_values_in_attribute(&node_text(current, source)));
            }
            "line_comment" | "block_comment" => {}
            _ => break,
        }
        sibling = current.prev_sibling();
    }
    values
}

/// Extrait les `path = "…"` d'un texte d'attribut (`#[path = "x"]`, `#[cfg_attr(a, path = "x")]`).
#[must_use]
pub fn path_values_in_attribute(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut values = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("path") {
        let start = from + found;
        let end = start + 4;
        from = end;
        let before_ok = start == 0 || !is_ident_byte(bytes[start - 1]);
        let after_ok = end >= bytes.len() || !is_ident_byte(bytes[end]);
        if !(before_ok && after_ok) {
            continue;
        }
        let rest = text[end..].trim_start();
        let Some(rest) = rest.strip_prefix('=') else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.starts_with('"') {
            continue;
        }
        // Chaîne ordinaire : on lit jusqu'au guillemet non échappé.
        let mut closing = None;
        let mut escaped = false;
        for (index, byte) in rest.bytes().enumerate().skip(1) {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                closing = Some(index);
                break;
            }
        }
        if let Some(closing) = closing
            && let Some(value) = decode_string_literal(&rest[..=closing])
        {
            values.push(value);
        }
    }
    values
}

fn is_ident_byte(byte: u8) -> bool {
    byte == b'_' || byte.is_ascii_alphanumeric()
}

/// Décode le texte d'un littéral chaîne (`"x"`, `b"x"`, `r#"x"#`, `c"x"`).
#[must_use]
pub fn decode_string_literal(raw: &str) -> Option<String> {
    let trimmed = raw.trim_start_matches(['b', 'c']);
    let is_raw = trimmed.starts_with('r');
    let body = trimmed.strip_prefix('r').unwrap_or(trimmed);
    let hashes = body.bytes().take_while(|byte| *byte == b'#').count();
    let body = &body[hashes..];
    let body = body.strip_prefix('"')?;
    let closing = format!("\"{}", "#".repeat(hashes));
    let body = body.strip_suffix(closing.as_str())?;
    if is_raw {
        return Some(body.to_string());
    }
    let mut decoded = String::with_capacity(body.len());
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            decoded.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => decoded.push('\n'),
            Some('r') => decoded.push('\r'),
            Some('t') => decoded.push('\t'),
            Some('0') => decoded.push('\0'),
            Some('"') => decoded.push('"'),
            Some('\'') => decoded.push('\''),
            Some('\n') => {
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
            }
            Some('\\') | None => decoded.push('\\'),
            Some(other) => {
                decoded.push('\\');
                decoded.push(other);
            }
        }
    }
    Some(decoded)
}

/// Normalise lexicalement `dir/relative` (séparateur `/`, `.` et `..` résolus).
/// `None` si le résultat sort de la racine, est absolu ou contient un octet nul.
#[must_use]
pub fn normalize_join(dir: &str, relative: &str) -> Option<RelPath> {
    if relative.starts_with('/') || relative.contains('\0') || relative.contains('\\') {
        return None;
    }
    let mut parts: Vec<&str> = dir
        .split('/')
        .filter(|p| !p.is_empty() && *p != ".")
        .collect();
    for part in relative.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("/"))
}

/// Dossier d'un chemin relatif (`""` à la racine).
#[must_use]
pub fn dir_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(dir, _)| dir)
}

fn stem_of(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    name.strip_suffix(".rs").unwrap_or(name)
}

/// Arbre de modules d'un crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModuleTree {
    /// Fichier racine du crate.
    pub root: RelPath,
    /// Chemin de module → fichier qui le porte (un module inline est porté par son fichier).
    pub modules: BTreeMap<Vec<String>, RelPath>,
    /// Liaisons parent → enfant : `(parent, enfant, #[path] ?)`.
    pub links: BTreeSet<(RelPath, RelPath, bool)>,
    /// Fichiers Rust du crate (racine incluse, cibles de `include!` comprises).
    pub members: BTreeSet<RelPath>,
}

impl ModuleTree {
    /// Fichier du module le plus profond qui existe parmi les préfixes de `module_path`
    /// (la racine à défaut).
    #[must_use]
    pub fn resolve(&self, module_path: &[String]) -> &RelPath {
        for len in (0..=module_path.len()).rev() {
            if let Some(file) = self.modules.get(&module_path[..len]) {
                return file;
            }
        }
        &self.root
    }

    /// Fichier qui déclare le module `module_path` (le parent logique), si c'est un autre
    /// fichier que celui du module.
    #[must_use]
    pub fn declaring_file(&self, module_path: &[String]) -> Option<&RelPath> {
        let own = self.modules.get(module_path)?;
        let parent = self
            .modules
            .get(module_path.get(..module_path.len().checked_sub(1)?)?)?;
        (own != parent).then_some(parent)
    }
}

struct Pending {
    file: RelPath,
    module: Vec<String>,
    /// `Some(true)` : le fichier possède son dossier (`mod.rs`, racine, `#[path]`),
    /// `Some(false)` : non. `None` : incertain, les deux variantes sont essayées.
    owner: Option<bool>,
    depth: usize,
}

/// Construit l'arbre de modules du crate de racine `root`.
///
/// `scans` contient l'analyse de chaque fichier `.rs` du dépôt, `exists` dit si un chemin
/// relatif est un fichier du dépôt.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn build_tree(
    root: &str,
    scans: &BTreeMap<RelPath, FileScan>,
    exists: &dyn Fn(&str) -> bool,
) -> ModuleTree {
    let mut tree = ModuleTree {
        root: root.to_string(),
        ..ModuleTree::default()
    };
    tree.modules.insert(Vec::new(), root.to_string());
    tree.members.insert(root.to_string());
    let mut visited: BTreeSet<(RelPath, Vec<String>)> = BTreeSet::new();
    let mut stack = vec![Pending {
        file: root.to_string(),
        module: Vec::new(),
        owner: Some(true),
        depth: 0,
    }];
    while let Some(item) = stack.pop() {
        if visited.len() >= MAX_TREE_NODES || item.depth > MAX_MODULE_DEPTH {
            continue;
        }
        if !visited.insert((item.file.clone(), item.module.clone())) {
            continue;
        }
        let Some(scan) = scans.get(&item.file) else {
            continue;
        };
        for inline in &scan.inline_mods {
            let mut path = item.module.clone();
            path.extend(inline.iter().cloned());
            tree.modules
                .entry(path)
                .or_insert_with(|| item.file.clone());
        }
        let owners: &[bool] = match item.owner {
            Some(true) => &[true],
            Some(false) => &[false],
            None => &[true, false],
        };
        for decl in &scan.mods {
            let mut child_module = item.module.clone();
            child_module.extend(decl.inline.iter().cloned());
            child_module.push(decl.name.clone());
            let file_dir = dir_of(&item.file);
            let mut found: Vec<(RelPath, bool, Option<bool>)> = Vec::new();
            for &owns in owners {
                let base = if owns {
                    file_dir.to_string()
                } else {
                    join(file_dir, stem_of(&item.file))
                };
                let mut inline_dir = base;
                for segment in &decl.inline {
                    inline_dir = join(&inline_dir, segment);
                }
                if decl.path_attrs.is_empty() {
                    for (candidate, child_owns) in [
                        (format!("{}.rs", decl.name), Some(false)),
                        (format!("{}/mod.rs", decl.name), Some(true)),
                    ] {
                        if let Some(path) = normalize_join(&inline_dir, &candidate) {
                            found.push((path, false, child_owns));
                        }
                    }
                } else {
                    // `#[path]` hors module inline : relatif au dossier du fichier.
                    let anchor = if decl.inline.is_empty() {
                        file_dir
                    } else {
                        &inline_dir
                    };
                    for value in &decl.path_attrs {
                        if let Some(path) = normalize_join(anchor, value) {
                            found.push((path, true, None));
                        }
                    }
                }
            }
            for (path, via_path, child_owner) in found {
                if !exists(&path) || !scans.contains_key(&path) {
                    continue;
                }
                tree.links
                    .insert((item.file.clone(), path.clone(), via_path));
                tree.members.insert(path.clone());
                tree.modules
                    .entry(child_module.clone())
                    .or_insert_with(|| path.clone());
                stack.push(Pending {
                    file: path,
                    module: child_module.clone(),
                    owner: child_owner,
                    depth: item.depth + 1,
                });
            }
        }
        for include in &scan.includes {
            if include.kind != IncludeKind::Include {
                continue;
            }
            for literal in &include.literals {
                for candidate in include_candidates(&item.file, literal) {
                    // `scans` ne contient que des fichiers Rust.
                    if exists(&candidate) && scans.contains_key(&candidate) {
                        tree.links
                            .insert((item.file.clone(), candidate.clone(), false));
                        tree.members.insert(candidate.clone());
                        stack.push(Pending {
                            file: candidate,
                            module: item.module.clone(),
                            owner: item.owner,
                            depth: item.depth + 1,
                        });
                    }
                }
            }
        }
    }
    tree
}

fn join(dir: &str, segment: &str) -> String {
    if dir.is_empty() {
        segment.to_string()
    } else {
        format!("{dir}/{segment}")
    }
}

/// Chemins candidats pour un littéral d'une macro d'inclusion écrite dans `file` :
/// relatif au fichier, puis relatif à la racine (cas `concat!(env!("CARGO_MANIFEST_DIR"), "/x")`).
#[must_use]
pub fn include_candidates(file: &str, literal: &str) -> Vec<RelPath> {
    let mut out = Vec::new();
    if let Some(path) = normalize_join(dir_of(file), literal) {
        out.push(path);
    }
    if let Some(path) = normalize_join("", literal.trim_start_matches('/'))
        && !out.contains(&path)
    {
        out.push(path);
    }
    out
}
