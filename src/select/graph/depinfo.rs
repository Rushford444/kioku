//! Cargo : cibles (`cargo metadata`), compilation des binaires de test, inventaire
//! (`cargo nextest list --message-format json`) et dep-info rustc (`target/debug/deps/*.d`).

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::select::{BinaryId, RelPath, SelectError};

use super::scan::normalize_join;
use super::{CommandSpec, capture, run_command};

fn invalid(what: &str, message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: what.to_string(),
        message: message.into(),
    }
}

/// Décode un fichier dep-info (syntaxe Makefile de rustc) et retourne les dépendances de
/// la première règle (les règles suivantes répètent les mêmes fichiers).
///
/// Échappements rustc : `\ ` (espace), `\\`, `\n`, `\r` ; `\#` et `$$` sont aussi
/// compris. Les lignes commençant par `#` (`# env-dep:`) sont des commentaires.
///
/// # Errors
/// Fichier sans règle (ni cible) : un dep-info vide est suspect, jamais « aucun fichier ».
pub fn parse_dep_info(text: &str) -> Result<Vec<String>, SelectError> {
    let mut words: Vec<(String, bool)> = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut line_start = true;
    let mut chars = text.chars().peekable();
    let finish = |word: &mut String, in_word: &mut bool, words: &mut Vec<(String, bool)>| {
        if *in_word {
            let is_target = word.ends_with(':');
            let mut value = std::mem::take(word);
            if is_target {
                value.pop();
            }
            words.push((value, is_target));
            *in_word = false;
        }
    };
    while let Some(ch) = chars.next() {
        match ch {
            '#' if line_start && !in_word => {
                for rest in chars.by_ref() {
                    if rest == '\n' {
                        break;
                    }
                }
                continue;
            }
            '\\' => match chars.next() {
                Some('\n') => finish(&mut word, &mut in_word, &mut words),
                Some('\r') if chars.peek() == Some(&'\n') => {
                    chars.next();
                    finish(&mut word, &mut in_word, &mut words);
                }
                Some(' ') => {
                    word.push(' ');
                    in_word = true;
                }
                Some('#') => {
                    word.push('#');
                    in_word = true;
                }
                Some('n') => {
                    word.push('\n');
                    in_word = true;
                }
                Some('r') => {
                    word.push('\r');
                    in_word = true;
                }
                // `\\` seul ou barre oblique finale : une barre oblique inverse.
                Some('\\') | None => {
                    word.push('\\');
                    in_word = true;
                }
                Some(other) => {
                    word.push('\\');
                    word.push(other);
                    in_word = true;
                }
            },
            '$' if chars.peek() == Some(&'$') => {
                chars.next();
                word.push('$');
                in_word = true;
            }
            c if c.is_whitespace() => {
                finish(&mut word, &mut in_word, &mut words);
                line_start = c == '\n';
                continue;
            }
            c => {
                word.push(c);
                in_word = true;
            }
        }
        line_start = false;
    }
    finish(&mut word, &mut in_word, &mut words);
    let first = words
        .iter()
        .position(|(_, is_target)| *is_target)
        .ok_or_else(|| invalid("dep-info", "no rule found"))?;
    Ok(words[first + 1..]
        .iter()
        .take_while(|(_, is_target)| !is_target)
        .map(|(value, _)| value.clone())
        .collect())
}

/// Fichiers du dépôt cités par un dep-info : chemins relatifs normalisés (`tests/../src/x.rs`
/// donne `src/x.rs`), chemins absolus ramenés à la racine ; les fichiers hors dépôt, sous
/// `target/` ou sous `.git/` sont ignorés.
#[must_use]
pub fn repo_files(dependencies: &[String], roots: &[&Path]) -> BTreeSet<RelPath> {
    let mut files = BTreeSet::new();
    for dependency in dependencies {
        let path = Path::new(dependency);
        let relative: Option<String> = if path.is_absolute() {
            roots.iter().find_map(|root| {
                path.strip_prefix(root)
                    .ok()
                    .and_then(|rest| rest.to_str())
                    .and_then(|rest| normalize_join("", rest))
            })
        } else {
            normalize_join("", dependency)
        };
        let Some(relative) = relative else {
            continue;
        };
        if relative == "target" || relative.starts_with("target/") || relative.starts_with(".git/")
        {
            continue;
        }
        files.insert(relative);
    }
    files
}

