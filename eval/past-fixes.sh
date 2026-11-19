#!/bin/bash
# Would tmt have pointed at the code a bug fix later had to change?
#
# For every fix commit C that touches the source of a given package, check out C^ (the
# code and the tests as they were before the fix) and run tmt on the lines that C changes.
# A finding or unreached code on those lines means the tests of the time left room for
# the bug there. Nothing the fix adds is shown to the tool.
#
# usage: past-fixes.sh <clone of the repository> <output directory> <tmt binary> <file with "commit package" lines>
set -u
clone=$1; out=$2; tmt=$3; list=$4
mkdir -p "$out"
summary="$out/summary.tsv"
printf 'commit\tpackage\texit\tsites\tinput_gaps\tassertion_gaps\tunreached\tseconds\tsubject\n' > "$summary"
while read -r commit package; do
  [ -z "$commit" ] && continue
  git -C "$clone" checkout -q --detach "${commit}^" || { echo "cannot check out ${commit}^" >&2; continue; }
  subject=$(git -C "$clone" log -1 --format=%s "$commit" | cut -c1-90)
  started=$(date +%s)
  "$tmt" "$clone" --package "$package" --since "$commit" --json --quiet \
      --work-dir "$out/work" > "$out/$commit.json" 2> "$out/$commit.err"
  code=$?
  seconds=$(( $(date +%s) - started ))
  python3 - "$out/$commit.json" "$commit" "$package" "$code" "$seconds" "$subject" >> "$summary" <<'PY'
import json, sys
path, commit, package, code, seconds, subject = sys.argv[1:7]
try:
    report = json.load(open(path))
    kinds = [f["kind"] for f in report["findings"]]
    row = [report["sites"], kinds.count("input_gap"), kinds.count("assertion_gap"), len(report["unreached"])]
except Exception:
    row = ["-", "-", "-", "-"]
print("\t".join([commit, package, code] + [str(x) for x in row] + [seconds, subject]))
PY
  tail -1 "$summary"
done < "$list"
