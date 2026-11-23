# Corpus de fuzzing des parseurs (lot F3a)

Deux familles, deux dossiers, deux fichiers d'attentes. Aucun fichier de production n'est
touché : les tests vivent dans `src/select/learn.rs` (`mod tests`) et
`tests/select_fuzz_parsers.rs`.

Inventaire mesuré (compté sur disque, pas annoncé) : 39 fichiers `lcov/`, 25 fichiers
`nextest/` (1 capture réelle + 1 metadata dérivé + 1 metadata de cas limites + 22 cas
limites), 2 fichiers d'attentes et ce README, soit 67 fichiers.

## Normalisation des chemins

Les captures réelles viennent d'une station précise. Trois préfixes sont réécrits, et rien
d'autre :

| avant | après |
| --- | --- |
| `/home/node/work/kioku-optA` | `/w/repo` |
| `/home/node/.cargo` | `/w/cargo` |
| `/home/node/.rustup` | `/w/rustup` |

Aucun autre octet n'est modifié : compteurs, noms de tests, identifiants de binaire et
empreintes de binaires restent ceux de la capture. Les SHA-256 des attentes portent sur le
fichier **après** normalisation. Aucun secret (jeton, clé, en-tête, courriel, chemin client)
n'a été trouvé dans les captures ; la normalisation sert à ne pas figer l'identité d'une
station, pas à masquer un secret.

## `lcov/` — `parse_lcov` (`src/select/learn.rs`)

Attentes : `expectations-lcov.json` (verdict `accept`/`reject`, fichiers retenus, SHA-256,
taille en octets). Racine de dépôt simulée : `/w/repo`, cible `/w/repo/target`.

Captures réelles (LLVM 23.1.1, `llvm-cov export -format=lcov`, Rust 1.99.1) :

| fichier | origine |
| --- | --- |
| `real-llvm23-kioku-6records.lcov` | 6 records extraits de `export.lcov` d'un clone kioku (`w1-confirm-3254289711ca93c1/ci2-n1-S/agent-1/clone`) : 4 fichiers du dépôt + 2 du registre Cargo |
| `real-llvm23-constructs.lcov` | `lcov-mini-repro-1794826425-constructs/e2.lcov` (macros, closures, `?`, boucles) |
| `real-llvm23-monomorph.lcov` | `lcov-mini-repro-1794826273-lfmismatch/e2.lcov` (génériques monomorphisés : `FNF` < lignes `FN`) |
| `real-llvm23-uncovered.lcov` | `lcov-mini-repro-1794825975/export.lcov` (record entièrement non couvert, `LH:0`) |

Cas limites par grammaire (`edge-*.lcov`), écrits à la main : `TN` hors/dans un record,
`DA` avec checksum, `DA` dupliqué, ligne 0, non entier, hors record, `LF`/`LH` absents ou
incohérents, `LH>0` sans `DA` positif, `FNF`/`FNH` hors bornes, `FN` ligne 0 ou nom vide,
`FN` sans `FNDA`, `BRDA` (4 champs, `-`, comptage `BRF`/`BRH`), clé inconnue, ligne vide,
CRLF, absence de saut de ligne final, export tronqué, record vide, chemins hors dépôt /
sous `target/` / avec `..`, multi-records mixte.

## `nextest/` — `parse_nextest_list` (`src/select/base.rs` et `src/select/graph/depinfo.rs`)

Attentes : `expectations-nextest.json` (verdict, identifiants, tests ignorés, SHA-256,
taille en octets, pour **les deux** lecteurs).

| fichier | origine |
| --- | --- |
| `real-kioku-67suites.json` | capture réelle `cargo nextest list --message-format json` sur kioku (`lot9c-diag-94u5k06e/repo/scratch/optA/m1/list.json`) : 67 binaires, 634 cas, 59 ignorés |
| `derived-metadata-67suites.json` | `cargo metadata` **dérivé** de cette capture (nom de cible → `src/lib.rs`, `src/main.rs`, `tests/<nom>.rs`), pas une capture : il sert uniquement à relier les 67 binaires |
| `edge-metadata.json` | `cargo metadata` minimal (lib `kioku`, bin `kioku`, test `cli`) pour les cas limites |
| `edge-*.json` | 22 cas limites : racine non-objet, `null`, JSON tronqué, `rust-suites` absent/vide, clé ≠ `binary-id`, `status` absent ou `skipped`, cible inconnue, `kind` inconnu, `test-count` faux ou juste, `ignored` absent ou non booléen, `testcases` absent ou vide, `binary-path` absent, cible dupliquée, nom unicode ou vide, champs supplémentaires |

Les deux lecteurs n'ont pas le même contrat, et le corpus le fige : `edge-testcases-missing`,
`edge-key-mismatch`, `edge-status-skipped`, `edge-test-count-mismatch` et
`edge-unknown-target` sont **rejetés** par `base` et **acceptés** par `depinfo` ;
`edge-binary-path-missing` est l'inverse.

## Mutations et génération

Déterministes, graine fixe (`RngSeed::Fixed`), `failure_persistence: None` :

- LCOV : 96 cas, graine `0x6b69_6f6b_75`, 1 à 5 opérations sur 4 exports de base. L'offset
  est tiré sur `any::<usize>()` puis ramené modulo la longueur : toutes les régions du
  fichier sont atteignables, pas seulement les 4096 premiers octets.
- nextest : 64 cas, graine `0x6e65_7874`, 1 à 4 opérations sur la capture réelle.
- JSON généré : 64 cas, même graine, documents construits par grammaire (0 à 3 binaires,
  0 à 3 cas, ignoré ou non) puis corrompus 0 à 2 fois. La branche « accepté » est
  réellement atteinte : `generated_document_without_corruption_is_accepted` vérifie qu'un
  document non corrompu est accepté par les deux lecteurs.

Opérations : troncature, insertion, suppression, octet remplacé, ligne dupliquée. Invariants
vérifiés, jamais un modèle Python des parseurs :

- aucune panique ;
- un export LCOV accepté ne cite que des fichiers dont une ligne `DA` est positive, relatifs
  au dépôt et hors `target/` ;
- un inventaire nextest accepté n'a ni identifiant de binaire ni nom de test en double, et
  ne cite que des chemins relatifs au dépôt (même règle que la production, lettre de
  lecteur refusée).

## Régénération

Les captures réelles ne sont pas régénérables hors d'un run instrumenté ; elles sont figées
ici avec leur SHA-256. Les cas limites sont du texte, modifiables à la main : mettre à jour
le fichier **et** son entrée d'attentes (verdict, fichiers, `sha256`, `bytes`) dans le même
commit, sinon `lcov_fuzz_corpus_matches_expectations` / `nextest_corpus_matches_expectations`
échoue. Les deux tests comparent aussi le nombre de fichiers du corpus au nombre d'attentes.
