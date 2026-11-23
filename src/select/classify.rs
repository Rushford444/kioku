//! Lot L1 : la matrice de doute de la spec § 4 (invariants S1, S2, S3).
//!
//! [`classify`] ne renvoie jamais d'erreur : tout doute devient [`Scope::Everything`] avec
//! **toutes** les raisons trouvées (rien n'est court-circuité), sinon [`Scope::Partial`]
//! décrit ce que le plan (L4) doit élargir en plus de l'atteignabilité.
//!
//! Le lecteur de contenu ([`ContentSource`]) est injecté : [`RepoContent`] lit `git` et le
//! disque, un test passe une simple fermeture.
//!
//! Écarts assumés par rapport au brief (tous dans le sens « plus prudent ») :
//! - `ChangeSet::base` doit être le commit de l'inventaire, sinon `Everything` ;
//! - schémas de la base et du graphe vérifiés ;
//! - contenu `.rs` illisible → `Everything` (`Reason::Error`) ;
//! - détection macro élargie : `macro_rules!`, `macro_export`, `proc_macro` (espaces
//!   ignorés), en plus des nouveaux binaires `tests/<x>/main.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

use super::base::{run_command, validate_rel_path};
use super::{
    BASE_SCHEMA, BaseInventory, BaseStatus, BinaryKind, Change, ChangeSet, GRAPH_SCHEMA, GraphData,
    PartialScope, Reason, RelPath, Scope, TestId,
};

/// Globs inertes par défaut : documentation, scratch, Markdown à la racine.
pub const DEFAULT_INERT_GLOBS: [&str; 3] = ["docs/**", "scratch/**", "*.md"];
/// Branches protégées par défaut (S1).
pub const DEFAULT_PROTECTED_BRANCHES: [&str; 2] = ["main", "master"];

/// Arbres dans lesquels un fichier non Rust sans référence est un doute.
const COMPILED_TREES: [&str; 4] = ["src", "tests", "examples", "benches"];
/// Taille maximale d'un source lu pour la détection de macros.
const MAX_SOURCE: usize = 8 * 1024 * 1024;
const SHOW_TIMEOUT: Duration = Duration::from_secs(30);

/// Version d'un fichier : celle de la base ou celle de l'arbre de travail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// Contenu au commit de la base.
    Base,
    /// Contenu actuel (arbre de travail).
    Current,
}

/// Lecteur de contenu de fichiers pour la détection de macros.
///
/// `None` = fichier illisible ou absent : pour un fichier qui devrait exister dans cette
/// version, [`classify`] y voit un doute (`Everything`). Toute fermeture
/// `Fn(&str, Version) -> Option<String>` convient.
pub trait ContentSource {
    /// Contenu UTF-8 de `path` (relatif au dépôt) dans la version demandée.
    fn read(&self, path: &str, version: Version) -> Option<String>;
}

impl<F> ContentSource for F
where
    F: Fn(&str, Version) -> Option<String>,
{
    fn read(&self, path: &str, version: Version) -> Option<String> {
        self(path, version)
    }
}

/// Lecteur adossé à `git cat-file blob <base>:<path>` et au disque.
#[derive(Debug, Clone)]
pub struct RepoContent {
    repo: PathBuf,
    base: String,
}

impl RepoContent {
    /// `repo` : racine du dépôt ; `base` : commit de la base.
    #[must_use]
    pub fn new(repo: &Path, base: &str) -> Self {
        Self {
            repo: repo.to_path_buf(),
            base: base.to_owned(),
        }
    }
}

impl ContentSource for RepoContent {
    fn read(&self, path: &str, version: Version) -> Option<String> {
        validate_rel_path(path).ok()?;
        match version {
            Version::Base => {
                let spec = format!("{}:{path}", self.base);
                let out = run_command(
                    "git",
                    &["--no-optional-locks", "cat-file", "blob", &spec],
                    &self.repo,
                    SHOW_TIMEOUT,
                    MAX_SOURCE,
                )
                .ok()?;
                String::from_utf8(out).ok()
            }
            Version::Current => {
                let full = self.repo.join(path);
                let meta = std::fs::symlink_metadata(&full).ok()?;
                if !meta.is_file() || meta.len() > MAX_SOURCE as u64 {
                    return None;
                }
                std::fs::read_to_string(full).ok()
            }
        }
    }
}