/// Cible cargo d'un paquet (`cargo metadata`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CargoTarget {
    /// Paquet.
    pub package: String,
    /// Nom de la cible.
    pub name: String,
    /// Genres cargo (`lib`, `bin`, `test`, `example`, `bench`, `custom-build`…).
    pub kinds: Vec<String>,
    /// Fichier racine relatif au dépôt.
    pub src_path: RelPath,
}

#[derive(Deserialize)]
struct MetadataJson {
    packages: Vec<PackageJson>,
}

#[derive(Deserialize)]
struct PackageJson {
    name: String,
    targets: Vec<TargetJson>,
}

#[derive(Deserialize)]
struct TargetJson {
    name: String,
    kind: Vec<String>,
    src_path: String,
}

/// Décode la sortie de `cargo metadata --no-deps`.
///
/// # Errors
/// JSON illisible ou chemin de cible hors du dépôt.
pub fn parse_cargo_metadata(json: &[u8], roots: &[&Path]) -> Result<Vec<CargoTarget>, SelectError> {
    let parsed: MetadataJson = serde_json::from_slice(json)
        .map_err(|error| invalid("cargo metadata", error.to_string()))?;
    let mut targets = Vec::new();
    for package in parsed.packages {
        for target in package.targets {
            let src = Path::new(&target.src_path);
            let relative = if src.is_absolute() {
                roots.iter().find_map(|root| {
                    src.strip_prefix(root)
                        .ok()
                        .and_then(|rest| rest.to_str())
                        .and_then(|rest| normalize_join("", rest))
                })
            } else {
                normalize_join("", &target.src_path)
            }
            .ok_or_else(|| {
                invalid(
                    "cargo metadata",
                    format!("target {} lies outside the repository", target.name),
                )
            })?;
            targets.push(CargoTarget {
                package: package.name.clone(),
                name: target.name,
                kinds: target.kind,
                src_path: relative,
            });
        }
    }
    Ok(targets)
}

/// Binaire de test listé par nextest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NextestSuite {
    /// Identifiant nextest (`kdw-kioku`, `kdw-kioku::bin/kioku`, `kdw-kioku::cli`).
    pub binary_id: BinaryId,
    /// Paquet.
    pub package: String,
    /// Nom de la cible.
    pub binary_name: String,
    /// `lib`, `bin`, `test`, `example`, `bench`…
    pub kind: String,
    /// Exécutable de test (`…/target/debug/deps/cli-<hash>`).
    pub binary_path: PathBuf,
    /// Tests : nom → ignoré ?
    pub tests: BTreeMap<String, bool>,
}

#[derive(Deserialize)]
struct ListJson {
    #[serde(rename = "rust-suites")]
    rust_suites: BTreeMap<String, SuiteJson>,
}

#[derive(Deserialize)]
struct SuiteJson {
    #[serde(rename = "package-name")]
    package_name: String,
    #[serde(rename = "binary-name")]
    binary_name: String,
    kind: String,
    #[serde(rename = "binary-path")]
    binary_path: String,
    #[serde(default)]
    testcases: BTreeMap<String, CaseJson>,
}

#[derive(Deserialize)]
struct CaseJson {
    #[serde(default)]
    ignored: bool,
}

