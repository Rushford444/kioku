# Référence de configuration

`kioku.toml`, facultatif, se trouve à la racine du dépôt Git. Il choisit un
mode pour un vecteur exact d’arguments, pas pour une expression shell :

```toml
# Chaque élément est un argument, exécutable compris.
[[commands]]
argv = ["cargo", "test", "--locked"]
mode = "bytes" # « bytes » ou « ast »

[[commands]]
argv = ["cargo", "clippy"]
mode = "ast"
```

L’ordre et les arguments doivent correspondre exactement. Champs inconnus,
modes invalides et commandes correspondantes dupliquées sont des erreurs.
`--mode` permet de choisir le mode en ligne de commande. La réutilisation
tracée utilise toujours les octets, même avec AST ; l’analyse AST de `explain`
est hypothétique. La configuration n’active jamais la réutilisation à elle
seule. `--inputs` ne limite pas le manifeste tracé et `--env` ne limite pas
l’identité de l’environnement tracé : l’environnement initial complet est
inclus sous forme d’empreinte. Voir le [guide](usage.md) et les
[hypothèses de traçage](../../README.fr.md#hypothèses-de-réutilisation-tracée).

Variables d’environnement :

- `KIOKU_DIR` : emplacement du cache local, à protéger comme donnée sensible.
- `KIOKU_DISABLE=1` : exécuter sans réutilisation.
- `KIOKU_STRACE` : chemin absolu du traceur Linux approuvé, aarch64 ou x86_64.

Aucune configuration ne rend un cache sûr pour les commandes dont les sorties
ou effets de bord sont nécessaires. Utilisez `--require-output` ou exécutez-les directement.
