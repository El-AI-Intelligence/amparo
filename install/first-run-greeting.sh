#!/bin/sh
# Amparo first-run greeting — install-time asset (#173).
#
# Ships in the source tree (repo went public 2026-09-22) and with the
# installer assets on downloads.ellmstack.dev. The awakened power-on
# experience (#165) will fold this greeting into the product; until then the
# installer can place this script as the post-install first-run step.
#
# Greets with the operator's exact wording, reads the user's introduction,
# and keeps it beside the workspace so the later first-run wiring can pick
# it up. No LLM call, no telemetry, nothing leaves the machine.
set -eu

cat <<'AMPARO'
Greetings! My name is Amparo, built by EL AI Intelligence. Please tell me about yourself and what you would like for me to know?
AMPARO

if [ -t 0 ]; then
    workspace="${AMPARO_WORKSPACE:-$HOME/amparo-workspace}"
    mkdir -p "$workspace/.amparo"
    printf '%s\n' "You:"
    printf '%s\n' "—" >>"$workspace/.amparo/first-run-notes.txt" 2>/dev/null || true
    while IFS= read -r line; do
        [ -z "$line" ] && break
        printf '%s\n' "$line" >>"$workspace/.amparo/first-run-notes.txt"
    done
    printf '%s\n' "Noted — kept at $workspace/.amparo/first-run-notes.txt."
fi
exit 0