/// Contexte courant de la classification (branche, outils, build).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassifyContext {
    /// Branche courante ; `None` = inconnue (HEAD détaché) : non protégée, mais notée.
    pub branch: Option<String>,
    /// Demande de merge en cours : toujours la suite complète (S1).
    pub is_merge_request: bool,
    /// Branches protégées (comparaison insensible à la casse ASCII).
    pub protected_branches: Vec<String>,
    /// Versions d'outils actuelles (mêmes clés que `BaseInventory::toolchain`).
    pub toolchain: BTreeMap<String, String>,
    /// Features actuelles.
    pub features: Vec<String>,
    /// Variables de build actuelles, valeurs hachées (voir `base::current_build_env`).
    pub build_env: BTreeMap<String, String>,
    /// Globs de fichiers sans effet sur les tests (`*` ne franchit pas `/`).
    pub inert_globs: Vec<String>,
}

impl Default for ClassifyContext {
    fn default() -> Self {
        Self {
            branch: None,
            is_merge_request: false,
            protected_branches: DEFAULT_PROTECTED_BRANCHES.map(str::to_owned).to_vec(),
            toolchain: BTreeMap::new(),
            features: Vec::new(),
            build_env: BTreeMap::new(),
            inert_globs: DEFAULT_INERT_GLOBS.map(str::to_owned).to_vec(),
        }
    }
}

/// Clôture transitive de `graph.edges` depuis `roots` (les racines en font partie).
#[must_use]
pub fn reach(graph: &GraphData, roots: &BTreeSet<RelPath>) -> BTreeSet<RelPath> {
    let mut seen = roots.clone();
    let mut stack: Vec<&RelPath> = roots.iter().collect();
    while let Some(file) = stack.pop() {
        if let Some(next) = graph.edges.get(file) {
            for target in next {
                if seen.insert(target.clone()) {
                    stack.push(target);
                }
            }
        }
    }
    seen
}

/// Rôle d'un chemin dans un changement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    Added,
    Modified,
    Deleted,
    RenamedFrom,
    RenamedTo,
}

impl Role {
    /// Le chemin n'existe plus qu'à la base.
    fn is_gone(self) -> bool {
        matches!(self, Self::Deleted | Self::RenamedFrom)
    }

    fn versions(self) -> &'static [Version] {
        match self {
            Self::Modified => &[Version::Base, Version::Current],
            Self::Added | Self::RenamedTo => &[Version::Current],
            Self::Deleted | Self::RenamedFrom => &[Version::Base],
        }
    }
}

fn paths_of(change: &Change) -> Vec<(&str, Role)> {
    match change {
        Change::Added { path } => vec![(path, Role::Added)],
        Change::Modified { path } => vec![(path, Role::Modified)],
        Change::Deleted { path } => vec![(path, Role::Deleted)],
        Change::Renamed { from, to } => vec![(from, Role::RenamedFrom), (to, Role::RenamedTo)],
    }
}

