//! Lot L4a : plan d'exécution (sélection, cibles nextest, d1 S5).
//!
//! Entrée : la portée de la classification (§ 4), l'inventaire de la base, le graphe
//! (L2 + arêtes apprises L3) et, au plus, une proposition d1. Sortie : un [`Plan`] avec une
//! entrée par test de l'inventaire, les cibles Cargo à compiler et le filtre nextest.
//!
//! Règle d'or : jamais d'erreur, jamais de panique ; un doute donne « lancé ».

use std::collections::{BTreeMap, BTreeSet, VecDeque};

mod filterset;

pub use filterset::FilterSet;

use super::classify::reach;
use super::{
    BaseBinary, BaseInventory, BaseStatus, BinaryId, BinaryKind, D1Proposal, Decision, GraphData,
    JournalEntry, NextestTarget, PartialScope, Plan, Reason, RelPath, Scope, TestId,
};

/// Taille maximale du filtre `-E` (un seul argument : la limite Linux est 128 Kio).
const MAX_FILTER_BYTES: usize = 100_000;

/// Construit le plan : une entrée par test de l'inventaire de la base. Jamais d'erreur :
/// un doute donne « lancé ».
///
/// d1 (S5) n'est appliqué qu'à une portée partielle (la portée « tout » lance déjà tout) ;
/// ses ajouts sont des lancements, ses sauts sont ignorés et journalisés. Un test ajouté
/// par d1 dont le binaire est inconnu de la base est ignoré : un binaire inconnu est traité
/// par la classification (`NewBinary`), pas par d1.
#[must_use]
pub fn plan(
    scope: &Scope,
    base: &BaseInventory,
    graph: &GraphData,
    d1: Option<&D1Proposal>,
) -> Plan {
    let mut states = inventory_states(base);
    let mut modified_files: Vec<RelPath> = Vec::new();
    let mut whole_absent: Vec<BinaryId> = Vec::new();
    let mut owners: BTreeSet<&str> = BTreeSet::new();
    let mut targets_all = false;

    match scope {
        Scope::Everything { reasons } => {
            targets_all = true;
            for state in states.values_mut() {
                state.run(reasons.iter().cloned());
            }
        }
        Scope::Partial(partial) => {
            modified_files = partial.modified.iter().cloned().collect();
            owners = owner_binaries(base, graph, &partial.modified);
            select_partial(&mut states, partial, graph);
            if let Some(proposal) = d1 {
                apply_d1(&mut states, proposal);
            }
            whole_absent = partial
                .whole_binaries
                .keys()
                .filter(|id| !base.binaries.iter().any(|b| &b.binary_id == *id))
                .cloned()
                .collect();
        }
    }

    let kinds: BTreeMap<&str, &BaseBinary> = base
        .binaries
        .iter()
        .map(|b| (b.binary_id.as_str(), b))
        .collect();
    if !targets_all {
        force_unexpressible(&mut states);
    }
    let mut targets = if targets_all {
        None
    } else {
        compute_targets(&states, &kinds, &whole_absent, &owners)
    };
    let mut filter = if targets_all {
        None
    } else {
        build_filter(
            &states,
            compiled_binaries(&kinds, targets.as_deref()).as_ref(),
        )
    };
    if filter.as_ref().is_some_and(|f| f.len() > MAX_FILTER_BYTES) {
        // Filtre trop long pour un argument : on lance les binaires compilés en entier.
        widen_targeted_binaries(
            &mut states,
            compiled_binaries(&kinds, targets.as_deref()).as_ref(),
        );
        targets = compute_targets(&states, &kinds, &whole_absent, &owners);
        filter = build_filter(
            &states,
            compiled_binaries(&kinds, targets.as_deref()).as_ref(),
        );
    }

    Plan {
        scope: scope.clone(),
        base_commit: base.commit.clone(),
        graph_digest: graph.digest.clone(),
        modified_files,
        entries: states
            .into_iter()
            .map(|(test, state)| JournalEntry {
                test,
                decision: state.decision(),
                reasons: state.finish(&graph.digest),
            })
            .collect(),
        targets,
        filter,
    }
}

