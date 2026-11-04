#!/usr/bin/env bash
# crawllab bench: spider driving obscura over CDP, against local fixtures, all
# in containers on one private network. Exits non-zero if any check fails.
#
#   OBSCURA_BIN_DIR=/dir/with/linux/obscura ./run.sh
#   OBSCURA_PROFILE=/path/device.json   optional: run obscura with a device profile
#   CDP_PORT=19222                      optional: host port for the CDP relay
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
: "${OBSCURA_BIN_DIR:?set OBSCURA_BIN_DIR to a directory holding a Linux obscura binary built with --features stealth}"
port=${CDP_PORT:-19222}
net=crawllab-bench-net
work=$(mktemp -d)

cleanup() {
  docker rm -f cb-echo cb-anubis cb-obscura cb-relay >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
cleanup
work=$(mktemp -d)

ip() { docker inspect -f "{{(index .NetworkSettings.Networks \"$net\").IPAddress}}" "$1"; }

docker network create "$net" >/dev/null
docker run -d --name cb-echo --network "$net" --user 1000:1000 \
  -v "$here/fixtures":/srv:ro python:3.12-slim python -I /srv/echo.py >/dev/null
docker run -d --name cb-anubis --network "$net" -v "$here/fixtures":/cfg:ro \
  -e BIND=:8923 -e TARGET=http://cb-echo:8000 -e POLICY_FNAME=/cfg/anubis-policy.yaml \
  -e COOKIE_SECURE=false -e COOKIE_SAME_SITE=Lax -e USE_REMOTE_ADDRESS=true \
  ghcr.io/techarohq/anubis:v1.28.2 >/dev/null

profile_args=()
if [ -n "${OBSCURA_PROFILE:-}" ]; then
  profile_args=(-e OBSCURA_PROFILE_FILE=/profile/device.json -v "$OBSCURA_PROFILE":/profile/device.json:ro)
fi
docker run -d --name cb-obscura --network "$net" --user 1000:1000 -e HOME=/tmp \
  -e RUST_LOG=obscura_cdp=debug -e OBSCURA_ALLOW_PRIVATE_NETWORK=1 "${profile_args[@]}" \
  -p "127.0.0.1:$port:$port" -v "$OBSCURA_BIN_DIR":/opt/obscura:ro debian:bookworm-slim \
  /opt/obscura/obscura serve --stealth --host 127.0.0.1 --port "$port" >/dev/null
# obscura binds loopback inside its container; relay the published port to it.
docker run -d --name cb-relay --network container:cb-obscura alpine/socat \
  "TCP-LISTEN:$port,bind=$(ip cb-obscura),fork,reuseaddr" "TCP:127.0.0.1:$port" >/dev/null
for _ in $(seq 1 100); do
  curl -s -m 1 "http://127.0.0.1:$port/json/version" >/dev/null && break
done

export CDP="http://127.0.0.1:$port/json/version"
export ANUBIS="http://$(ip cb-anubis):8923"
export ECHO="http://$(ip cb-echo):8000"
export ECHO_HOST="http://cb-echo:8000"

# The challenge page is captured at run time, never committed: Anubis embeds
# the client's IP in it.
docker run --rm --network "$net" curlimages/curl:8.11.1 -s --compressed \
  -A "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/146.0.0.0 Safari/537.36" \
  http://cb-anubis:8923/fast >"$work/challenge.html"
export CHALLENGE_HTML="$work/challenge.html"

runner=(cargo run -q --manifest-path "$here/runner/Cargo.toml" --)
status=0
for check in challenge-page refresh dns anubis httponly; do
  "${runner[@]}" "$check" || status=1
done

# Two crawls sharing a cookie jar: Anubis challenges only the first.
issued() { docker logs cb-anubis 2>&1 | grep -c "new challenge issued" || true; }
before=$(issued)
"${runner[@]}" session || status=1
after=$(issued)
if [ "$((after - before))" = 1 ]; then
  echo "PASS one challenge across two crawls sharing a jar"
else
  echo "FAIL $((after - before)) challenges across two crawls sharing a jar"
  status=1
fi

# The runner turns PointerAssist off: no synthetic mouse events may reach the browser.
before=$(docker logs cb-obscura 2>&1 | grep -c "Input.dispatchMouseEvent" || true)
"${runner[@]}" fetch "$ECHO/content" || status=1
after=$(docker logs cb-obscura 2>&1 | grep -c "Input.dispatchMouseEvent" || true)
if [ "$after" = "$before" ]; then
  echo "PASS no synthetic mouse events"
else
  echo "FAIL $((after - before)) synthetic mouse events"
  status=1
fi

# Every request that reached the backend carried one user agent and platform.
docker logs cb-echo 2>&1 | python3 -I -c '
import json, sys
seen = {(r["ua"], r["sec-ch-ua-platform"]) for r in map(json.loads, (l for l in sys.stdin if l.startswith("{\"port"))) if r["ua"]}
ok = len(seen) == 1
print(("PASS" if ok else "FAIL"), "one identity across", "requests:", sorted(seen))
sys.exit(0 if ok else 1)
' || status=1

[ "$status" = 0 ] && echo "bench: all checks passed" || echo "bench: FAILED"
exit "$status"
