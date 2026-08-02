#!/usr/bin/env bash
# Record a narrated demo to an animated GIF: asciinema captures the rich-Console
# run, agg renders it. Requires a server already running + snapshot imported
# (run_case_study.sh handles that). vhs is intentionally not used (asciinema+agg
# is the pipeline proven on the powergrid/telecom demos).
#
#   record_gif.sh <demo.py> <out.gif> <lib_dir>
set -uo pipefail

DEMO="${1:?demo.py}"; OUT="${2:?out.gif}"; LIB_DIR="${3:?lib dir}"
CAST="${OUT%.gif}.cast"

for t in asciinema agg; do
  command -v "$t" >/dev/null || { echo "[gif] '$t' not installed — skipping GIF"; exit 0; }
done

export PYTHONPATH="$LIB_DIR${PYTHONPATH:+:$PYTHONPATH}"
export SG_BASE_URL="${SG_BASE_URL:-http://127.0.0.1:8080}"
export SG_GRAPH="${SG_GRAPH:-default}"

echo "[gif] recording $DEMO → $CAST"
# Long-form: record a TALL terminal so the whole narrated report renders in one
# vertical image with nothing scrolling off — the format used by the lea-triage
# demo. asciinema 3.x ignores COLUMNS/LINES and needs --window-size, so set both.
# GIF_ROWS tunes the height per case study (enough rows for the full report);
# PYTHON lets a venv with rich/requests be used instead of the system python3.
# Record idle up to 6s so the demo's read-pauses survive into the cast.
ROWS="${GIF_ROWS:-120}"
PYTHON="${PYTHON:-python3}"
rm -f "$CAST"
COLUMNS=100 LINES="$ROWS" TERM=xterm-256color asciinema rec --overwrite -q -i 6 \
  --window-size "100x${ROWS}" \
  -c "env COLUMNS=100 LINES=$ROWS TERM=xterm-256color $PYTHON $DEMO" "$CAST" \
  || { echo "[gif] asciinema failed"; exit 1; }

echo "[gif] rendering $CAST → $OUT"
# speed 1.0 (real-time) + a 4s idle cap so the post-result read pauses render in
# full — a looping GIF can't be paused, so the reading time must be in the frames.
# --last-frame-duration holds the completed report for a few seconds before the
# GIF loops, so the final infographic is readable without freezing forever.
agg --speed 1.0 --idle-time-limit 4.0 --last-frame-duration 8 \
  --font-size 18 --theme asciinema "$CAST" "$OUT" \
  || { echo "[gif] agg failed"; exit 1; }

SZ=$(du -h "$OUT" | cut -f1)
echo "[gif] wrote $OUT ($SZ)"
