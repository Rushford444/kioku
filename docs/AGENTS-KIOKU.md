# Kioku pour les agents : sélection de tests

## Avant de remplacer une commande

**Ne remplacez pas aveuglément `cargo nextest run` par `kioku test`.** La
sélection réelle cible Cargo/nextest ; Vitest n’a qu’un repli, et les codes de
retour sont différents.
Elle ne transmet pas arbitrairement les options nextest. Pour la CI, gardez la
commande native tant que le consommateur n’interprète pas explicitement les
statuts Kioku. Un vert partiel n’est jamais un vert de suite complète.

## Rust : préparer et utiliser la sélection

Depuis la racine du dépôt, avec Git, Cargo et nextest disponibles :

```sh
# À un commit de base propre et vert, dans un magasin local protégé :
kioku select base --store "$HOME/.cache/kioku/select"
# Utiliser une couverture par test réellement mesurée sur ce même commit :
kioku select learn --store "$HOME/.cache/kioku/select" --coverage-json /chemin/vers/lines.json
# Sur la branche de travail :
kioku test --store "$HOME/.cache/kioku/select" --base main --journal target/kioku-select/journal.json
```

Le fichier de couverture doit correspondre à la base ; ne pas inventer ses
arêtes. `kioku select learn --run` existe également mais ses outils et sa
couverture réelle ne sont pas établis par ce document. Les features, filtres
et options de la base doivent correspondre au besoin ; `--features` et `-E`
sont des options explicites de `kioku test`, pas une interface nextest complète.
Un filtre utilisateur rend le résultat partiel, même si tous les tests retenus
passent. `kioku select plan` ne lance aucun test.

Le magasin de sélection est distinct de `KIOKU_DIR` : `--store`, puis
`KIOKU_SELECT_STORE`, puis `$XDG_CACHE_HOME/kioku/select`, puis
`~/.cache/kioku/select`. Il faut une base et ses arêtes apprises pour un
ancêtre admissible de la référence. Données absentes, douteuses ou inconnues :
élargir à la suite entière ; une erreur ne justifie jamais moins de tests. Sur
`main` ou en contexte de merge request, la sélection ne réduit pas la suite.
Consulter aussi les diagnostics : si l’exécution complète elle-même ne peut pas
être lancée, le résultat est incomplet, pas une preuve verte. Protéger magasin
et journal : ils peuvent contenir chemins et détails sensibles.

## JavaScript / Vitest

La sélection `kioku test` ne pilote pas Vitest. `kioku test --runner vitest`
existe, mais ce n’est pas une sélection : il lance toute la suite locale et
renvoie 3 même quand elle passe, parce que la préparation de base n’est pas
disponible. Garder la commande native du
projet, par exemple `npm exec -- vitest run` (ou le gestionnaire déjà utilisé
par le projet). Ne pas promettre une sélection JS ni transposer les codes
Kioku à Vitest. `kioku run -- npm exec -- vitest run` est un autre mode : par
défaut il exécute la commande entière ; une réutilisation tracée est opt-in,
soumise aux hypothèses strictes du README et ne restitue ni sorties ni fichiers.

## Statuts et codes de `kioku test`

| Code | Statut | Interprétation |
| --- | --- | --- |
| 0 | `partial-green` | Sélection exécutée avec succès ; pas une preuve complète. |
| 1 | `red` | Échec constaté. |
| 3 | `incomplete` ou erreur | Pas de preuve verte ; vérifier les diagnostics et lancer la suite si possible. |
| 5 | `full-green` | Suite complète verte selon le journal ; ce code non nul casse un appelant shell ordinaire. |

Ce sont les codes du rapport de sélection, **pas** une propagation générale du
code du processus nextest. Les erreurs d’arguments CLI peuvent aussi avoir leurs
propres codes. `planned` vaut 0 pour `select plan`, sans exécution. Ne convertissez
pas 0 en succès complet. Ne masquez pas un code inattendu.

## Capturer le résultat sans masquer le code

Sous `set -e`, capturez le code dans un sous-shell où il est désactivé.
Le code 5 est non nul : un shell ordinaire ne le traite pas comme un succès.
Cet exemple rend **le code original**, y compris 5, à son appelant :

```sh
(
  set +e
  kioku --json test --store "$HOME/.cache/kioku/select" --base main \
    --journal target/kioku-select/journal.json \
    >target/kioku-select.stdout 2>target/kioku-select.stderr
  rc=$?
  set -e
  case "$rc" in
    0) echo 'partial-green: sélection seulement' >&2 ;;
    1) echo 'red: échec constaté' >&2 ;;
    3) echo 'incomplete ou erreur: aucune preuve verte' >&2 ;;
    5) echo 'full-green: suite complète selon le journal' >&2 ;;
    *) echo "résultat inattendu: $rc" >&2 ;;
  esac
  exit "$rc"
)
```

Préparez les répertoires et permissions avant l'appel. `--base main` désigne
la référence Git nommée `main`, qui doit exister localement ; ce n'est pas une
comparaison automatique avec tout HEAD. Sur main et en merge request, exigez
la suite native complète. Si la référence, les données ou le contexte sont
inconnus, utilisez la suite entière, pas une sélection supposée. Le résumé
`--json test` et le journal détaillé sont distincts : stdout et stderr peuvent
contenir les flux natifs de nextest ; stderr peut aussi contenir des événements
Kioku. Ne traitez aucun de ces fichiers comme un flux JSON pur ni un `cargo`
redirigé comme s'il était du JSON Kioku. Protégez les sorties et le journal.

## Lire le résumé et les raisons

La sortie humaine affiche les tests sautés et leurs raisons, puis
les comptages et le chemin du journal. Le journal JSON est la source complète :
chaque entrée `decision: "skip"` possède `test.binary_id`, `test.name` et
`reasons`. Les résultats `filtered-out` et `ignored` sont distincts des sauts
de sélection : les examiner dans `outcomes` et consulter `user_filter`.
Le chemin par défaut est `target/kioku-select/journal.json` sous le dépôt.
Si le journal ne peut être écrit, le code de sortie d’un résultat vert devient 3 ; le statut affiché ne suffit donc pas : vérifier aussi le code et les diagnostics.

```sh
# Inspecter localement sans publier les chemins ni le contenu du journal :
jq -r '.entries[] | select(.decision == "skip") | [.test.binary_id, .test.name, (.reasons | tojson)] | @tsv' target/kioku-select/journal.json
```

Le journal et le résumé `kioku --json test` portent actuellement tous deux
`kioku/select/v1` mais n’ont pas la même forme : le résumé compte les résultats,
le journal contient les entrées détaillées. Ne pas les confondre et ne pas
supposer que le résumé JSON contient toutes les raisons. Les flux du processus
nextest peuvent coexister avec la sortie ; ne pas supposer un flux JSON pur sans
vérification. Voir `kioku test --help`, `README.md` et `SECURITY.md`.