/// Arguments à ajouter après `cargo nextest run --locked --no-fail-fast` : les cibles
/// (`--lib`, `--bin X`, `--test X`), puis `-E <expr>` avec `expr = (user) & (filter)`, ou
/// l'un des deux seul, ou rien. Un seul argument par expression (pas de shell).
///
/// Un plan partiel sans aucune cible (rien à lancer) donne `-E none()` : nextest ne lance
/// rien, il ne retombe jamais sur la suite complète par oubli. L'appelant ne doit de toute
/// façon pas lancer nextest quand aucun test n'est `Run`.
#[must_use]
pub fn nextest_args(plan: &Plan, user_filter: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(targets) = &plan.targets {
        if targets.is_empty() {
            return vec!["-E".to_owned(), "none()".to_owned()];
        }
        for target in targets {
            match target {
                NextestTarget::Lib => args.push("--lib".to_owned()),
                NextestTarget::Bin(name) => {
                    args.push("--bin".to_owned());
                    args.push(name.clone());
                }
                NextestTarget::Test(name) => {
                    args.push("--test".to_owned());
                    args.push(name.clone());
                }
            }
        }
    }
    let expr = match (user_filter, plan.filter.as_deref()) {
        (Some(user), Some(filter)) => Some(format!("({user}) & ({filter})")),
        (Some(only), None) | (None, Some(only)) => Some(only.to_owned()),
        (None, None) => None,
    };
    if let Some(expr) = expr {
        args.push("-E".to_owned());
        args.push(expr);
    }
    args
}

/// Restreint les cibles d'un plan partiel aux binaires que le filtre `-E` de l'utilisateur
/// peut encore faire tourner (lot L4e).
///
/// Sans cela, `-E test(ci2)` compile tous les binaires ayant un test lancé alors que nextest
/// n'en exécutera que quelques-uns. Un binaire reste cible s'il a au moins un test `Run` que
/// `user_filter` sélectionne, et dans le **doute** :
/// - il est lancé en entier (`whole_binaries`) ou a une entrée `Run` pour cause d'erreur ;
/// - un fichier modifié est compilé dans ce binaire (ou le graphe ne connaît pas ses
///   fichiers) : il peut contenir des tests **nouveaux**, absents de l'inventaire, que le
///   filtre sélectionnerait — y compris quand aucun de ses tests connus n'est lancé (cible
///   propriétaire, voir [`owner_binaries`]) ;
/// - sa cible vient d'un binaire absent de la base (`whole_binaries`).
///
/// Un `binary_id(=…)` qui ne désigne aucun binaire connu (base ou `whole_binaries`) laisse le
/// plan intact : nextest refuse un tel identifiant, ce refus ne doit pas devenir un vert.
///
/// Les décisions du plan ne changent pas (les tests lancés non sélectionnés finiront
/// `filtered-out`), ni son filtre `-E`. Si plus aucune cible ne reste, `targets` vaut
/// `Some(vec![])` : l'exécution ne compile ni ne lance rien.
///
/// Renvoie le nombre de cibles retirées, ou `None` — plan inchangé — si l'expression sort du
/// sous-ensemble de [`FilterSet`], si la portée est « tout », ou si le plan compile déjà tout
/// (`targets == None`).
pub fn restrict_to_user_filter(
    plan: &mut Plan,
    base: &BaseInventory,
    graph: &GraphData,
    user_filter: &str,
) -> Option<usize> {
    let Scope::Partial(partial) = &plan.scope else {
        return None;
    };
    let current = plan.targets.as_ref()?;
    let filter = FilterSet::parse(user_filter)?;
    let kinds: BTreeMap<&str, &BaseBinary> = base
        .binaries
        .iter()
        .map(|b| (b.binary_id.as_str(), b))
        .collect();
    // nextest refuse un `binary_id(=…)` inconnu : ne pas le masquer par un « rien à lancer ».
    let cites_unknown_binary = filter
        .binary_ids()
        .into_iter()
        .any(|id| !kinds.contains_key(id) && !partial.whole_binaries.contains_key(id));
    if cites_unknown_binary {
        return None;
    }
    let modified: BTreeSet<&str> = plan.modified_files.iter().map(String::as_str).collect();
    // Cibles propriétaires (L5 A5) : un binaire qui compile un fichier modifié peut contenir un
    // test NEUF que le filtre de l'utilisateur sélectionne, même sans test `Run` connu.
    let owner_targets: BTreeSet<NextestTarget> = owner_binaries(base, graph, &partial.modified)
        .into_iter()
        .filter_map(|id| kinds.get(id).and_then(|b| target_of(b)))
        .collect();

    let mut with_run: BTreeSet<&str> = BTreeSet::new();
    let mut needed_binaries: BTreeSet<&str> = BTreeSet::new();
    for entry in plan
        .entries
        .iter()
        .filter(|e| e.decision == Decision::Run && e.reasons.iter().any(|r| !only_ignored(r)))
    {
        let id = entry.test.binary_id.as_str();
        with_run.insert(id);
        let doubt = entry.reasons.iter().any(|r| matches!(r, Reason::Error(_)));
        if doubt || filter.matches(id, &entry.test.name) {
            needed_binaries.insert(id);
        }
    }
    let mut explained: BTreeSet<NextestTarget> = BTreeSet::new();
    let mut needed: BTreeSet<NextestTarget> = owner_targets;
    for id in with_run {
        let target = target_of(kinds.get(id)?)?;
        let may_hold_new_tests = partial.whole_binaries.contains_key(id)
            || graph
                .binary_files
                .get(id)
                .is_none_or(|files| files.iter().any(|f| modified.contains(f.as_str())));
        if may_hold_new_tests || needed_binaries.contains(id) {
            needed.insert(target.clone());
        }
        explained.insert(target);
    }
    // Cibles de binaires absents de la base : toujours gardées.
    let from_absent: BTreeSet<NextestTarget> = partial
        .whole_binaries
        .keys()
        .filter(|id| !kinds.contains_key(id.as_str()))
        .filter_map(|id| target_of_new_binary(id).ok())
        .collect();
    let kept: Vec<NextestTarget> = current
        .iter()
        .filter(|t| from_absent.contains(t) || !explained.contains(t) || needed.contains(t))
        .cloned()
        .collect();
    let removed = current.len() - kept.len();
    plan.targets = Some(kept);
    Some(removed)
}

