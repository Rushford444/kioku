# Guide d’utilisation

Depuis un dépôt Git, le comportement par défaut exécute toujours la commande.
Seuls les codes de sortie réussis peuvent être réutilisés après activation
explicite. Un résultat réutilisé ne restitue ni sortie ni fichier.

```sh
kioku run -- cargo test
kioku verify -- cargo test
kioku explain -- cargo test
kioku status
kioku clean --older-than 7d
```

`verify` exécute toujours et compare au choix du cache. `explain` explique ce
choix sans lancer la commande. `clean --all` supprime toutes les entrées ;
`quarantine reset -- COMMANDE` efface la quarantaine de la commande exacte et
recommence l’apprentissage. `run --require-output` force l’exécution. `--quiet`
réduit les messages humains.

N’activez le traçage qu’après acceptation de toutes les
[hypothèses](../../README.fr.md#hypothèses-de-réutilisation-tracée) :

```sh
kioku run --trust-trace --verify-rate 1 -- /chemin/absolu/vers/verification-deterministe
```

`--verify-rate` va de 0 à 1 ; 1 vérifie chaque résultat réutilisable. Le
traçage exige Linux aarch64 ou x86_64, le strace Debian épinglé pour cette
architecture et un exécutable ELF statique little-endian sans interpréteur. Les
observations non prises en charge entraînent l’exécution.
Le traçage inclut toujours l’environnement initial complet ; aucune option
supplémentaire n’est nécessaire. Consultez la [configuration](configuration.md).
Les commandes dynamiques ordinaires ne bénéficient pas actuellement du cache tracé.

## CI et agents

Aucune interaction n’est requise. En CI, exécutez la commande directement ou
utilisez le mode par défaut ; ne comptez pas sur un cache pour créer les artefacts :

```sh
kioku run -- cargo test --locked
```

À placer dans `AGENTS.md` ou `CLAUDE.md` :

```text
Utiliser kioku run -- COMMANDE si le code de sortie suffit.
Le mode par défaut exécute. Ne pas attendre de sortie ni de fichiers d’un cache.
Activer le traçage seulement après lecture et acceptation de ses hypothèses.
```

## JSON et codes de sortie

```sh
kioku --json status
kioku --json run -- cargo test
```

Le schéma kioku est `kioku/v1`. Pour `run` et `verify`, son JSON va sur stderr ;
stdout et stderr de la commande restent hérités. Rediriger stderr ne sépare donc
pas le JSON du stderr de la commande. Les autres commandes écrivent le JSON sur
stdout ; les erreurs vont sur stderr. Consultez `cacheable` et `denial_reason`,
pas seulement `trace_state`. Kioku transmet le code de sortie de l’enfant, y
compris 1, 2, 3, 126 et 127 ; une terminaison sans code vaut 1. Kioku utilise
aussi 2 pour l’usage, 3 pour une erreur interne, 126 si le lancement échoue et
127 si la commande est introuvable. Ces valeurs se chevauchent : le code seul
ne révèle pas son origine. Les arguments doivent être en UTF-8.
