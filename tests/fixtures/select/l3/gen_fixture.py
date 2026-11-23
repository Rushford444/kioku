#!/usr/bin/env python3
"""Regenere les fixtures L3 depuis les mesures de la tranche T1 (kioku 941cec0).

Entrees : graph_raw.json (produit par scip_graph.py : aretes SCIP + #[path] + racines de test)
          et cov/lines.json (couverture par test, cov_run.py).
Sorties : kioku-941cec0-graph.json  {edges, test_roots}  (cle de test = "binaire nom")
          kioku-941cec0-coverage.json  meme format que cov/lines.json, lignes reduites a la
          premiere ligne executee (le contenu des lignes ne sert pas a `learn`).
Usage : gen_fixture.py GRAPH_RAW LINES_JSON OUT_DIR
"""
import json
import sys

raw, lines, out = sys.argv[1:4]
g = json.load(open(raw))
json.dump(
    {"edges": g["edges"], "test_roots": g["test_roots_raw"]},
    open(f"{out}/kioku-941cec0-graph.json", "w"),
    indent=0,
    sort_keys=True,
)
cov = json.load(open(lines))
slim = {t: {f: v[:1] for f, v in sorted(fs.items()) if v} for t, fs in sorted(cov.items())}
json.dump(slim, open(f"{out}/kioku-941cec0-coverage.json", "w"), indent=0)
