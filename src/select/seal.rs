//! Scellé d'intégrité des fichiers du magasin de sélection (correctif A4).
//!
//! Les estampilles (`base_commit`, `graph_digest`, nom du fichier) disent À QUOI un fichier se
//! rapporte, pas CE QU'IL CONTIENT : un fichier dont on a retiré des arêtes ou des binaires
//! restait un JSON valide, bien estampillé, et la sélection sautait alors des tests à tort.
//! Chaque fichier (`base-*`, `graph-*`, `learned-*`) porte donc :
//!
//! - des comptes (`edge_count`, `binary_count`…), recalculés à la lecture ;
//! - `content_digest` : `sha256:<hex>` d'une sérialisation CANONIQUE (clés triées, sans
//!   espace) de tout le reste du fichier, comptes compris.
//!
//! La lecture re-sérialise la valeur relue (pas les octets du fichier) : l'ordre des clés, la
//! mise en forme et les champs inconnus n'entrent pas dans le calcul. Compte absent, compte
//! différent, digest absent ou différent → [`SelectError::Invalid`], donc « tout » (S3).
//!
//! Limite assumée : le scellé n'est PAS une défense contre un acteur qui contrôle le
//! stockage (il peut recalculer le digest, aucun secret n'intervient). Il protège contre la
//! troncature, l'altération et l'édition partielle (un champ modifié sans les autres).

use serde::Serialize;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::SelectError;

/// Nom du champ qui porte le digest du contenu.
pub(crate) const DIGEST_FIELD: &str = "content_digest";

/// Comptes sérialisés à côté du contenu : `(nom du champ, valeur)`.
pub(crate) type Counts<'a> = &'a [(&'static str, usize)];

fn invalid(what: &str, message: impl Into<String>) -> SelectError {
    SelectError::Invalid {
        what: what.to_owned(),
        message: message.into(),
    }
}

/// Alimente le hash avec la forme canonique de `value` : objets à clés triées, tableaux dans
/// leur ordre, scalaires au format compact de `serde_json`.
fn feed(hasher: &mut Sha256, value: &Value) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            hasher.update(b"{");
            for key in keys {
                hasher.update(Value::String(key.clone()).to_string());
                hasher.update(b":");
                if let Some(inner) = map.get(key) {
                    feed(hasher, inner);
                }
                hasher.update(b",");
            }
            hasher.update(b"}");
        }
        Value::Array(items) => {
            hasher.update(b"[");
            for item in items {
                feed(hasher, item);
                hasher.update(b",");
            }
            hasher.update(b"]");
        }
        scalar => hasher.update(scalar.to_string()),
    }
}

