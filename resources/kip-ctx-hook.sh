#!/bin/sh
# kip-ctx-hook: snapshots Claude Code statusline context for kip.
# stdin: statusline JSON from claude (called ~1/s). $1 (optional): the user's
# previous statusline command to chain; it receives the same stdin, its stdout
# goes back to claude untouched. POSIX sh only - /bin/sh may be bash or zsh.
# Any snapshot failure is silent: the user's statusline always runs.

PREV="${1:-}"
DIR="${HOME}/.kip/ctx"
IN="${DIR}/in.$$"

run_prev() {
    if [ -n "$PREV" ]; then
        # stdin is a one-shot pipe, already consumed by cat - replay it from
        # the buffer file (unlinked after redirect, the fd keeps it alive).
        if [ -f "$IN" ]; then
            exec 0< "$IN"
            rm -f "$IN"
        fi
        exec /bin/sh -c "$PREV"
    fi
    rm -f "$IN" 2>/dev/null
    exit 0
}

mkdir -p "${DIR}/by-sid" "${DIR}/by-pid" 2>/dev/null || run_prev
cat > "$IN" 2>/dev/null || run_prev

line=$(tr -d '\n\r' < "$IN" 2>/dev/null)
sid=$(printf '%s' "$line" | sed -n 's/.*"session_id"[[:space:]]*:[[:space:]]*"\([0-9a-fA-F-]\{8,64\}\)".*/\1/p')
# The context percentage is context_window.used_percentage. Claude 2.1.215+
# also puts used_percentage inside rate_limits (five_hour/seven_day); strip
# that block first so the greedy match cannot pick a rate-limit number instead.
ctx=$(printf '%s' "$line" | sed 's/"rate_limits".*//' \
    | sed -n 's/.*"used_percentage"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\(\.[0-9][0-9]*\)\{0,1\}\).*/\1/p')

# ctx 0 = no data yet (claude still booting), not an actual 0% session.
if [ -n "$sid" ] && [ -n "$ctx" ] && [ "$ctx" != "0" ] && [ "$ctx" != "0.0" ]; then
    ts=$(date +%s)
    snap="{\"session_id\":\"${sid}\",\"ctx\":${ctx},\"ts\":${ts}}"
    # $PPID is the claude process: it invokes this as a single command, so the
    # intermediate shell execs away.
    for f in "${DIR}/by-sid/${sid}.json" "${DIR}/by-pid/${PPID}.json"; do
        tmp="${f}.tmp.$$"
        if printf '%s' "$snap" > "$tmp" 2>/dev/null; then
            mv -f "$tmp" "$f" 2>/dev/null || rm -f "$tmp" 2>/dev/null
        fi
    done
fi

run_prev
