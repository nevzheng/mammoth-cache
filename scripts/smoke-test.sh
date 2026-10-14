#!/usr/bin/env bash
# Smoke-tests a built mammoth binary or image: the version it reports, a token it issues, and a node
# started from a config that uses that token, stores and serves one Gradle entry, and exits cleanly
# on SIGTERM. Run by release.yml on every binary and on the image before anything is published.
#
# usage: scripts/smoke-test.sh binary <path-to-mammoth> <expected-version>
#        scripts/smoke-test.sh image <image-ref> <expected-version>
#
# Needs bash, curl and (for an image) docker. Works with macOS's bash 3.2.
set -euo pipefail

mode=${1:?usage: smoke-test.sh binary|image <path-or-image> <expected-version>}
target=${2:?missing the binary path or image reference}
expected=${3:?missing the expected version}
port=15071
base="http://127.0.0.1:$port"

work=$(mktemp -d)
pid=""
container=""
cleanup() {
  [ -n "$pid" ] && kill -9 "$pid" 2>/dev/null || true
  [ -n "$container" ] && docker rm -f "$container" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

fail() {
  echo "smoke test failed: $*" >&2
  if [ -f "$work/node.log" ]; then
    echo "--- node log ---" >&2
    cat "$work/node.log" >&2
  fi
  if [ -n "$container" ]; then
    echo "--- container log ---" >&2
    docker logs "$container" >&2 || true
  fi
  exit 1
}

mammoth() {
  case "$mode" in
    binary) "$target" "$@" ;;
    image) docker run --rm "$target" "$@" ;;
    *) fail "unknown mode $mode" ;;
  esac
}

echo "== version"
version=$(mammoth --version) || fail "--version exited non-zero"
[ "$version" = "mammoth $expected" ] || fail "--version printed '$version', expected 'mammoth $expected'"
echo "$version"

echo "== token"
mammoth token new >"$work/token.txt" 2>/dev/null || fail "token new exited non-zero"
token=$(sed -n 's/^token: *//p' "$work/token.txt")
hash=$(sed -n 's/^hash: *//p' "$work/token.txt")
[ -n "$token" ] && [ -n "$hash" ] || fail "token new printed no token or hash"

case "$mode" in
  binary)
    listen="127.0.0.1:$port"
    disk="$work/disk"
    ;;
  image)
    listen="[::]:5071"
    disk="/data"
    ;;
esac
cat >"$work/mammoth.toml" <<EOF
version = 1
listen = "$listen"

[server]
shutdown_delay = "1s"
shutdown_timeout = "10s"

[disk]
path = "$disk"
size = "1GiB"
free_space_buffer = "16MiB"

[auth.users.smoke]
scopes = ["cache:read", "cache:write"]
token_hashes = ["$hash"]
EOF
# Readable by the image's nonroot user through the bind mount.
chmod 0644 "$work/mammoth.toml"

echo "== config check"
case "$mode" in
  binary) mammoth config check --config "$work/mammoth.toml" || fail "config check refused the config" ;;
  # The disk path only exists in the running container; start-up validates it there.
  image) ;;
esac

echo "== start"
case "$mode" in
  binary)
    "$target" --config "$work/mammoth.toml" >"$work/node.log" 2>&1 &
    pid=$!
    ;;
  image)
    container=$(docker run -d -p "127.0.0.1:$port:5071" \
      --tmpfs /data:uid=65532,gid=65532,mode=0700 \
      -v "$work/mammoth.toml:/etc/mammoth/mammoth.toml:ro" \
      "$target" --config /etc/mammoth/mammoth.toml) || fail "docker run failed"
    ;;
esac

ready=""
for _ in $(seq 1 60); do
  if curl -fsS "$base/readyz" >/dev/null 2>&1; then
    ready=yes
    break
  fi
  sleep 0.5
done
[ -n "$ready" ] || fail "/readyz did not answer 200 within 30s"

echo "== ping"
ping=$(curl -fsS "$base/ping") || fail "/ping failed"
[ "$ping" = "SUCCESS" ] || fail "/ping answered '$ping'"

echo "== store and fetch"
key=0123456789abcdef0123456789abcdef
head -c 65536 /dev/urandom >"$work/entry"
status=$(curl -sS -o /dev/null -w '%{http_code}' -X PUT -u "smoke:$token" \
  --data-binary "@$work/entry" "$base/cache/$key") || fail "PUT failed"
[ "$status" = "201" ] || fail "PUT answered $status, expected 201"
curl -fsS -u "smoke:$token" -o "$work/fetched" "$base/cache/$key" || fail "GET of the stored entry failed"
cmp -s "$work/entry" "$work/fetched" || fail "GET returned different bytes"
status=$(curl -sS -o /dev/null -w '%{http_code}' "$base/cache/$key") || fail "anonymous GET failed"
[ "$status" = "401" ] || fail "anonymous GET answered $status, expected 401"

echo "== shutdown"
case "$mode" in
  binary)
    kill -TERM "$pid"
    code=0
    wait "$pid" || code=$?
    pid=""
    ;;
  image)
    docker stop -t 30 "$container" >/dev/null
    code=$(docker inspect -f '{{.State.ExitCode}}' "$container")
    ;;
esac
[ "$code" = "0" ] || fail "exited with $code after SIGTERM"

echo "smoke test passed: $mode $target"