// ---------------------------------------------------------------------------------------
// États par test
// ---------------------------------------------------------------------------------------

#[derive(Debug, Default)]
struct State {
    run_reasons: Vec<Reason>,
    notes: Vec<Reason>,
}

impl State {
    fn run(&mut self, reasons: impl IntoIterator<Item = Reason>) {
        for reason in reasons {
            if !self.run_reasons.contains(&reason) {
                self.run_reasons.push(reason);
            }
        }
    }

    fn note(&mut self, reason: Reason) {
        if !self.notes.contains(&reason) {
            self.notes.push(reason);
        }
    }

    fn decision(&self) -> Decision {
        if self.run_reasons.is_empty() {
            Decision::Skip
        } else {
            Decision::Run
        }
    }

    fn is_run(&self) -> bool {
        !self.run_reasons.is_empty()
    }

    /// Ce test justifie-t-il de compiler son binaire ? Non s'il n'est lancé QUE parce qu'il
    /// est `#[ignore]` dans la base : nextest ne l'exécute pas sans `--run-ignored`, le
    /// compiler ne servirait à rien (il reste `Run` dans le journal, résultat `ignored`). S'il
    /// a une autre raison (atteint un fichier modifié, binaire entier, d1…), il compte.
    fn needs_compilation(&self) -> bool {
        self.run_reasons.iter().any(|r| !only_ignored(r))
    }

    fn finish(self, graph_digest: &str) -> Vec<Reason> {
        let mut reasons = if self.run_reasons.is_empty() {
            vec![Reason::NoPathToModified {
                graph_digest: graph_digest.to_owned(),
            }]
        } else {
            self.run_reasons
        };
        for note in self.notes {
            if !reasons.contains(&note) {
                reasons.push(note);
            }
        }
        reasons
    }
}

/// Raison qui ne justifie PAS de compiler : test `#[ignore]` dans la base (nextest ne le
/// lance pas par défaut), ou simple note de journal (`D1SkipIgnored`).
fn only_ignored(reason: &Reason) -> bool {
    matches!(
        reason,
        Reason::BaseNotGreen(BaseStatus::Ignored) | Reason::D1SkipIgnored
    )
}

fn inventory_states(base: &BaseInventory) -> BTreeMap<TestId, State> {
    let mut states = BTreeMap::new();
    for binary in &base.binaries {
        for test in &binary.tests {
            states.insert(
                TestId {
                    binary_id: binary.binary_id.clone(),
                    name: test.name.clone(),
                },
                State::default(),
            );
        }
    }
    states
}

