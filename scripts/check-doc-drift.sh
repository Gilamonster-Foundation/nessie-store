#!/usr/bin/env bash
# check-doc-drift.sh — assert README.md's crate inventory matches crates/ on disk.
#
# Durability guard (see docs "durable prose" pass): prose drifts, checks don't.
# The README's machine-readable crate inventory carries two marked regions:
#
#   <!-- crate-inventory:implemented BEGIN --> ... <!-- ...:implemented END -->
#   <!-- crate-inventory:planned     BEGIN --> ... <!-- ...:planned     END -->
#
# This script enforces three invariants so the doc physically cannot claim a
# capability the workspace does not have:
#   1. every crate listed as *implemented* exists as crates/<name>/
#   2. every crates/<name>/ on disk is listed as *implemented* (nothing undocumented)
#   3. no crate listed as *planned* exists on disk yet (promote it when it lands)
#
# Mirrored by `just doc-check`, the pre-push hook, and the ci.yml `docs-drift` job.
set -euo pipefail

cd "$(dirname "$0")/.."   # repo root
README="README.md"
fail=0

# Print the unique nessie-* crate names found inside a named inventory region.
extract_region() {
  awk -v key="$1" '
    $0 ~ ("crate-inventory:" key " BEGIN") { inreg = 1; next }
    $0 ~ ("crate-inventory:" key " END")   { inreg = 0 }
    inreg { print }
  ' "$README" | grep -oE 'nessie-[a-z0-9-]+' | sort -u
}

mapfile -t disk        < <(find crates -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort -u)
mapfile -t impl_doc    < <(extract_region implemented)
mapfile -t planned_doc < <(extract_region planned)

if [ "${#impl_doc[@]}" -eq 0 ]; then
  echo "doc-drift: no 'implemented' crate-inventory region found in $README" >&2
  exit 1
fi

# 1. documented-implemented ⊆ disk
for c in "${impl_doc[@]}"; do
  if [ ! -d "crates/$c" ]; then
    echo "doc-drift: README lists '$c' as implemented, but crates/$c does not exist" >&2
    fail=1
  fi
done

# 2. disk ⊆ documented-implemented
for c in "${disk[@]}"; do
  if ! printf '%s\n' "${impl_doc[@]}" | grep -qxF "$c"; then
    echo "doc-drift: crates/$c exists but is not in the README implemented inventory" >&2
    fail=1
  fi
done

# 3. planned ∩ disk = ∅
for c in "${planned_doc[@]}"; do
  if [ -d "crates/$c" ]; then
    echo "doc-drift: README lists '$c' as planned, but crates/$c now exists — move it to implemented" >&2
    fail=1
  fi
done

if [ "$fail" -ne 0 ]; then
  echo "doc-drift: README crate inventory is out of sync with crates/ (see above)" >&2
  exit 1
fi
echo "doc-drift: README crate inventory matches crates/ (${#disk[@]} implemented, ${#planned_doc[@]} planned)"