fn under(path: &str, tree: &str) -> bool {
    path.strip_prefix(tree)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn is_build_input(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    matches!(name, "Cargo.toml" | "Cargo.lock" | "build.rs" | "deny.toml")
        || name.starts_with("rust-toolchain")
        || path.split('/').any(|part| part == ".cargo")
        || under(path, ".config")
}

/// `tests/x.rs` ou `tests/x/main.rs` → `x` (cible d'intégration créée par ce fichier).
fn integration_target(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("tests/")?;
    let name = match rest.split_once('/') {
        None => rest.strip_suffix(".rs")?,
        Some((dir, "main.rs")) => dir,
        Some(_) => return None,
    };
    (!name.is_empty()).then_some(name)
}

fn defines_macro(source: &str) -> bool {
    let compact: String = source.chars().filter(|c| !c.is_whitespace()).collect();
    ["macro_rules!", "macro_export", "proc_macro"]
        .iter()
        .any(|marker| compact.contains(marker))
}

fn differing<'a>(
    base: &'a BTreeMap<String, String>,
    current: &'a BTreeMap<String, String>,
) -> Vec<&'a str> {
    let mut keys: BTreeSet<&str> = base.keys().map(String::as_str).collect();
    keys.extend(current.keys().map(String::as_str));
    keys.into_iter()
        .filter(|key| base.get(*key) != current.get(*key))
        .collect()
}

struct Engine<'a> {
    base: &'a BaseInventory,
    graph: &'a GraphData,
    ctx: &'a ClassifyContext,
    content: &'a dyn ContentSource,
    inert: GlobSet,
    reasons: Vec<Reason>,
    partial: PartialScope,
    notes: Vec<String>,
}

impl<'a> Engine<'a> {
    fn new(
        base: &'a BaseInventory,
        graph: &'a GraphData,
        ctx: &'a ClassifyContext,
        content: &'a dyn ContentSource,
    ) -> Self {
        let mut notes = Vec::new();
        let mut builder = GlobSetBuilder::new();
        for pattern in &ctx.inert_globs {
            match GlobBuilder::new(pattern).literal_separator(true).build() {
                Ok(glob) => {
                    builder.add(glob);
                }
                Err(_) => notes.push(format!("glob inerte invalide ignoré : {pattern}")),
            }
        }
        let inert = builder.build().unwrap_or_else(|_| {
            notes.push("globs inertes inutilisables : aucun fichier inerte".to_owned());
            GlobSet::empty()
        });
        Self {
            base,
            graph,
            ctx,
            content,
            inert,
            reasons: Vec::new(),
            partial: PartialScope::default(),
            notes,
        }
    }

    /// Ajoute une raison « tout » (sans doublon, ordre d'arrivée conservé).
    fn everything(&mut self, reason: Reason) {
        if !self.reasons.contains(&reason) {
            self.reasons.push(reason);
        }
    }

    // --- règle 1 : S1 ---
    fn check_protection(&mut self) {
        if self.ctx.is_merge_request {
            self.everything(Reason::ProtectedBranch("demande de merge".to_owned()));
        }
        let branch = self
            .ctx
            .branch
            .as_deref()
            .map(|name| name.strip_prefix("refs/heads/").unwrap_or(name))
            .filter(|name| !name.is_empty());
        let Some(branch) = branch else {
            self.notes.push(
                "branche inconnue (HEAD détaché ?) : non protégée, sélection permise".to_owned(),
            );
            return;
        };
        if self
            .ctx
            .protected_branches
            .iter()
            .any(|protected| protected.eq_ignore_ascii_case(branch))
        {
            self.everything(Reason::ProtectedBranch(branch.to_owned()));
        }
    }