fn digest_of(body: &Map<String, Value>) -> String {
    let mut hasher = Sha256::new();
    for (key, value) in body {
        if key != DIGEST_FIELD {
            hasher.update(Value::String(key.clone()).to_string());
            hasher.update(b"=");
            feed(&mut hasher, value);
            hasher.update(b";");
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// Objet JSON de `value` plus les comptes (sans le digest).
fn body<T: Serialize>(
    what: &str,
    value: &T,
    counts: Counts<'_>,
) -> Result<Map<String, Value>, SelectError> {
    let Value::Object(mut map) =
        serde_json::to_value(value).map_err(|e| invalid(what, e.to_string()))?
    else {
        return Err(invalid(what, "scellé : un objet JSON est attendu"));
    };
    for (name, count) in counts {
        if map
            .insert((*name).to_owned(), Value::from(*count))
            .is_some()
        {
            return Err(invalid(what, format!("scellé : champ réservé `{name}`")));
        }
    }
    if map.contains_key(DIGEST_FIELD) {
        return Err(invalid(
            what,
            format!("scellé : champ réservé `{DIGEST_FIELD}`"),
        ));
    }
    Ok(map)
}

/// Document à écrire : `value` + comptes + `content_digest`.
///
/// # Errors
/// [`SelectError::Invalid`] si `value` ne se sérialise pas en objet ou utilise un champ réservé.
pub(crate) fn seal<T: Serialize>(
    what: &str,
    value: &T,
    counts: Counts<'_>,
) -> Result<Value, SelectError> {
    let mut map = body(what, value, counts)?;
    let digest = digest_of(&map);
    map.insert(DIGEST_FIELD.to_owned(), Value::String(digest));
    Ok(Value::Object(map))
}

/// Vérifie que `document` (relu tel quel) est le scellé de `value` (la valeur qu'on en a tirée).
///
/// `message` est la phrase d'erreur (« arêtes apprises incohérentes : … ») ; le détail du
/// premier désaccord y est ajouté entre parenthèses.
///
/// # Errors
/// [`SelectError::Invalid`] : compte ou digest absent, ou différent de celui recalculé.
pub(crate) fn verify<T: Serialize>(
    what: &str,
    message: &str,
    document: &Value,
    value: &T,
    counts: Counts<'_>,
) -> Result<(), SelectError> {
    let refuse = |detail: String| invalid(what, format!("{message} ({detail})"));
    let Some(found) = document.as_object() else {
        return Err(refuse("document : objet JSON attendu".to_owned()));
    };
    let expected = body(what, value, counts)?;
    for (name, count) in counts {
        match found.get(*name).and_then(Value::as_u64) {
            None => return Err(refuse(format!("`{name}` absent ou illisible"))),
            Some(claimed) if usize::try_from(claimed) != Ok(*count) => {
                return Err(refuse(format!(
                    "`{name}` : {claimed} dans le fichier, {count} relu"
                )));
            }
            Some(_) => {}
        }
    }
    let Some(claimed) = found.get(DIGEST_FIELD).and_then(Value::as_str) else {
        return Err(refuse(format!("`{DIGEST_FIELD}` absent ou illisible")));
    };
    if claimed != digest_of(&expected) {
        return Err(refuse(format!(
            "`{DIGEST_FIELD}` différent du contenu relu"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[derive(Serialize)]
    struct Doc {
        schema: String,
        items: Vec<String>,
    }

    fn doc(items: &[&str]) -> Doc {
        Doc {
            schema: "s/v2".to_owned(),
            items: items.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    const MESSAGE: &str = "incohérent";

    fn check(document: &Value, value: &Doc, n: usize) -> Result<(), SelectError> {
        verify("doc", MESSAGE, document, value, &[("item_count", n)])
    }

    #[test]
    fn round_trip_is_accepted_and_digest_is_prefixed() {
        let value = doc(&["a", "b"]);
        let sealed = seal("doc", &value, &[("item_count", 2)]).unwrap();
        check(&sealed, &value, 2).unwrap();
        let digest = sealed[DIGEST_FIELD].as_str().unwrap();
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);
        assert_eq!(sealed["item_count"], 2);
    }

    #[test]
    fn digest_ignores_formatting_and_key_order() {
        let value = doc(&["a", "b"]);
        let sealed = seal("doc", &value, &[("item_count", 2)]).unwrap();
        let text = serde_json::to_string_pretty(&sealed).unwrap();
        let reread: Value = serde_json::from_str(&text).unwrap();
        check(&reread, &value, 2).unwrap();
    }

    #[test]
    fn every_alteration_is_refused() {
        let value = doc(&["a", "b"]);
        let sealed = seal("doc", &value, &[("item_count", 2)]).unwrap();
        // Contenu relu différent, scellé d'origine gardé (élément retiré, élément changé).
        assert!(check(&sealed, &doc(&["a"]), 1).is_err());
        assert!(check(&sealed, &doc(&["a", "c"]), 2).is_err());
        assert!(
            check(&sealed, &doc(&["b", "a"]), 2).is_err(),
            "l'ordre compte"
        );
        // Compte changé.
        let mut bad = sealed.clone();
        bad["item_count"] = json!(3);
        assert!(check(&bad, &value, 2).is_err());
        // Digest changé, absent, d'un autre type.
        let mut bad = sealed.clone();
        bad[DIGEST_FIELD] = json!(format!("sha256:{}", "0".repeat(64)));
        assert!(check(&bad, &value, 2).is_err());
        for removed in [DIGEST_FIELD, "item_count"] {
            let mut bad = sealed.clone();
            bad.as_object_mut().unwrap().remove(removed);
            let error = check(&bad, &value, 2).unwrap_err().to_string();
            assert!(
                error.contains(MESSAGE) && error.contains(removed),
                "{error}"
            );
        }
        let mut bad = sealed;
        bad[DIGEST_FIELD] = json!(7);
        assert!(check(&bad, &value, 2).is_err());
        assert!(check(&json!([1]), &value, 2).is_err());
    }

    #[test]
    fn reserved_fields_are_refused() {
        let value = doc(&[]);
        assert!(seal("doc", &value, &[("schema", 1)]).is_err());
        assert!(seal("doc", &json!({ DIGEST_FIELD: "x" }), &[]).is_err());
        assert!(seal("doc", &json!([1]), &[]).is_err());
    }

    #[test]
    fn canonical_form_is_unambiguous() {
        // Deux valeurs distinctes ne partagent jamais le même digest (séparateurs explicites).
        let one = seal("d", &json!({"a": ["x", "y"]}), &[]).unwrap();
        let two = seal("d", &json!({"a": ["x,y"]}), &[]).unwrap();
        let three = seal("d", &json!({"a": ["x"], "b": ["y"]}), &[]).unwrap();
        let digests = [&one, &two, &three].map(|v| v[DIGEST_FIELD].as_str().unwrap().to_owned());
        assert!(digests[0] != digests[1] && digests[0] != digests[2] && digests[1] != digests[2]);
    }
}
