//! Réconciliation du plan avec les résultats de nextest (pure, sans processus).
//!
//! Chaque test lancé reçoit son résultat `JUnit`. Un test absent du `JUnit` après une fin
//! normale est `ignored` (`#[ignore]`), `filtered-out` (seulement avec un `-E` utilisateur)
//! ou `not-run` — et `not-run` rend le run incomplet. Un test présent dans le `JUnit` mais
//! inconnu du plan est un test nouveau : il est ajouté au journal, lancé.
//!
//! L'oracle de complétude (L5 A5, [`check_listing`]) compare en plus ces résultats à la liste
//! indépendante de `cargo nextest list` : aucun test que nextest connaît ne peut manquer du
//! journal sans que le run soit déclaré incomplet.

use std::collections::{BTreeMap, BTreeSet};

use crate::select::base::JunitResults;
use crate::select::journal::{Outcome, TestOutcome};
use crate::select::{
    BaseStatus, Decision, JournalEntry, NextestTarget, Plan, Reason, Scope, TestId,
};

/// Comment s'est terminé le lancement de nextest.
#[derive(Debug)]
pub(super) enum End {
    /// Fin normale (code 0 ou 100) : résultats lus.
    Normal(JunitResults),
    /// Échec de compilation : aucun test n'a tourné.
    BuildFailed,
    /// Délai, signal, `JUnit` absent ou illisible : aucun résultat n'est fiable.
    Abnormal(String),
    /// Rien n'a été lancé (plan partiel sans test à lancer).
    NothingToRun,
    /// Fin normale, mais l'oracle de complétude a refusé le run (raison fournie) : les
    /// résultats lus restent visibles (un rouge reste rouge, S7) mais un test lancé qui
    /// manque est `not-run` ; le run est incomplet.
    Unverified(JunitResults, String),
}

/// Entrées de la réconciliation.
pub(super) struct Input<'a> {
    /// Plan exécuté.
    pub plan: &'a Plan,
    /// Binaires de l'inventaire de la base (vide si la base est illisible).
    pub inventory_binaries: &'a BTreeSet<String>,
    /// Tests `#[ignore]` de la base.
    pub ignored: &'a BTreeSet<TestId>,
    /// Filtre `-E` de l'utilisateur.
    pub user_filter: Option<&'a str>,
    /// Comment nextest s'est terminé.
    pub end: &'a End,
    /// Ce que l'oracle de complétude a vu en plus des résultats.
    pub observed: &'a Observed,
}

/// Résultat de la réconciliation.
#[derive(Debug, Default)]
pub(super) struct Reconciled {
    /// Entrées du journal : celles du plan + les tests nouveaux (triées).
    pub entries: Vec<JournalEntry>,
    /// Résultat de chaque test lancé (triés).
    pub outcomes: Vec<TestOutcome>,
    /// Tests de l'inventaire de la base.
    pub inventory: BTreeSet<TestId>,
    /// Tests nouveaux.
    pub new: BTreeSet<TestId>,
}

impl Reconciled {
    /// Au moins un test a échoué.
    pub fn any_failed(&self) -> bool {
        self.count(Outcome::Failed) != 0
    }

    /// Au moins un test qui devait tourner n'a pas de résultat.
    pub fn any_not_run(&self) -> bool {
        self.count(Outcome::NotRun) != 0
    }

    /// Nombre de résultats d'une sorte.
    pub fn count(&self, wanted: Outcome) -> usize {
        self.outcomes.iter().filter(|o| o.outcome == wanted).count()
    }
}

/// Tests que la liste de nextest montre et que le `JUnit` ne contient pas, une fois l'oracle
/// satisfait.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Observed {
    /// Tests inconnus du plan, listés, non lancés (nouveau `#[ignore]`, ou exclus par le `-E`
    /// de l'utilisateur) : résultat à journaliser.
    pub new_outcomes: BTreeMap<TestId, Outcome>,
    /// Tests du plan que la liste dit `#[ignore]` (même s'ils ne l'étaient pas dans la base).
    pub ignored: BTreeSet<TestId>,
}