    // --- règle 2 : S3 ---
    fn check_state(&mut self, changes: &ChangeSet) {
        let (base, ctx) = (self.base, self.ctx);
        if base.schema != BASE_SCHEMA {
            self.everything(Reason::Error(format!(
                "schéma de la base {:?}",
                base.schema
            )));
        }
        if self.graph.schema != GRAPH_SCHEMA {
            self.everything(Reason::Error(format!(
                "schéma du graphe {:?}",
                self.graph.schema
            )));
        }
        if changes.base != base.commit {
            self.everything(Reason::Error(format!(
                "le diff part de {:?}, l'inventaire de {:?}",
                changes.base, base.commit
            )));
        }
        if !base.green {
            self.everything(Reason::Error("base non verte".to_owned()));
        }
        if base.graph_digest.as_deref() != Some(self.graph.digest.as_str()) {
            self.everything(Reason::Error(format!(
                "digest du graphe {:?} différent de celui de la base {:?}",
                self.graph.digest, base.graph_digest
            )));
        }
        for key in differing(&base.toolchain, &ctx.toolchain) {
            let describe = |map: &BTreeMap<String, String>| {
                map.get(key).cloned().unwrap_or_else(|| "absent".to_owned())
            };
            self.reasons.push(Reason::BuildEnvironment(format!(
                "toolchain {key} : base {:?}, actuel {:?}",
                describe(&base.toolchain),
                describe(&ctx.toolchain)
            )));
        }
        let normalize =
            |features: &[String]| -> BTreeSet<String> { features.iter().cloned().collect() };
        if normalize(&base.features) != normalize(&ctx.features) {
            self.everything(Reason::BuildEnvironment(format!(
                "features : base {:?}, actuelles {:?}",
                base.features, ctx.features
            )));
        }
        let env = differing(&base.build_env, &ctx.build_env);
        if !env.is_empty() {
            self.everything(Reason::BuildEnvironment(format!(
                "variables de build différentes : {}",
                env.join(", ")
            )));
        }
    }

    // --- règles 3 à 9, par chemin ---
    fn visit(&mut self, path: &str, role: Role) {
        if let Err(message) = validate_rel_path(path) {
            self.everything(Reason::Error(format!("chemin de changement : {message}")));
            return;
        }
        // Cargo ne compile que les `.rs` en minuscules : `X.RS` reste un fichier non Rust.
        #[allow(clippy::case_sensitive_file_extension_comparisons)]
        let is_rust = path.ends_with(".rs");
        if is_build_input(path) {
            self.everything(Reason::BuildInput(path.to_owned()));
            if !is_rust {
                return;
            }
        }
        if is_rust {
            self.check_macro(path, role);
            if !(under(path, "src") || under(path, "tests")) {
                self.everything(Reason::UnknownRustFile(path.to_owned()));
            }
        }
        if role.is_gone() {
            self.record_gone(path);
        }
        if !is_rust {
            self.track_non_rust(path);
        } else if !role.is_gone() {
            self.track_rust(path, role);
        }
    }

    /// Règle 4 : macro dans la version base OU actuelle.
    fn check_macro(&mut self, path: &str, role: Role) {
        for version in role.versions() {
            match self.content.read(path, *version) {
                None => self.everything(Reason::Error(format!(
                    "contenu illisible ({version:?}) : {path}"
                ))),
                Some(text) if text.len() > MAX_SOURCE => self.everything(Reason::Error(format!(
                    "source trop grand ({version:?}) : {path}"
                ))),
                Some(text) => {
                    if defines_macro(&text) {
                        self.everything(Reason::Macro(path.to_owned()));
                    }
                }
            }
        }
    }

    /// Règle 9 : chemin de base supprimé ou renommé.
    fn record_gone(&mut self, path: &str) {
        self.partial.modified.insert(path.to_owned());
        for (binary, files) in &self.graph.binary_files {
            if files.contains(path) {
                self.partial
                    .whole_binaries
                    .entry(binary.clone())
                    .or_insert_with(|| Reason::DeletedOrRenamed(path.to_owned()));
            }
        }
    }

    /// Règle 6 : fichier non Rust.
    fn track_non_rust(&mut self, path: &str) {
        if let Some(citing) = self.graph.literal_refs.get(path)
            && !citing.is_empty()
        {
            self.partial.modified.extend(citing.iter().cloned());
        } else if COMPILED_TREES.iter().any(|tree| under(path, tree)) {
            self.everything(Reason::UnreferencedNonRust(path.to_owned()));
        } else if self.inert.is_match(path) {
            self.partial.inert.insert(path.to_owned());
        } else {
            self.everything(Reason::UnreferencedNonRust(path.to_owned()));
        }
    }

    /// Règles 7 et 8 : `.rs` présent dans l'arbre actuel.
    fn track_rust(&mut self, path: &str, role: Role) {
        if !(under(path, "src") || under(path, "tests")) {
            return; // déjà `UnknownRustFile`
        }
        if matches!(role, Role::Added | Role::RenamedTo)
            && let Some(target) = integration_target(path)
            && !self.target_exists(target, path)
        {
            self.new_binary(target);
            return;
        }
        self.partial.modified.insert(path.to_owned());
    }

