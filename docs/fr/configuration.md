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
seule. Le manifeste tracé liste les fichiers observés par l’enfant et
l’environnement initial complet est inclus sous forme d’empreinte. Aucune option
ne les restreint. Voir le [guide](usage.md) et les
[hypothèses de traçage](../../README.fr.md#hypothèses-de-réutilisation-tracée).

Le diagnostic AST empreint chaque fichier visible par Git pour `explain`, et
avant puis après `run` tant que le cache est actif. Il lit les fichiers
réguliers de cette liste, y compris les fichiers non suivis, mais pas les
fichiers ignorés ; un gros arbre coûte des entrées/sorties, et un fichier
régulier sous verrou d’écriture (`lease`) ou sur un système lent peut bloquer le
diagnostic. Ignorez les fichiers coûteux ou bloquants s’ils n’influencent pas la
commande. `KIOKU_DISABLE=1` saute ce diagnostic pour `run` ; `explain` le
calcule toujours.

Variables d’environnement :

- `KIOKU_DIR` : emplacement du cache local, à protéger comme donnée sensible.
  Un store dans l’arbre de travail coupe le cache : kioku exécute la commande,
  n’apprend rien, ne réutilise rien et n’écrit rien à cet endroit. Un store
  laissé là par une version antérieure n’est pas supprimé par `kioku clean`.
- `KIOKU_DISABLE=1` : exécuter sans réutilisation.
- `KIOKU_STRACE` : chemin absolu du traceur Linux aarch64 ou x86_64 approuvé.
- `NO_COLOR` : désactiver les couleurs si applicables.

Aucune configuration ne rend un cache sûr pour les commandes dont les sorties
ou effets de bord sont nécessaires. Utilisez `--require-output` ou exécutez-les directement.
