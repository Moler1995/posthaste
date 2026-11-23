#!/bin/sh
# Launch a Posthaste build in an isolated workspace and require it to report
# its first rendered frame.
#
#   scripts/smoke-launch.sh target/debug/posthaste
#
# The launch uses a temporary workspace and redirected preferences, recents,
# and cookies, so it reads and writes no user data. It exits 0 only when the
# application itself exits successfully after reporting the frame; a failure
# prints the application's stderr.
set -eu

if [ "$#" -ne 1 ]; then
    echo "usage: $0 <posthaste-binary>" >&2
    exit 2
fi

binary=$1
if [ ! -x "$binary" ]; then
    echo "smoke launch: $binary is not an executable file" >&2
    exit 2
fi

root=$(mktemp -d "${TMPDIR:-/tmp}/posthaste-smoke.XXXXXX")
trap 'rm -rf "$root"' EXIT INT TERM

workspace=$root/workspace
mkdir -p "$workspace" "$root/cookies" "$root/preferences"
printf '{"schema_version":1,"name":"Smoke launch","vars":{}}\n' > "$workspace/workspace.json"

log=$root/stderr.log
POSTHASTE_WORKSPACE=$workspace \
POSTHASTE_PREFERENCES=$root/preferences \
POSTHASTE_COOKIE_DIR=$root/cookies \
POSTHASTE_SMOKE_EXIT_AFTER_FRAME=1 \
    "$binary" 2>"$log" &
pid=$!

# The application bounds its own wait; this bound only catches a wedged
# process that never exits at all.
waited=0
while kill -0 "$pid" 2>/dev/null; do
    if [ "$waited" -ge 120 ]; then
        kill "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
        echo "smoke launch: the application did not exit within 120 s" >&2
        sed 's/^/  /' "$log" >&2
        exit 1
    fi
    sleep 1
    waited=$((waited + 1))
done

status=0
wait "$pid" || status=$?

if [ "$status" -ne 0 ]; then
    echo "smoke launch: the application exited with status $status" >&2
    sed 's/^/  /' "$log" >&2
    exit 1
fi

if ! grep -q 'posthaste smoke: first frame rendered' "$log"; then
    echo "smoke launch: the first-frame confirmation is missing" >&2
    sed 's/^/  /' "$log" >&2
    exit 1
fi

grep 'posthaste smoke: first frame rendered' "$log"