    fn target_exists(&self, target: &str, path: &str) -> bool {
        self.base.binaries.iter().any(|binary| {
            (binary.kind == BinaryKind::Test && binary.target_name == target)
                || binary.src_path == path
        })
    }

    fn new_binary(&mut self, target: &str) {
        let packages: BTreeSet<&str> = self
            .base
            .binaries
            .iter()
            .filter_map(|binary| binary.binary_id.split("::").next())
            .collect();
        let mut packages = packages.into_iter();
        match (packages.next(), packages.next()) {
            (Some(package), None) => {
                let id = format!("{package}::{target}");
                self.partial
                    .whole_binaries
                    .entry(id.clone())
                    .or_insert(Reason::NewBinary(id));
            }
            _ => self.everything(Reason::Error(format!(
                "paquet de la base indéterminable pour le nouveau binaire {target}"
            ))),
        }
    }

    // --- règle 10 ---
    fn force_tests(&mut self) {
        for binary in &self.base.binaries {
            for test in &binary.tests {
                if test.status != BaseStatus::Pass {
                    self.partial.forced_tests.insert(
                        TestId {
                            binary_id: binary.binary_id.clone(),
                            name: test.name.clone(),
                        },
                        Reason::BaseNotGreen(test.status),
                    );
                }
            }
        }
        if self.graph.external_inputs.is_empty() {
            return;
        }
        let mut closures: BTreeMap<&BTreeSet<RelPath>, BTreeSet<RelPath>> = BTreeMap::new();
        for (test, roots) in &self.graph.test_roots {
            let reached = closures
                .entry(roots)
                .or_insert_with(|| reach(self.graph, roots));
            let hit = self
                .graph
                .external_inputs
                .iter()
                .find(|(_, citing)| citing.iter().any(|file| reached.contains(file)));
            if let Some((input, _)) = hit {
                self.partial
                    .forced_tests
                    .entry(test.clone())
                    .or_insert_with(|| Reason::ExternalInput(input.clone()));
            }
        }
    }
}

/// Classe les changements (matrice § 4) ; renvoie aussi les notes à journaliser (branche
/// inconnue, glob invalide).
#[must_use]
pub fn classify_noted(
    changes: &ChangeSet,
    base: &BaseInventory,
    graph: &GraphData,
    ctx: &ClassifyContext,
    content: &dyn ContentSource,
) -> (Scope, Vec<String>) {
    let mut engine = Engine::new(base, graph, ctx, content);
    engine.check_protection();
    engine.check_state(changes);
    for change in &changes.changes {
        for (path, role) in paths_of(change) {
            engine.visit(path, role);
        }
    }
    if engine.reasons.is_empty() {
        engine.force_tests();
    }
    let scope = if engine.reasons.is_empty() {
        Scope::Partial(std::mem::take(&mut engine.partial))
    } else {
        Scope::Everything {
            reasons: std::mem::take(&mut engine.reasons),
        }
    };
    (scope, std::mem::take(&mut engine.notes))
}

/// Classe les changements (matrice § 4). Jamais d'erreur : un doute donne `Everything`.
///
/// Règles, toutes accumulées : S1 (branche protégée, demande de merge), S3 (base non
/// verte, digest du graphe, toolchain, features, environnement de build), fichiers de
/// build, macros, `.rs` hors `src/` et `tests/`, non-Rust, nouveaux binaires,
/// suppressions et renommages, puis tests forcés (base non verte, entrées externes).
#[must_use]
pub fn classify(
    changes: &ChangeSet,
    base: &BaseInventory,
    graph: &GraphData,
    ctx: &ClassifyContext,
    content: &dyn ContentSource,
) -> Scope {
    classify_noted(changes, base, graph, ctx, content).0
}
