#!/usr/bin/env bash
# Seed a running plane (dev sign-in enabled) with fake connectors, a project,
# a repository, a ticket and one rehearsal run, so the portal has something to show.
#
#   scripts/demo-seed.sh http://127.0.0.1:8080 /path/for/fake/repos
set -euo pipefail
B=${1:-http://127.0.0.1:8080}
ROOT=${2:-$PWD/.factory-data/demo/repos}
mkdir -p "$ROOT"
command -v cygpath >/dev/null && ROOT=$(cygpath -m "$ROOT")
S=$(curl -s -D - -o /dev/null -X POST "$B/auth/dev-login" -H 'content-type: application/json' -d '{"email":"admin@example.test"}' | grep -i x-factory-session | cut -d' ' -f2 | tr -d '\r')
j() { curl -sf -X "$1" "$B$2" -H "x-factory-session: $S" -H 'content-type: application/json' ${3:+-d "$3"}; }
id() { python -c "import sys,json;print(json.load(sys.stdin)['id'])"; }
j PUT /api/settings '{"tick_ms":1000,"heartbeat_secs":3,"poll_secs":5}' >/dev/null
T=$(j POST /api/connectors '{"kind":"fake_tickets","name":"Fake Jira"}' | id)
R=$(j POST /api/connectors "{\"kind\":\"fake_repo\",\"name\":\"Fake GitHub\",\"config\":{\"root\":\"$ROOT\"}}" | id)
C=$(j POST /api/connectors '{"kind":"fake_chat","name":"Fake Slack"}' | id)
j POST "/api/fake/$R/repos" '{"name":"acme/widgets","files":{"README.md":"# Widgets\n","package.json":"{}"}}' >/dev/null
P=$(j POST /api/projects "{\"key\":\"DEMO\",\"name\":\"Demo\",\"config\":{\"ticket_connector_id\":\"$T\",\"chat_connector_id\":\"$C\"}}" | id)
j POST "/api/projects/$P/repos" "{\"connector_id\":\"$R\",\"full_name\":\"acme/widgets\"}" >/dev/null
j POST "/api/fake/$T/tickets" '{"key":"DEMO-1","title":"Add a changelog","description":"Please add a changelog","status":"To Do","repo":"acme/widgets"}' >/dev/null
RUN=$(j POST /api/runs "{\"project_id\":\"$P\",\"workflow\":\"ticket_build\",\"ticket_key\":\"DEMO-1\"}" | id)
echo "run: $B/runs/$RUN"
echo "chat connector: $C   repo connector: $R"