// ---------------------------------------------------------------------------------------
// Sélection partielle
// ---------------------------------------------------------------------------------------

fn select_partial(states: &mut BTreeMap<TestId, State>, partial: &PartialScope, graph: &GraphData) {
    for (test, state) in states.iter_mut() {
        if let Some(reason) = partial.whole_binaries.get(&test.binary_id) {
            state.run([reason.clone()]);
        }
        if let Some(reason) = partial.forced_tests.get(test) {
            state.run([reason.clone()]);
        }
        let roots = graph.test_roots.get(test).filter(|r| !r.is_empty());
        let Some(roots) = roots else {
            state.run([Reason::Error(
                "racine du test inconnue dans le graphe".to_owned(),
            )]);
            continue;
        };
        if partial.modified.is_empty() {
            continue;
        }
        let reached = reach(graph, roots);
        let hit: BTreeSet<&RelPath> = partial.modified.intersection(&reached).collect();
        if hit.is_empty() {
            continue;
        }
        let paths = shortest_paths(graph, test, roots, &hit);
        for file in hit {
            match paths.get(file) {
                Some(path) => {
                    state.run([Reason::ReachesModified {
                        file: file.clone(),
                        via: path.clone(),
                    }]);
                    if path_is_learned(graph, test, path) {
                        state.run([Reason::Learned { file: file.clone() }]);
                    }
                }
                // Atteint selon `reach` mais chemin introuvable : ne devrait pas arriver,
                // le doute lance quand même le test.
                None => state.run([Reason::ReachesModified {
                    file: file.clone(),
                    via: vec![file.clone()],
                }]),
            }
        }
    }
}

fn learned_only(graph: &GraphData, key: &str) -> bool {
    graph
        .provenance
        .get(key)
        .is_some_and(|sources| !sources.is_empty() && sources.iter().all(|s| s == "learned"))
}

fn root_key(test: &TestId, root: &str) -> String {
    format!("test:{} {} -> {root}", test.binary_id, test.name)
}

fn edge_key(from: &str, to: &str) -> String {
    format!("{from} -> {to}")
}

/// Le chemin emprunte-t-il une racine ou une arête dont la seule provenance est `learned` ?
fn path_is_learned(graph: &GraphData, test: &TestId, path: &[RelPath]) -> bool {
    path.first()
        .is_some_and(|root| learned_only(graph, &root_key(test, root)))
        || path
            .windows(2)
            .any(|w| learned_only(graph, &edge_key(&w[0], &w[1])))
}

/// BFS déterministe multi-sources depuis les racines : plus court chemin racine → fichier
/// pour chaque fichier visé. À longueur égale, les racines et arêtes non apprises passent
/// avant les apprises, puis l'ordre alphabétique.
fn shortest_paths(
    graph: &GraphData,
    test: &TestId,
    roots: &BTreeSet<RelPath>,
    targets: &BTreeSet<&RelPath>,
) -> BTreeMap<RelPath, Vec<RelPath>> {
    let mut ordered_roots: Vec<&RelPath> = roots.iter().collect();
    ordered_roots.sort_by_key(|r| (learned_only(graph, &root_key(test, r)), *r));

    let mut parent: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    let mut queue: VecDeque<&str> = VecDeque::new();
    for root in ordered_roots {
        if parent.insert(root.as_str(), None).is_none() {
            queue.push_back(root.as_str());
        }
    }
    let mut remaining: BTreeSet<&str> = targets
        .iter()
        .map(|t| t.as_str())
        .filter(|t| !parent.contains_key(t))
        .collect();
    while let Some(file) = queue.pop_front() {
        if remaining.is_empty() {
            break;
        }
        let Some(next) = graph.edges.get(file) else {
            continue;
        };
        let mut ordered: Vec<&String> = next.iter().collect();
        ordered.sort_by_key(|n| (learned_only(graph, &edge_key(file, n)), *n));
        for target in ordered {
            if !parent.contains_key(target.as_str()) {
                parent.insert(target.as_str(), Some(file));
                remaining.remove(target.as_str());
                queue.push_back(target.as_str());
            }
        }
    }

    let mut paths = BTreeMap::new();
    for target in targets {
        if !parent.contains_key(target.as_str()) {
            continue;
        }
        let mut path = vec![(*target).clone()];
        let mut current = target.as_str();
        while let Some(Some(prev)) = parent.get(current) {
            path.push((*prev).to_owned());
            current = prev;
        }
        path.reverse();
        paths.insert((*target).clone(), path);
    }
    paths
}