/// Pourquoi nextest ne sélectionne pas un test listé (`filter-match` de `nextest list`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ListedMatch {
    /// `{"status":"matches"}` : le test doit être lancé.
    Matches,
    /// `mismatch` / `ignored` : `#[ignore]`.
    Ignored,
    /// `mismatch` / `expression` : écarté par l'expression `-E`.
    Expression,
    /// Toute autre raison (profil, partition, nom…) : non attendue ici.
    Other(String),
}

/// Un binaire vu par `cargo nextest list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ListedSuite {
    /// `binary-id`.
    pub binary_id: String,
    /// `binary-name`.
    pub binary_name: String,
    /// `kind` : `lib`, `bin`, `test`…
    pub kind: String,
    /// `status == "listed"` (sinon nextest a écarté le binaire entier, ex. `binary_id(=…)`).
    pub listed: bool,
    /// Tests listés.
    pub tests: BTreeMap<String, ListedMatch>,
}

impl ListedSuite {
    fn is_target(&self, target: &NextestTarget) -> bool {
        match target {
            NextestTarget::Lib => self.kind == "lib",
            NextestTarget::Bin(name) => self.kind == "bin" && &self.binary_name == name,
            NextestTarget::Test(name) => self.kind == "test" && &self.binary_name == name,
        }
    }
}

/// Entrées de l'oracle de complétude.
pub(super) struct OracleInput<'a> {
    /// Plan exécuté.
    pub plan: &'a Plan,
    /// Binaires vus par `cargo nextest list` (même cibles, même `-E`), par `binary-id`.
    pub listing: &'a BTreeMap<String, ListedSuite>,
    /// Résultats du `JUnit`.
    pub results: &'a JunitResults,
    /// Cible Cargo de chaque binaire de la base (`None` : nature « autre »).
    pub inventory: &'a BTreeMap<String, Option<NextestTarget>>,
    /// L'utilisateur a donné un `-E`.
    pub user_filter: bool,
}

/// Oracle de complétude indépendant (fonction pure) : tout test que nextest liste doit être
/// expliqué par le `JUnit`, le plan ou une raison connue de ne pas tourner.
///
/// - un binaire ciblé (cible du plan, ou binaire de la base compilé par elle) absent de la
///   liste est une erreur ; écarté en entier (`skipped`) il n'est accepté qu'avec un `-E`
///   utilisateur (c'est son filtre qui l'écarte) ;
/// - test présent dans le `JUnit` : normal ;
/// - `matches` mais absent du `JUnit` : erreur ;
/// - `mismatch ignored` : résultat `ignored` (nouveau test : entrée `new-test`) ;
/// - `mismatch expression` : normal pour un saut du plan (notre exclusion) ou, avec un `-E`
///   utilisateur, pour un test lancé (`filtered-out`) ; un test NEUF y est journalisé
///   `filtered-out` ; sans `-E` utilisateur c'est une erreur (notre filtre n'exclut jamais
///   un test neuf, ni un test lancé) ;
/// - toute autre raison : erreur.
///
/// # Errors
/// Le premier écart trouvé, en clair : le run est alors incomplet (S3).
pub(super) fn check_listing(input: &OracleInput<'_>) -> Result<Observed, String> {
    if let Some(targets) = &input.plan.targets {
        for target in targets {
            if !input.listing.values().any(|suite| suite.is_target(target)) {
                return Err(format!("cible {target:?} absente de la liste de nextest"));
            }
        }
        for (binary_id, target) in input.inventory {
            let compiled = target.as_ref().is_some_and(|t| targets.contains(t));
            if compiled && !input.listing.contains_key(binary_id) {
                return Err(format!(
                    "binaire {binary_id} compilé mais absent de la liste"
                ));
            }
        }
    }
    let decisions: BTreeMap<&TestId, Decision> = input
        .plan
        .entries
        .iter()
        .map(|entry| (&entry.test, entry.decision))
        .collect();
    let mut observed = Observed::default();
    for suite in input.listing.values() {
        if !suite.listed {
            if input.user_filter {
                continue;
            }
            return Err(format!(
                "binaire {} écarté par nextest sans filtre utilisateur",
                suite.binary_id
            ));
        }
        for (name, listed) in &suite.tests {
            let test = TestId {
                binary_id: suite.binary_id.clone(),
                name: name.clone(),
            };
            if input
                .results
                .contains_key(&(test.binary_id.clone(), test.name.clone()))
            {
                continue;
            }
            let decision = decisions.get(&test).copied();
            match (listed, decision) {
                (ListedMatch::Ignored, Some(_)) => {
                    observed.ignored.insert(test);
                }
                (ListedMatch::Ignored, None) => {
                    observed.new_outcomes.insert(test, Outcome::Ignored);
                }
                (ListedMatch::Expression, Some(Decision::Skip)) => {}
                (ListedMatch::Expression, Some(Decision::Run)) if input.user_filter => {}
                (ListedMatch::Expression, None) if input.user_filter => {
                    observed.new_outcomes.insert(test, Outcome::FilteredOut);
                }
                (ListedMatch::Matches, _) => {
                    return Err(format!(
                        "test {} {} listé et sélectionné, absent du JUnit",
                        test.binary_id, test.name
                    ));
                }
                (ListedMatch::Expression, _) => {
                    return Err(format!(
                        "test {} {} écarté par l'expression sans filtre utilisateur",
                        test.binary_id, test.name
                    ));
                }
                (ListedMatch::Other(reason), _) => {
                    return Err(format!(
                        "test {} {} écarté par nextest ({reason})",
                        test.binary_id, test.name
                    ));
                }
            }
        }
    }
    Ok(observed)
}

