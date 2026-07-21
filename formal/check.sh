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

run_tlc() { # $1 = module ; $2 = cfg ; captures output to $3
  # A unique metadir per run: TLC's default is timestamp-based and collides when
  # several models run within the same second.
  local md; md="$(mktemp -d)"
  ( cd "$here/tla" && java -cp "$JAR" tlc2.TLC -metadir "$md" -config "$2" "$1.tla" ) >"$3" 2>&1 || true
  rm -rf "$md"
}

out="$(mktemp)"; trap 'rm -f "$out"' EXIT

# A model that must verify with no error.
expect_pass() { # $1 module ; $2 cfg ; $3 description
  echo "== TLA+: $3 (must find NO error) =="
  run_tlc "$1" "$2" "$out"
  if grep -q "No error has been found" "$out"; then
    echo "   OK"
  else
    echo "   FAIL: $1/$2 did not verify"; cat "$out"; exit 1
  fi
}

# A boundary model that must EXHIBIT a specific invariant violation.
expect_violation() { # $1 module ; $2 cfg ; $3 invariant ; $4 description
  echo "== TLA+: $4 (must EXHIBIT the violation) =="
  run_tlc "$1" "$2" "$out"
  if grep -q "Invariant $3 is violated" "$out"; then
    echo "   OK — boundary counterexample found, as designed"
  else
    echo "   FAIL: $1/$2 did not violate $3 as expected"; cat "$out"; exit 1
  fi
}

# --- AcCrdt (PO-AC-*) ---
expect_pass      AcCrdt AcCrdt.cfg             "AcCrdt main (NoForgery/Agreement/TypeOK/MonotoneStore)"
expect_violation AcCrdt AcCrdt_ByzThreshold.cfg ForgeryFree \
                 "AcCrdt |Byzantine| = K forges"

# --- Gc (PO-GC-1-op: the put->reference race guard) ---
expect_pass      Gc Gc.cfg                     "Gc guarded (InflightProtected/RootsStored)"
expect_violation Gc Gc_Unguarded.cfg InflightProtected \
                 "Gc unguarded sweeps an in-flight blob"

# --- Eviction (PO-GC-2 + PO-GC-2-B: no reachable blob lost swarm-wide) ---
expect_pass      Eviction Eviction.cfg         "Eviction gated + durable (NoReachableLost)"
expect_violation Eviction Eviction_Unsafe.cfg NoReachableLost \
                 "Eviction ungated pure-cache loses a blob"

echo "ALL FORMAL CHECKS PASSED"