// ---------------------------------------------------------------------------------------
// d1 (S5)
// ---------------------------------------------------------------------------------------

/// d1 ne peut qu'ajouter : `lancés(avec d1) ⊇ lancés(sans d1)`. Les sauts proposés sont
/// ignorés (la décision ne change pas, la raison `D1SkipIgnored` est ajoutée).
fn apply_d1(states: &mut BTreeMap<TestId, State>, proposal: &D1Proposal) {
    for test in &proposal.add {
        if let Some(state) = states.get_mut(test) {
            state.run([Reason::D1Added]);
        } else {
            // Test inconnu : si le binaire est connu, tout le binaire est lancé (le test
            // est peut-être nouveau). Binaire inconnu : ignoré, la classification s'en charge.
            for (known, state) in states.iter_mut() {
                if known.binary_id == test.binary_id {
                    state.run([Reason::D1Added]);
                }
            }
        }
    }
    for test in &proposal.skip {
        if let Some(state) = states.get_mut(test) {
            state.note(Reason::D1SkipIgnored);
        }
    }
}

// ---------------------------------------------------------------------------------------
// Cibles et filtre
// ---------------------------------------------------------------------------------------

fn target_of(binary: &BaseBinary) -> Option<NextestTarget> {
    match binary.kind {
        BinaryKind::Lib => Some(NextestTarget::Lib),
        BinaryKind::Bin => Some(NextestTarget::Bin(binary.target_name.clone())),
        BinaryKind::Test => Some(NextestTarget::Test(binary.target_name.clone())),
        BinaryKind::Other => None,
    }
}

/// Cible Cargo de chaque binaire de la base (`None` : nature « autre », suite complète) —
/// pour l'oracle de complétude de l'exécution.
pub(super) fn base_targets(base: &BaseInventory) -> BTreeMap<String, Option<NextestTarget>> {
    base.binaries
        .iter()
        .map(|binary| (binary.binary_id.clone(), target_of(binary)))
        .collect()
}

/// Cible d'un binaire absent de la base, d'après son identifiant nextest.
/// `Err(())` = nature inconnue (exemple, bench…) : suite complète.
fn target_of_new_binary(binary_id: &str) -> Result<NextestTarget, ()> {
    match binary_id.split_once("::") {
        None => Ok(NextestTarget::Lib),
        Some((_, rest)) => match rest.split_once('/') {
            None if !rest.is_empty() => Ok(NextestTarget::Test(rest.to_owned())),
            Some(("bin", name)) if !name.is_empty() => Ok(NextestTarget::Bin(name.to_owned())),
            _ => Err(()),
        },
    }
}

/// Binaires de la base que les fichiers modifiés atteignent À LA COMPILATION (dep-info
/// rustc, [`GraphData::binary_files`] : autorité fichier → binaire).
///
/// Un tel binaire doit être compilé même si aucun de ses tests connus n'est lancé : il peut
/// contenir un test NEUF (ex. un `#[test]` ajouté à un fichier de production dont la cible
/// lib n'a que des tests `#[ignore]` ou sautés), que seule la compilation révèle (L5 A5,
/// S4/S7). Un binaire de la base absent de `binary_files` est un doute : il est cible.
fn owner_binaries<'a>(
    base: &'a BaseInventory,
    graph: &GraphData,
    modified: &BTreeSet<RelPath>,
) -> BTreeSet<&'a str> {
    if modified.is_empty() {
        return BTreeSet::new();
    }
    base.binaries
        .iter()
        .filter(|binary| {
            graph
                .binary_files
                .get(&binary.binary_id)
                .is_none_or(|files| files.iter().any(|file| modified.contains(file)))
        })
        .map(|binary| binary.binary_id.as_str())
        .collect()
}