/// Décode la sortie de `cargo nextest list --message-format json`.
///
/// # Errors
/// JSON illisible.
pub fn parse_nextest_list(json: &[u8]) -> Result<Vec<NextestSuite>, SelectError> {
    let parsed: ListJson =
        serde_json::from_slice(json).map_err(|error| invalid("nextest list", error.to_string()))?;
    Ok(parsed
        .rust_suites
        .into_iter()
        .map(|(binary_id, suite)| NextestSuite {
            binary_id,
            package: suite.package_name,
            binary_name: suite.binary_name,
            kind: suite.kind,
            binary_path: PathBuf::from(suite.binary_path),
            tests: suite
                .testcases
                .into_iter()
                .map(|(name, case)| (name, case.ignored))
                .collect(),
        })
        .collect())
}

/// Contexte d'appel de cargo.
#[derive(Debug, Clone)]
pub struct CargoContext<'a> {
    /// Racine du dépôt.
    pub repo: &'a Path,
    /// Features demandées.
    pub features: &'a [String],
    /// Toutes les features.
    pub all_features: bool,
    /// Délai de la compilation.
    pub build_timeout: Duration,
}

impl CargoContext<'_> {
    fn feature_args(&self) -> Vec<OsString> {
        if self.all_features {
            vec!["--all-features".into()]
        } else if self.features.is_empty() {
            Vec::new()
        } else {
            vec!["--features".into(), self.features.join(",").into()]
        }
    }

    fn command(&self, args: Vec<OsString>, label: &str, stdout: Option<PathBuf>) -> CommandSpec {
        CommandSpec {
            program: "cargo".into(),
            args,
            cwd: self.repo.to_path_buf(),
            timeout: self.build_timeout,
            stdout,
            label: label.to_string(),
        }
    }

    /// `cargo metadata --no-deps` : cibles du dépôt.
    ///
    /// # Errors
    /// Commande en échec, JSON illisible, cible hors du dépôt.
    pub fn targets(&self, roots: &[&Path]) -> Result<Vec<CargoTarget>, SelectError> {
        let spec = self.command(
            ["metadata", "--no-deps", "--format-version", "1", "--locked"]
                .map(OsString::from)
                .to_vec(),
            "cargo metadata",
            None,
        );
        let output = capture(&spec)?;
        parse_cargo_metadata(&output, roots)
    }

    /// `cargo nextest run --no-run --locked [features]` : compile les binaires de test.
    ///
    /// # Errors
    /// Compilation en échec ou délai dépassé.
    pub fn compile_tests(&self) -> Result<(), SelectError> {
        let mut args: Vec<OsString> = ["nextest", "run", "--no-run", "--locked"]
            .map(OsString::from)
            .to_vec();
        args.extend(self.feature_args());
        run_command(&self.command(args, "cargo nextest run --no-run", None))
    }

    /// `cargo nextest list --message-format json --locked [features]`.
    ///
    /// # Errors
    /// Commande en échec, JSON illisible.
    pub fn list_tests(&self) -> Result<Vec<NextestSuite>, SelectError> {
        let mut args: Vec<OsString> = ["nextest", "list", "--message-format", "json", "--locked"]
            .map(OsString::from)
            .to_vec();
        args.extend(self.feature_args());
        let output = capture(&self.command(args, "cargo nextest list", None))?;
        parse_nextest_list(&output)
    }
}

/// Fichiers compilés dans un binaire de test : lit `<binary_path>.d`.
///
/// # Errors
/// Fichier dep-info absent, illisible ou sans règle.
pub fn binary_files(
    suite: &NextestSuite,
    roots: &[&Path],
) -> Result<BTreeSet<RelPath>, SelectError> {
    let mut name = suite.binary_path.clone().into_os_string();
    name.push(".d");
    let path = PathBuf::from(name);
    let text = std::fs::read_to_string(&path).map_err(|error| SelectError::Io {
        path: path.display().to_string(),
        message: error.to_string(),
    })?;
    let dependencies = parse_dep_info(&text)?;
    Ok(repo_files(&dependencies, roots))
}
