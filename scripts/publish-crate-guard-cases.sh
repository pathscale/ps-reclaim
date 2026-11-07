#!/usr/bin/env bash
# Local-only guard harness. All external commands are isolated stubs; cargo
# publish is recorded by a stub; no real registry operation is performed.
set -euo pipefail

repo_root=$(cd "$(dirname "$0")/.." && pwd)
helper="$repo_root/scripts/publish-crate.sh"
: "${PUBLISH_TEST_SCRATCH_ROOT:?set an existing owned scratch directory}"
tmp_root=$(mktemp -d "$PUBLISH_TEST_SCRATCH_ROOT/reclaim-guards.XXXXXX")
trap 'rm -rf "$tmp_root"' EXIT
printf 'Owner: publisher guard harness. Delete on harness exit.\n' > "$tmp_root/OWNER.md"
mkdir -p "$tmp_root/bin"

cat > "$tmp_root/bin/git" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
  rev-parse) printf '%s\n' '0123456789abcdef0123456789abcdef01234567' ;;
  status)
    count_file=${STATUS_COUNT_FILE:?}
    count=0
    [ ! -f "$count_file" ] || read -r count < "$count_file"
    count=$((count + 1))
    printf '%s\n' "$count" > "$count_file"
    if [ "${STATUS_FAILURE_AT:-}" = "$count" ]; then exit 19; fi
    printf '%s' "${STATUS_OUTPUT:-}"
    ;;
  *) exit 90 ;;
esac
STUB
cat > "$tmp_root/bin/cargo" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
  read-manifest) printf '%s\n' '{"name":"ps-reclaim","version":"0.1.5","publish":["crates-io"]}' ;;
  publish) printf 'publish-called\n' >> "$PUBLISH_MARKER" ;;
  *) exit 91 ;;
esac
STUB
cat > "$tmp_root/bin/jq" <<'STUB'
#!/usr/bin/env bash
case "${1:-}" in
  -er)
    case "${2:-}" in
      '.name | select(type == "string" and length > 0)') printf 'ps-reclaim\n' ;;
      '.version | select(type == "string" and length > 0)') printf '0.1.5\n' ;;
      *) exit 92 ;;
    esac
    ;;
  -e) [ "${2:-}" = '(.publish | type) == "array" and (.publish | index("crates-io") != null)' ] ;;
  -nr) case "${2:-}" in
      --arg) case "${3:-}" in
        name) printf 'ps-reclaim\n' ;;
        version) printf '0.1.5\n' ;;
        *) exit 93 ;;
      esac ;;
      *) exit 94 ;;
    esac ;;
  *) exit 95 ;;
esac
STUB
cat > "$tmp_root/bin/curl" <<'STUB'
#!/usr/bin/env bash
[ "${1:-}" = --disable ] || { echo 'curl --disable was not first' >&2; exit 96; }
args="$*"
case "$args" in *'--proto =https'*) ;; *) echo 'HTTPS protocol restriction missing' >&2; exit 97 ;; esac
case "$args" in *'--location'*) echo 'redirect following is forbidden' >&2; exit 98 ;; esac
case "$args" in *'https://crates.io/api/v1/crates/ps-reclaim/0.1.5'*) ;; *) echo 'unexpected probe URL' >&2; exit 99 ;; esac
[ "${CURL_MODE:-}" != transport ] || exit 7
printf '%s' "${CURL_STATUS:-200}"
STUB
chmod +x "$tmp_root/bin/"*

run_case() {
  local name=$1 expected=$2 mode=$3 http=$4 fail_at=$5
  local marker="$tmp_root/$name.publish" status_count="$tmp_root/$name.status-count" output="$tmp_root/$name.out" rc=0
  : > "$marker"
  rm -f "$status_count"
  if PATH="$tmp_root/bin:$PATH" \
    GITHUB_REF=refs/heads/master LOCAL_CHECKS_ATTESTED=yes \
    CARGO_REGISTRY_TOKEN=stub EXPECTED_COMMIT=0123456789abcdef0123456789abcdef01234567 \
    PUBLISH_MARKER="$marker" STATUS_COUNT_FILE="$status_count" \
    CURL_MODE="$mode" CURL_STATUS="$http" STATUS_FAILURE_AT="$fail_at" \
    bash "$helper" >"$output" 2>&1; then rc=0; else rc=$?; fi
  case "$expected" in
    success) [ "$rc" -eq 0 ] || { cat "$output"; return 1; } ;;
    failure) [ "$rc" -ne 0 ] || { cat "$output"; return 1; } ;;
    *) return 1 ;;
  esac
  if [ "$name" = direct404 ]; then
    [ -s "$marker" ] || { echo '404 did not select the publish call' >&2; return 1; }
  else
    [ ! -s "$marker" ] || { echo "$name unexpectedly selected publish" >&2; return 1; }
  fi
  printf 'PASS %s (exit %s)\n' "$name" "$rc"
}

run_case status_failure_first failure none 200 1
run_case status_failure_second failure none 404 2
run_case redirect failure none 302 ''
run_case transport failure transport 000 ''
run_case direct200 success none 200 ''
run_case direct404 success none 404 ''
