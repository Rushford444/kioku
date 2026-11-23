//! Magasin de la sélection : `base-<commit>.json`, `graph-<digest>.json`,
//! `learned-<commit>.json`, écrits de façon atomique.
//!
//! Racine : `--store DIR`, sinon `$KIOKU_SELECT_STORE`, sinon `$XDG_CACHE_HOME/kioku/select`,
//! sinon `$HOME/.cache/kioku/select`. Les noms de fichiers viennent d'un commit (40 hex) ou
//! d'un digest (caractères sûrs) : jamais de chemin construit à partir d'une entrée libre.

use std::path::{Path, PathBuf};

use crate::select::base::{self, invalid};
use crate::select::journal::read_bounded;
use crate::select::learn::{self, LearnedEdges};
use crate::select::{BaseInventory, GraphData, SelectError, graph};

/// Magasin sur disque.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Store {
    root: PathBuf,
}

type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<std::ffi::OsString>;

fn non_empty_env(env: EnvLookup<'_>, name: &str) -> Option<PathBuf> {
    env(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn is_commit(commit: &str) -> bool {
    commit.len() == 40
        && commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn is_safe_digest(digest: &str) -> bool {
    !digest.is_empty()
        && digest.len() <= 128
        && digest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

impl Store {
    /// Magasin de racine donnée.
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Racine par défaut (voir le module), ou `explicit` si fourni.
    ///
    /// # Errors
    /// [`SelectError::Invalid`] si aucune racine ne peut être déterminée.
    pub fn resolve(explicit: Option<&Path>) -> Result<Self, SelectError> {
        Self::resolve_with(explicit, &|name| std::env::var_os(name))
    }

    /// Comme [`Store::resolve`] avec un accès à l'environnement injecté (tests).
    ///
    /// # Errors
    /// [`SelectError::Invalid`] si aucune racine ne peut être déterminée.
    pub fn resolve_with(explicit: Option<&Path>, env: EnvLookup<'_>) -> Result<Self, SelectError> {
        if let Some(dir) = explicit {
            return Ok(Self::new(dir));
        }
        if let Some(dir) = non_empty_env(env, "KIOKU_SELECT_STORE") {
            return Ok(Self::new(dir));
        }
        if let Some(xdg) = non_empty_env(env, "XDG_CACHE_HOME").filter(|p| p.is_absolute()) {
            return Ok(Self::new(xdg.join("kioku").join("select")));
        }
        if let Some(home) = non_empty_env(env, "HOME") {
            return Ok(Self::new(home.join(".cache").join("kioku").join("select")));
        }
        Err(invalid(
            "store",
            "aucune racine : ni --store, ni KIOKU_SELECT_STORE, ni XDG_CACHE_HOME, ni HOME",
        ))
    }

    /// Dossier racine.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn commit_file(&self, prefix: &str, commit: &str) -> Result<PathBuf, SelectError> {
        if !is_commit(commit) {
            return Err(invalid(
                "store",
                format!("commit attendu en 40 hexadécimaux minuscules : {commit:?}"),
            ));
        }
        Ok(self.root.join(format!("{prefix}-{commit}.json")))
    }

    /// `base-<commit>.json`.
    ///
    /// # Errors
    /// Commit mal formé.
    pub fn base_path(&self, commit: &str) -> Result<PathBuf, SelectError> {
        self.commit_file("base", commit)
    }

    /// `learned-<commit>.json`.
    ///
    /// # Errors
    /// Commit mal formé.
    pub fn learned_path(&self, commit: &str) -> Result<PathBuf, SelectError> {
        self.commit_file("learned", commit)
    }

    /// `graph-<digest>.json`.
    ///
    /// # Errors
    /// Digest vide, trop long ou avec des caractères hors `[A-Za-z0-9_-]`.
    pub fn graph_path(&self, digest: &str) -> Result<PathBuf, SelectError> {
        if !is_safe_digest(digest) {
            return Err(invalid(
                "store",
                format!("digest de graphe refusé : {digest:?}"),
            ));
        }
        Ok(self.root.join(format!("graph-{digest}.json")))
    }

    /// Les deux fichiers d'une base (inventaire et arêtes apprises) existent-ils ?
    #[must_use]
    pub fn has_base_and_learned(&self, commit: &str) -> bool {
        let exists = |path: Result<PathBuf, SelectError>| path.is_ok_and(|p| p.is_file());
        exists(self.base_path(commit)) && exists(self.learned_path(commit))
    }

    /// Écrit l'inventaire de la base (atomique).
    ///
    /// # Errors
    /// Inventaire invalide ou entrée/sortie.
    pub fn save_base(&self, inv: &BaseInventory) -> Result<PathBuf, SelectError> {
        let path = self.base_path(&inv.commit)?;
        std::fs::create_dir_all(&self.root).map_err(|e| base::io_error(&self.root, &e))?;
        base::save(&path, inv)?;
        Ok(path)
    }

    /// Lit l'inventaire de `commit` et vérifie qu'il décrit bien ce commit.
    ///
    /// # Errors
    /// Fichier absent ou illisible, ou commit interne différent du nom.
    pub fn load_base(&self, commit: &str) -> Result<BaseInventory, SelectError> {
        let inv = base::load(&self.base_path(commit)?)?;
        if inv.commit != commit {
            return Err(invalid(
                "base inventory",
                format!("le fichier du commit {commit} décrit {}", inv.commit),
            ));
        }
        Ok(inv)
    }

    /// Lit le graphe de digest `digest` : fichier absent, illisible ou digest interne
    /// différent du nom = erreur (S3).
    ///
    /// # Errors
    /// Voir ci-dessus.
    pub fn load_graph(&self, digest: &str) -> Result<GraphData, SelectError> {
        let path = self.graph_path(digest)?;
        let data = graph::from_json(&read_bounded(&path)?)?;
        if data.digest != digest {
            return Err(invalid(
                "graph",
                format!(
                    "le fichier {} porte le digest interne {:?}",
                    path.display(),
                    data.digest
                ),
            ));
        }
        Ok(data)
    }

    /// Écrit les arêtes apprises de `commit` (atomique).
    ///
    /// # Errors
    /// Commit mal formé, schéma faux ou entrée/sortie.
    pub fn save_learned(
        &self,
        commit: &str,
        learned: &LearnedEdges,
    ) -> Result<PathBuf, SelectError> {
        let path = self.learned_path(commit)?;
        learn::write(&path, learned)?;
        Ok(path)
    }

    /// Lit les arêtes apprises de `commit` ; elles doivent être estampillées pour ce commit
    /// et pour le graphe `graph_digest` (des arêtes calculées sur un autre graphe pourraient
    /// en manquer : on refuse, S3).
    ///
    /// # Errors
    /// Fichier illisible ou estampille différente.
    pub fn load_learned(
        &self,
        commit: &str,
        graph_digest: &str,
    ) -> Result<LearnedEdges, SelectError> {
        let learned = learn::read(&self.learned_path(commit)?)?;
        if learned.base_commit != commit {
            return Err(invalid(
                "learned edges",
                format!(
                    "estampillées pour {:?}, pas pour {commit}",
                    learned.base_commit
                ),
            ));
        }
        if learned.graph_digest != graph_digest {
            return Err(invalid(
                "learned edges",
                format!(
                    "calculées sur le graphe {:?}, la base utilise {graph_digest:?}",
                    learned.graph_digest
                ),
            ));
        }
        Ok(learned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(
        pairs: &'a [(&'a str, &'a str)],
    ) -> impl Fn(&str) -> Option<std::ffi::OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| std::ffi::OsString::from(value))
        }
    }

    #[test]
    fn root_follows_the_documented_precedence() {
        let all = [
            ("KIOKU_SELECT_STORE", "/env/store"),
            ("XDG_CACHE_HOME", "/xdg"),
            ("HOME", "/home/u"),
        ];
        let root = |explicit: Option<&Path>, pairs: &[(&str, &str)]| {
            Store::resolve_with(explicit, &env_of(pairs)).map(|s| s.root().to_path_buf())
        };
        assert_eq!(
            root(Some(Path::new("/flag")), &all).unwrap(),
            Path::new("/flag")
        );
        assert_eq!(root(None, &all).unwrap(), Path::new("/env/store"));
        assert_eq!(
            root(None, &all[1..]).unwrap(),
            Path::new("/xdg/kioku/select")
        );
        assert_eq!(
            root(None, &all[2..]).unwrap(),
            Path::new("/home/u/.cache/kioku/select")
        );
        // Variables vides ou XDG relatif : ignorés.
        let weak = [
            ("KIOKU_SELECT_STORE", ""),
            ("XDG_CACHE_HOME", "rel"),
            ("HOME", "/h"),
        ];
        assert_eq!(
            root(None, &weak).unwrap(),
            Path::new("/h/.cache/kioku/select")
        );
        assert!(root(None, &[]).is_err());
    }

    #[test]
    fn names_are_validated_before_becoming_paths() {
        let store = Store::new("/s");
        let commit = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(
            store.base_path(commit).unwrap(),
            Path::new("/s").join(format!("base-{commit}.json"))
        );
        assert_eq!(
            store.learned_path(commit).unwrap(),
            Path::new("/s").join(format!("learned-{commit}.json"))
        );
        for bad in [
            "",
            "../x",
            "ABCDEF0123456789abcdef0123456789abcdef01",
            "abc",
        ] {
            assert!(store.base_path(bad).is_err(), "{bad:?}");
            assert!(store.learned_path(bad).is_err(), "{bad:?}");
        }
        assert!(store.graph_path("abc-DEF_1").is_ok());
        for bad in ["", "../x", "a/b", "a.b", &"x".repeat(129)] {
            assert!(store.graph_path(bad).is_err(), "{bad:?}");
        }
    }
}