/// `None` = suite complète (aucun argument de cible).
///
/// Cibles : les binaires ayant un test à compiler pour lui-même, les cibles propriétaires
/// des fichiers modifiés (`owners`) et les binaires entiers absents de la base.
fn compute_targets(
    states: &BTreeMap<TestId, State>,
    kinds: &BTreeMap<&str, &BaseBinary>,
    whole_absent: &[BinaryId],
    owners: &BTreeSet<&str>,
) -> Option<Vec<NextestTarget>> {
    let mut targets = BTreeSet::new();
    let mut binaries: BTreeSet<&str> = states
        .iter()
        .filter(|(_, s)| s.needs_compilation())
        .map(|(t, _)| t.binary_id.as_str())
        .collect();
    binaries.extend(owners.iter().copied());
    for id in binaries {
        // Un binaire sans fiche dans l'inventaire ne peut pas exister (états construits
        // depuis l'inventaire) ; le doute donne la suite complète.
        let target = kinds.get(id).and_then(|b| target_of(b))?;
        targets.insert(target);
    }
    for id in whole_absent {
        targets.insert(target_of_new_binary(id).ok()?);
    }
    Some(targets.into_iter().collect())
}

/// Binaires de la base effectivement compilés par `targets` ; `None` = tous (suite complète).
///
/// C'est sur cet ensemble que raisonne le filtre d'exclusion : tout test connu et sauté d'un
/// binaire compilé doit être exclu, qu'il ait ou non un voisin lancé.
fn compiled_binaries<'a>(
    kinds: &BTreeMap<&'a str, &BaseBinary>,
    targets: Option<&[NextestTarget]>,
) -> Option<BTreeSet<&'a str>> {
    let targets = targets?;
    Some(
        kinds
            .iter()
            .filter(|(_, binary)| target_of(binary).is_some_and(|t| targets.contains(&t)))
            .map(|(id, _)| *id)
            .collect(),
    )
}

/// Un nom de test ou de binaire que l'on sait écrire sans ambiguïté dans un filterset
/// nextest (`test(=…)`, `binary_id(=…)`) : ASCII alphanumérique, `_` et `:` (+ `-` et `/`
/// pour un identifiant de binaire).
fn expressible(text: &str, binary_id: bool) -> bool {
    !text.is_empty()
        && text.chars().all(|c| {
            c.is_ascii_alphanumeric()
                || c == '_'
                || c == ':'
                || (binary_id && (c == '-' || c == '/'))
        })
}

/// Un test sauté dont le nom (ou le binaire) ne peut pas s'écrire dans le filtre est lancé.
fn force_unexpressible(states: &mut BTreeMap<TestId, State>) {
    for (test, state) in states.iter_mut() {
        let expressible_test = expressible(&test.name, false) && expressible(&test.binary_id, true);
        if !state.is_run() && !expressible_test {
            state.run([Reason::Error(
                "nom de test non exprimable sans ambiguïté dans un filtre nextest".to_owned(),
            )]);
        }
    }
}

/// Expression qui EXCLUT exactement les tests sautés des binaires compilés.
///
/// `compiled` : binaires compilés par les cibles (`None` : aucune cible n'est donnée à cargo,
/// la suite complète est compilée, les tests sautés de TOUS les binaires sont exclus). Un test
/// absent de l'inventaire (nouveau) n'est jamais exclu, donc toujours lancé.
fn build_filter(
    states: &BTreeMap<TestId, State>,
    compiled: Option<&BTreeSet<&str>>,
) -> Option<String> {
    let mut by_binary: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (test, state) in states {
        if !state.is_run() {
            by_binary
                .entry(test.binary_id.as_str())
                .or_default()
                .push(test.name.as_str());
        }
    }
    let terms: Vec<String> = by_binary
        .into_iter()
        .filter(|(binary, _)| compiled.is_none_or(|set| set.contains(binary)))
        .map(|(binary, names)| {
            let tests: Vec<String> = names.iter().map(|n| format!("test(={n})")).collect();
            format!("(binary_id(={binary}) & ({}))", tests.join(" | "))
        })
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(format!("not ({})", terms.join(" | ")))
    }
}

/// Filtre trop long : les binaires compilés ayant des tests sautés sont lancés en entier
/// (tous les binaires si aucune cible n'est donnée à cargo).
fn widen_targeted_binaries(
    states: &mut BTreeMap<TestId, State>,
    compiled: Option<&BTreeSet<&str>>,
) {
    for (test, state) in states.iter_mut() {
        if !state.is_run() && compiled.is_none_or(|set| set.contains(test.binary_id.as_str())) {
            state.run([Reason::Error(
                "filtre nextest trop long : binaire lancé en entier".to_owned(),
            )]);
        }
    }
}
