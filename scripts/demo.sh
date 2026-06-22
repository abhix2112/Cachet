#!/usr/bin/env bash
#
# Demo-staging traffic for recording the Cachet dashboard GIF.
#
# Drives a realistic, paced mix against a running Cachet so the live feed shows
# misses, exact hits, semantic hits (rephrasings), and a streaming hit — while the
# "$ saved" counter climbs. Pricing and token counts are REAL (Cachet's built-in
# table + chars/4 on the served answer); the number is large only because the demo
# upstream returns paragraph-length answers and we send a realistic volume of hits.
#
# Usage:
#   1. Start the mock upstream:   python3 scripts/demo_upstream.py
#   2. Start Cachet against it:   CACHET_UPSTREAM=http://127.0.0.1:9999 ./target/release/cachet
#   3. Open http://localhost:8080/__cachet/  and start recording
#   4. Run this script:           ./scripts/demo.sh
#
# Real-key mode (hits OpenAI, costs real money): export OPENAI_API_KEY=sk-... and run
# Cachet with no CACHET_UPSTREAM (defaults to OpenAI). This script auto-adds the auth
# header when OPENAI_API_KEY is set.
#
# Tunables: CACHET (default http://localhost:8080), CACHET_DEMO_MODEL (gpt-4o),
#           CACHET_DEMO_ROUNDS (4).

set -u
CACHET="${CACHET:-http://localhost:8080}"
MODEL="${CACHET_DEMO_MODEL:-gpt-4o}"
ROUNDS="${CACHET_DEMO_ROUNDS:-4}"
URL="$CACHET/v1/chat/completions"

AUTH=()
if [ -n "${OPENAI_API_KEY:-}" ]; then
  AUTH=(-H "Authorization: Bearer $OPENAI_API_KEY")
  echo "[demo] real-key mode: sending Authorization header"
else
  echo "[demo] mock mode: no API key (expects the demo upstream behind Cachet)"
fi

# Original prompts (cold misses) and a higher-overlap rephrasing of each (semantic hits).
seeds=(
  "What is the capital of France?"
  "How do I reverse a string in Python?"
  "Explain what the Rust borrow checker does."
  "What is the difference between TCP and UDP?"
  "How does HTTPS keep my data secure?"
  "What is a database index and why does it help?"
)
rephrasings=(
  "Which city is the capital of France?"
  "In Python, how do I reverse a string?"
  "Explain what the borrow checker does in Rust."
  "What is the difference between UDP and TCP?"
  "How does HTTPS secure my data?"
  "Why does a database index help?"
)

ask() { # $1=content  $2=stream(true/false)
  local content="$1" stream="${2:-false}"
  local payload
  payload=$(printf '{"model":"%s","stream":%s,"messages":[{"role":"user","content":"%s"}]}' "$MODEL" "$stream" "$content")
  curl -sN -o /dev/null "${AUTH[@]}" -H "Content-Type: application/json" -d "$payload" "$URL"
}

echo "[demo] phase 1 — cold misses (seeding the cache)"
for s in "${seeds[@]}"; do ask "$s" false; sleep 0.5; done

echo "[demo] phase 2 — $ROUNDS rounds of exact + semantic hits"
for ((r = 1; r <= ROUNDS; r++)); do
  for i in "${!seeds[@]}"; do
    ask "${seeds[$i]}" false;        sleep 0.22   # exact hit
    ask "${rephrasings[$i]}" false;  sleep 0.22   # semantic hit
  done
done

echo "[demo] phase 3 — streaming miss, then streaming hit"
ask "Tell me the story of how coffee was discovered." true; sleep 0.5
ask "Tell me the story of how coffee was discovered." true; sleep 0.5

echo "[demo] done — watch the dashboard's \$ saved and hit rate settle."
