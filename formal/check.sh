#!/usr/bin/env bash
# formal/check.sh — machine-check every formal artifact under formal/.
#
# Runs the Lean proofs (lake build) and the TLA+ models (TLC), asserting that the
# PASS models verify AND the boundary model fails exactly as designed (its forgery
# counterexample is the proof that k-of-n needs a Byzantine minority). Exit 0 iff
# every expectation is met.
#
# tla2tools.jar is resolved from $TLA2TOOLS_JAR, else downloaded to formal/.cache/.
# `lake` (Lean) must be on PATH (elan installs it under ~/.elan/bin).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"

echo "== Lean: lake build (proofs must compile — no sorry, no warnings) =="
( cd "$here/lean" && lake build )
echo "   OK"

JAR="${TLA2TOOLS_JAR:-$here/.cache/tla2tools.jar}"
if [ ! -f "$JAR" ]; then
  mkdir -p "$(dirname "$JAR")"
  echo "== fetching tla2tools.jar =="
  curl -fsSL -o "$JAR" \
    https://github.com/tlaplus/tlaplus/releases/latest/download/tla2tools.jar
fi

run_tlc() { # $1 = cfg ; captures output to $2
  ( cd "$here/tla" && java -cp "$JAR" tlc2.TLC -config "$1" AcCrdt.tla ) >"$2" 2>&1 || true
}

out="$(mktemp)"; trap 'rm -f "$out"' EXIT

echo "== TLA+: AcCrdt main model (must find NO error) =="
run_tlc AcCrdt.cfg "$out"
if grep -q "No error has been found" "$out"; then
  echo "   OK — NoForgery, Agreement, TypeOK, MonotoneStore all hold"
else
  echo "   FAIL: main model did not verify"; cat "$out"; exit 1
fi

echo "== TLA+: AcCrdt Byzantine-threshold model (must EXHIBIT the forgery) =="
run_tlc AcCrdt_ByzThreshold.cfg "$out"
if grep -q "Invariant ForgeryFree is violated" "$out"; then
  echo "   OK — boundary counterexample found (|Byzantine| = K forges), as designed"
else
  echo "   FAIL: boundary model did not exhibit the expected forgery"; cat "$out"; exit 1
fi

echo "ALL FORMAL CHECKS PASSED"
