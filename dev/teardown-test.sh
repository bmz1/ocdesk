#!/usr/bin/env bash
# Verifies the sidecar never outlives ocdesk. Linux + Xvfb; run from repo root
# after `cargo build`. Needs dev/mock_llm.py running for the "idle" scenario.
set -u
BIN=${BIN:-target/debug/ocdesk}
export DISPLAY=${DISPLAY:-:99}
export OPENCODE_CONFIG_CONTENT="$(cat dev/mock-opencode.json)"
WORK=$(mktemp -d)
export OCDESK_DIR=$WORK
pass=0; fail=0
# A killed orphan can linger as a zombie when PID 1 does not reap (containers);
# a zombie holds no resources, so it counts as gone.
alive() { local st; st=$(ps -o stat= -p "$1" 2>/dev/null); [ -n "$st" ] && [[ $st != Z* ]]; }

run() { # name, extra-env, action
  local name=$1 envs=$2 action=$3 log=$WORK/$1.log
  env $envs "$BIN" >"$log" 2>&1 &
  local app=$!
  for _ in $(seq 100); do grep -q "sidecar healthy" "$log" && break; sleep 0.1; done
  local sc; sc=$(sed 's/\x1b\[[0-9;]*m//g' "$log" | sed -n 's/.*sidecar spawned pid=\([0-9]*\).*/\1/p' | head -1)
  case $action in
    TERM|INT|KILL) sleep 0.5; kill -s "$action" "$app" ;;
    wait) ;;
  esac
  for _ in $(seq 100); do alive "$app" || break; sleep 0.1; done
  sleep 1
  if [ -n "$sc" ] && [ "$sc" != 0 ] && ! alive "$sc" && ! alive "$app"; then
    echo "PASS $name (app $app, sidecar $sc gone)"; pass=$((pass+1))
  else
    echo "FAIL $name (app $app alive=$(alive $app && echo y || echo n), sidecar '$sc' alive=$(alive ${sc:-0} && echo y || echo n))"; fail=$((fail+1))
    kill -9 "$app" "$sc" 2>/dev/null
  fi
}

run sigterm "" TERM
run sigint "" INT
run sigkill "" KILL
run panic "OCDESK_DEBUG_PANIC=1" wait
run quit-after-idle "OCDESK_PROBE_PROMPT=hi OCDESK_EXIT_ON_IDLE=1" wait
echo "passed=$pass failed=$fail (logs in $WORK)"
[ "$fail" -eq 0 ]