fn from_status(status: BaseStatus) -> Outcome {
    match status {
        BaseStatus::Pass => Outcome::Passed,
        BaseStatus::Fail => Outcome::Failed,
        BaseStatus::Flaky => Outcome::Flaky,
        BaseStatus::Ignored => Outcome::Ignored,
        // Jamais produit par la lecture du JUnit ; jamais un vert par défaut.
        BaseStatus::Unknown => Outcome::NotRun,
    }
}

/// Raison d'un test qui n'est pas dans le plan.
fn new_test_reason(input: &Input<'_>, binary_id: &str) -> Reason {
    if input.inventory_binaries.is_empty() {
        // Pas de base lisible : tout vient des résultats, la portée donne la raison.
        if let Scope::Everything { reasons } = &input.plan.scope
            && let Some(first) = reasons.first()
        {
            return first.clone();
        }
        return Reason::Error("aucune base lisible".to_owned());
    }
    if input.inventory_binaries.contains(binary_id) {
        Reason::NewTest
    } else {
        Reason::NewBinary(binary_id.to_owned())
    }
}

/// Réconcilie le plan et les résultats.
pub(super) fn reconcile(input: &Input<'_>) -> Reconciled {
    let (mut results, normal) = match input.end {
        End::Normal(results) => (results.clone(), true),
        End::NothingToRun => (JunitResults::new(), true),
        End::Unverified(results, _) => (results.clone(), false),
        End::BuildFailed | End::Abnormal(_) => (JunitResults::new(), false),
    };
    let mut out = Reconciled::default();
    for entry in &input.plan.entries {
        out.inventory.insert(entry.test.clone());
        out.entries.push(entry.clone());
        let found = results.remove(&(entry.test.binary_id.clone(), entry.test.name.clone()));
        let outcome = match (found, entry.decision) {
            (Some((status, _)), _) => Some(from_status(status)),
            // Sauté et pas lancé : aucun résultat (c'est le cas normal d'un saut).
            (None, Decision::Skip) => None,
            (None, Decision::Run) if !normal => Some(Outcome::NotRun),
            (None, Decision::Run)
                if input.ignored.contains(&entry.test)
                    || input.observed.ignored.contains(&entry.test) =>
            {
                Some(Outcome::Ignored)
            }
            (None, Decision::Run) if input.user_filter.is_some() => Some(Outcome::FilteredOut),
            (None, Decision::Run) => Some(Outcome::NotRun),
        };
        if let Some(outcome) = outcome {
            out.outcomes.push(TestOutcome {
                test: entry.test.clone(),
                outcome,
            });
        }
    }
    // Tests vus par nextest mais inconnus du plan : nouveaux, lancés.
    for ((binary_id, name), (status, _)) in results {
        let test = TestId { binary_id, name };
        let reason = new_test_reason(input, &test.binary_id);
        out.new.insert(test.clone());
        out.entries.push(JournalEntry {
            test: test.clone(),
            decision: Decision::Run,
            reasons: vec![reason],
        });
        out.outcomes.push(TestOutcome {
            test,
            outcome: from_status(status),
        });
    }
    // Tests listés par nextest, inconnus du plan et du JUnit (nouveau `#[ignore]`, ou exclu
    // par le `-E` de l'utilisateur) : journalisés, jamais absents.
    for (test, outcome) in &input.observed.new_outcomes {
        if out.inventory.contains(test) || out.new.contains(test) {
            continue;
        }
        let reason = new_test_reason(input, &test.binary_id);
        out.new.insert(test.clone());
        out.entries.push(JournalEntry {
            test: test.clone(),
            decision: Decision::Run,
            reasons: vec![reason],
        });
        out.outcomes.push(TestOutcome {
            test: test.clone(),
            outcome: *outcome,
        });
    }
    out.entries.sort_by(|a, b| a.test.cmp(&b.test));
    out.outcomes.sort_by(|a, b| a.test.cmp(&b.test));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::select::{GRAPH_SCHEMA, GraphData};

    fn tid(binary: &str, name: &str) -> TestId {
        TestId {
            binary_id: binary.to_owned(),
            name: name.to_owned(),
        }
    }

    fn entry(test: &TestId, decision: Decision) -> JournalEntry {
        let reason = match decision {
            Decision::Run => Reason::NewTest,
            Decision::Skip => Reason::NoPathToModified {
                graph_digest: "g".to_owned(),
            },
        };
        JournalEntry {
            test: test.clone(),
            decision,
            reasons: vec![reason],
        }
    }

    fn plan_of(entries: Vec<JournalEntry>) -> Plan {
        let graph = GraphData {
            schema: GRAPH_SCHEMA.to_owned(),
            digest: "g".to_owned(),
            ..GraphData::default()
        };
        Plan {
            scope: Scope::Everything {
                reasons: vec![Reason::ProtectedBranch("main".to_owned())],
            },
            base_commit: "c".repeat(40),
            graph_digest: graph.digest,
            modified_files: vec![],
            entries,
            targets: None,
            filter: None,
        }
    }

    fn junit(rows: &[(&str, &str, BaseStatus)]) -> JunitResults {
        rows.iter()
            .map(|(b, n, s)| (((*b).to_owned(), (*n).to_owned()), (*s, Some(1))))
            .collect()
    }

    fn outcome_of(r: &Reconciled, test: &TestId) -> Option<Outcome> {
        r.outcomes
            .iter()
            .find(|o| &o.test == test)
            .map(|o| o.outcome)
    }

    #[test]
    fn results_map_one_to_one_and_skips_have_no_outcome() {
        let (a, b, c) = (tid("x", "a"), tid("x", "b"), tid("x", "c"));
        let plan = plan_of(vec![
            entry(&a, Decision::Run),
            entry(&b, Decision::Run),
            entry(&c, Decision::Skip),
        ]);
        let end = End::Normal(junit(&[
            ("x", "a", BaseStatus::Pass),
            ("x", "b", BaseStatus::Fail),
        ]));
        let none = BTreeSet::new();
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &BTreeSet::new(),
            user_filter: None,
            end: &end,
            observed: &Observed::default(),
        });
        assert_eq!(outcome_of(&r, &a), Some(Outcome::Passed));
        assert_eq!(outcome_of(&r, &b), Some(Outcome::Failed));
        assert_eq!(outcome_of(&r, &c), None);
        assert!(r.any_failed() && !r.any_not_run());
        assert_eq!(r.inventory.len(), 3);
        assert!(r.new.is_empty());
    }

    #[test]
    fn a_missing_test_is_ignored_filtered_out_or_not_run() {
        let (ign, miss) = (tid("x", "ign"), tid("x", "miss"));
        let plan = plan_of(vec![
            entry(&ign, Decision::Run),
            entry(&miss, Decision::Run),
        ]);
        let end = End::Normal(JunitResults::new());
        let ignored = BTreeSet::from([ign.clone()]);
        let none = BTreeSet::new();
        let without_filter = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &ignored,
            user_filter: None,
            end: &end,
            observed: &Observed::default(),
        });
        assert_eq!(outcome_of(&without_filter, &ign), Some(Outcome::Ignored));
        assert_eq!(outcome_of(&without_filter, &miss), Some(Outcome::NotRun));
        assert!(without_filter.any_not_run());
        let with_filter = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &ignored,
            user_filter: Some("test(zzz)"),
            end: &end,
            observed: &Observed::default(),
        });
        assert_eq!(outcome_of(&with_filter, &ign), Some(Outcome::Ignored));
        assert_eq!(outcome_of(&with_filter, &miss), Some(Outcome::FilteredOut));
        assert!(!with_filter.any_not_run());
    }

    #[test]
    fn an_abnormal_end_trusts_nothing() {
        let a = tid("x", "a");
        let plan = plan_of(vec![entry(&a, Decision::Run)]);
        let none = BTreeSet::new();
        for end in [End::BuildFailed, End::Abnormal("délai".to_owned())] {
            let r = reconcile(&Input {
                plan: &plan,
                inventory_binaries: &none,
                ignored: &BTreeSet::from([a.clone()]),
                user_filter: Some("all()"),
                end: &end,
                observed: &Observed::default(),
            });
            assert_eq!(outcome_of(&r, &a), Some(Outcome::NotRun), "{end:?}");
        }
    }

    #[test]
    fn new_tests_are_added_as_run_entries() {
        let known = tid("x", "known");
        let plan = plan_of(vec![entry(&known, Decision::Run)]);
        let end = End::Normal(junit(&[
            ("x", "known", BaseStatus::Pass),
            ("x", "fresh", BaseStatus::Pass),
            ("y", "other", BaseStatus::Fail),
        ]));
        let binaries = BTreeSet::from(["x".to_owned()]);
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &binaries,
            ignored: &BTreeSet::new(),
            user_filter: None,
            end: &end,
            observed: &Observed::default(),
        });
        assert_eq!(r.new.len(), 2);
        let reason_of = |t: &TestId| {
            r.entries
                .iter()
                .find(|e| &e.test == t)
                .map(|e| (e.decision, e.reasons.clone()))
        };
        assert_eq!(
            reason_of(&tid("x", "fresh")),
            Some((Decision::Run, vec![Reason::NewTest]))
        );
        assert_eq!(
            reason_of(&tid("y", "other")),
            Some((Decision::Run, vec![Reason::NewBinary("y".to_owned())]))
        );
        assert_eq!(outcome_of(&r, &tid("y", "other")), Some(Outcome::Failed));
        assert_eq!(r.entries.len(), 3);
    }

    #[test]
    fn without_a_base_the_scope_reason_explains_every_test() {
        let plan = Plan {
            scope: Scope::Everything {
                reasons: vec![Reason::Error("base introuvable".to_owned())],
            },
            ..plan_of(vec![])
        };
        let end = End::Normal(junit(&[("x", "a", BaseStatus::Pass)]));
        let none = BTreeSet::new();
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &BTreeSet::new(),
            user_filter: None,
            end: &end,
            observed: &Observed::default(),
        });
        assert!(r.inventory.is_empty());
        assert_eq!(
            r.entries[0].reasons,
            vec![Reason::Error("base introuvable".to_owned())]
        );
    }

    #[test]
    fn a_skipped_test_that_ran_and_failed_is_seen() {
        let c = tid("x", "c");
        let plan = plan_of(vec![entry(&c, Decision::Skip)]);
        let end = End::Normal(junit(&[("x", "c", BaseStatus::Fail)]));
        let none = BTreeSet::new();
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &BTreeSet::new(),
            user_filter: None,
            end: &end,
            observed: &Observed::default(),
        });
        assert!(
            r.any_failed(),
            "a failure is never hidden by a skip decision"
        );
    }

    // --- Oracle de complétude (A5) ---

    fn suite(
        binary_id: &str,
        kind: &str,
        name: &str,
        tests: &[(&str, ListedMatch)],
    ) -> (String, ListedSuite) {
        (
            binary_id.to_owned(),
            ListedSuite {
                binary_id: binary_id.to_owned(),
                binary_name: name.to_owned(),
                kind: kind.to_owned(),
                listed: true,
                tests: tests
                    .iter()
                    .map(|(n, m)| ((*n).to_owned(), m.clone()))
                    .collect(),
            },
        )
    }

    fn partial_plan(entries: Vec<JournalEntry>, targets: Option<Vec<NextestTarget>>) -> Plan {
        Plan {
            scope: Scope::Partial(crate::select::PartialScope::default()),
            targets,
            ..plan_of(entries)
        }
    }

    fn oracle(
        plan: &Plan,
        suites: Vec<(String, ListedSuite)>,
        results: &JunitResults,
        user_filter: bool,
    ) -> Result<Observed, String> {
        let listing: BTreeMap<String, ListedSuite> = suites.into_iter().collect();
        let inventory = BTreeMap::from([
            ("lib".to_owned(), Some(NextestTarget::Lib)),
            ("it".to_owned(), Some(NextestTarget::Test("it".to_owned()))),
            ("ex".to_owned(), None),
        ]);
        check_listing(&OracleInput {
            plan,
            listing: &listing,
            results,
            inventory: &inventory,
            user_filter,
        })
    }

    #[test]
    fn the_oracle_accepts_a_listed_and_run_test_and_a_skipped_one_the_filter_excluded() {
        let (a, skipped) = (tid("lib", "a"), tid("lib", "skipped"));
        let plan = partial_plan(
            vec![entry(&a, Decision::Run), entry(&skipped, Decision::Skip)],
            Some(vec![NextestTarget::Lib]),
        );
        let results = junit(&[("lib", "a", BaseStatus::Pass)]);
        let suites = vec![suite(
            "lib",
            "lib",
            "lib",
            &[
                ("a", ListedMatch::Matches),
                ("skipped", ListedMatch::Expression),
                ("ign_skipped", ListedMatch::Ignored),
            ],
        )];
        let observed = oracle(&plan, suites, &results, false).unwrap();
        // `ign_skipped` est `#[ignore]` et inconnu du plan : nouvelle entrée `ignored`.
        assert_eq!(
            observed.new_outcomes,
            BTreeMap::from([(tid("lib", "ign_skipped"), Outcome::Ignored)])
        );
        assert!(observed.ignored.is_empty());
    }

    #[test]
    fn a_listed_selected_test_missing_from_the_junit_is_an_error() {
        let a = tid("lib", "a");
        let plan = partial_plan(
            vec![entry(&a, Decision::Run)],
            Some(vec![NextestTarget::Lib]),
        );
        let suites = || vec![suite("lib", "lib", "lib", &[("a", ListedMatch::Matches)])];
        let err = oracle(&plan, suites(), &JunitResults::new(), false).unwrap_err();
        assert!(err.contains("absent du JUnit"), "{err}");
        // Un `-E` utilisateur n'y change rien : le test est sélectionné.
        assert!(oracle(&plan, suites(), &JunitResults::new(), true).is_err());
        // Un test NEUF listé `matches` mais jamais lancé (cas A5 d'origine) : erreur aussi.
        let plan = partial_plan(vec![], Some(vec![NextestTarget::Lib]));
        assert!(
            oracle(
                &plan,
                vec![suite(
                    "lib",
                    "lib",
                    "lib",
                    &[("fresh", ListedMatch::Matches)]
                )],
                &JunitResults::new(),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn a_new_ignored_test_is_journaled_ignored_and_a_planned_one_is_marked_ignored() {
        let planned_run = tid("lib", "now_ignored");
        let plan = partial_plan(
            vec![entry(&planned_run, Decision::Run)],
            Some(vec![NextestTarget::Lib]),
        );
        let suites = vec![suite(
            "lib",
            "lib",
            "lib",
            &[
                ("now_ignored", ListedMatch::Ignored),
                ("fresh_ignored", ListedMatch::Ignored),
            ],
        )];
        let observed = oracle(&plan, suites, &JunitResults::new(), false).unwrap();
        assert_eq!(
            observed.ignored,
            std::collections::BTreeSet::from([planned_run.clone()])
        );
        assert_eq!(
            observed.new_outcomes,
            BTreeMap::from([(tid("lib", "fresh_ignored"), Outcome::Ignored)])
        );
        // La réconciliation en tire `ignored` pour l'entrée du plan et une entrée neuve.
        let end = End::Normal(JunitResults::new());
        let none = BTreeSet::from(["lib".to_owned()]);
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &BTreeSet::new(),
            user_filter: None,
            end: &end,
            observed: &observed,
        });
        assert_eq!(outcome_of(&r, &planned_run), Some(Outcome::Ignored));
        assert_eq!(
            outcome_of(&r, &tid("lib", "fresh_ignored")),
            Some(Outcome::Ignored)
        );
        assert!(r.new.contains(&tid("lib", "fresh_ignored")));
        assert!(!r.any_not_run());
        let fresh = r
            .entries
            .iter()
            .find(|e| e.test == tid("lib", "fresh_ignored"))
            .unwrap();
        assert_eq!(
            (fresh.decision, fresh.reasons.clone()),
            (Decision::Run, vec![Reason::NewTest])
        );
    }

    #[test]
    fn an_expression_mismatch_is_fine_for_a_skip_or_a_user_filtered_run_only() {
        let (run, skip) = (tid("lib", "run"), tid("lib", "skip"));
        let plan = partial_plan(
            vec![entry(&run, Decision::Run), entry(&skip, Decision::Skip)],
            Some(vec![NextestTarget::Lib]),
        );
        let suites = || {
            vec![suite(
                "lib",
                "lib",
                "lib",
                &[
                    ("run", ListedMatch::Expression),
                    ("skip", ListedMatch::Expression),
                ],
            )]
        };
        // Sans `-E` utilisateur, notre filtre ne doit jamais exclure un test lancé.
        assert!(oracle(&plan, suites(), &JunitResults::new(), false).is_err());
        // Avec un `-E` utilisateur : `filtered-out` (réconciliation), saut normal.
        let observed = oracle(&plan, suites(), &JunitResults::new(), true).unwrap();
        assert_eq!(observed, Observed::default());
    }

    #[test]
    fn a_new_test_the_user_filter_excludes_is_journaled_filtered_out_never_absent() {
        let plan = partial_plan(vec![], Some(vec![NextestTarget::Lib]));
        let suites = || {
            vec![suite(
                "lib",
                "lib",
                "lib",
                &[("fresh", ListedMatch::Expression)],
            )]
        };
        let observed = oracle(&plan, suites(), &JunitResults::new(), true).unwrap();
        assert_eq!(
            observed.new_outcomes,
            BTreeMap::from([(tid("lib", "fresh"), Outcome::FilteredOut)])
        );
        // Sans filtre utilisateur : notre filtre n'exclut jamais un test neuf → erreur.
        let err = oracle(&plan, suites(), &JunitResults::new(), false).unwrap_err();
        assert!(err.contains("écarté par l'expression"), "{err}");
        // Réconciliation : entrée `new-test` lancée, résultat `filtered-out`.
        let end = End::Normal(JunitResults::new());
        let none = BTreeSet::from(["lib".to_owned()]);
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &BTreeSet::new(),
            user_filter: Some("test(zzz)"),
            end: &end,
            observed: &observed,
        });
        assert_eq!(
            outcome_of(&r, &tid("lib", "fresh")),
            Some(Outcome::FilteredOut)
        );
        assert_eq!(r.new.len(), 1);
        assert!(!r.any_not_run());
    }

    #[test]
    fn any_other_mismatch_reason_is_an_error() {
        let plan = partial_plan(vec![], Some(vec![NextestTarget::Lib]));
        let suites = vec![suite(
            "lib",
            "lib",
            "lib",
            &[("t", ListedMatch::Other("default-filter".to_owned()))],
        )];
        let err = oracle(&plan, suites, &JunitResults::new(), true).unwrap_err();
        assert!(err.contains("default-filter"), "{err}");
    }

    #[test]
    fn a_targeted_binary_missing_from_the_list_is_an_error() {
        let plan = partial_plan(
            vec![],
            Some(vec![
                NextestTarget::Lib,
                NextestTarget::Test("it".to_owned()),
            ]),
        );
        let lib_only = || vec![suite("lib", "lib", "lib", &[])];
        let err = oracle(&plan, lib_only(), &JunitResults::new(), false).unwrap_err();
        assert!(err.contains("Test(\"it\")"), "{err}");
        // La cible `--test it` est présente sous un autre `binary-id` que celui de la base :
        // le binaire de la base compilé par la cible manque alors.
        let renamed = vec![
            suite("lib", "lib", "lib", &[]),
            suite("other::it", "test", "it", &[]),
        ];
        let err = oracle(&plan, renamed, &JunitResults::new(), false).unwrap_err();
        assert!(err.contains("binaire it compilé"), "{err}");
        // Tout est là : accepté.
        let both = vec![
            suite("lib", "lib", "lib", &[]),
            suite("it", "test", "it", &[]),
        ];
        assert_eq!(
            oracle(&plan, both, &JunitResults::new(), false).unwrap(),
            Observed::default()
        );
    }

    #[test]
    fn a_whole_binary_skipped_by_nextest_is_accepted_only_with_a_user_filter() {
        let plan = partial_plan(vec![], Some(vec![NextestTarget::Lib]));
        let mut skipped = suite("lib", "lib", "lib", &[]);
        skipped.1.listed = false;
        assert!(oracle(&plan, vec![skipped.clone()], &JunitResults::new(), false).is_err());
        assert!(oracle(&plan, vec![skipped], &JunitResults::new(), true).is_ok());
    }

    #[test]
    fn the_full_suite_stage_checks_every_listed_binary_without_requiring_the_base_ones() {
        // Portée « tout » : aucune cible. Un binaire de la base supprimé n'est pas exigé.
        let a = tid("lib", "a");
        let plan = plan_of(vec![entry(&a, Decision::Run)]);
        let results = junit(&[("lib", "a", BaseStatus::Pass)]);
        let ok = vec![suite("lib", "lib", "lib", &[("a", ListedMatch::Matches)])];
        assert!(oracle(&plan, ok, &results, false).is_ok());
        // Un test listé sélectionné et absent du JUnit : erreur, aussi au stade « tout ».
        let bad = vec![suite(
            "lib",
            "lib",
            "lib",
            &[("a", ListedMatch::Matches), ("b", ListedMatch::Matches)],
        )];
        assert!(oracle(&plan, bad, &results, false).is_err());
    }

    #[test]
    fn an_unverified_end_keeps_the_results_but_trusts_no_missing_test() {
        let (a, b) = (tid("x", "a"), tid("x", "b"));
        let plan = plan_of(vec![entry(&a, Decision::Run), entry(&b, Decision::Run)]);
        let end = End::Unverified(
            junit(&[("x", "a", BaseStatus::Fail)]),
            "oracle de complétude : test".to_owned(),
        );
        let none = BTreeSet::new();
        let r = reconcile(&Input {
            plan: &plan,
            inventory_binaries: &none,
            ignored: &BTreeSet::from([b.clone()]),
            user_filter: Some("all()"),
            end: &end,
            observed: &Observed::default(),
        });
        assert_eq!(
            outcome_of(&r, &a),
            Some(Outcome::Failed),
            "un rouge reste rouge"
        );
        assert_eq!(outcome_of(&r, &b), Some(Outcome::NotRun));
        assert!(r.any_failed() && r.any_not_run());
    }
}
