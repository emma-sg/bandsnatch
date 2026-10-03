#!/bin/sh
# Container supervisor for the bandsnatch one-shot CLI.
#
# Two modes:
#   * `schedule` (default) - run the sync repeatedly on a timer.
#   * a real subcommand    - passed straight through, so on-demand re-downloads
#                            work via `docker exec bandsnatch release <url>`.
set -eu

BIN=/usr/local/bin/bandsnatch

log() {
    printf '%s %s\n' "$(date -Iseconds)" "$*"
}

# Local time for an epoch, e.g. 2026-10-03T03:00:12+01:00.
at_epoch() {
    date -Iseconds -d "@$1"
}

# The same seconds as "12s", "4m32s" or "1h02m03s".
human_duration() {
    seconds=$1
    hours=$((seconds / 3600))
    minutes=$(((seconds % 3600) / 60))
    seconds=$((seconds % 60))
    if [ "$hours" -gt 0 ]; then
        printf '%sh%02dm%02ds' "$hours" "$minutes" "$seconds"
    elif [ "$minutes" -gt 0 ]; then
        printf '%sm%02ds' "$minutes" "$seconds"
    else
        printf '%ss' "$seconds"
    fi
}

# ---------------------------------------------------------------------------
# Pass-through: a named subcommand or flag makes this a plain CLI wrapper, so
# the schedule is skipped.
# ---------------------------------------------------------------------------
case "${1:-}" in
    run | release | debug-collection | -h | --help | -V | --version)
        exec "$BIN" "$@"
        ;;
esac

# ---------------------------------------------------------------------------
# Privilege dropping. With no PUID/PGID the container runs as root, which suits
# a rootless/userns setup but not a NAS bind mount.
# ---------------------------------------------------------------------------
: "${PUID:=}"
: "${PGID:=}"

drop_privs() {
    if [ -n "$PUID" ] || [ -n "$PGID" ]; then
        su-exec "${PUID:-0}:${PGID:-0}" "$@"
    else
        "$@"
    fi
}

prepare_dirs() {
    if [ -n "$PUID" ] || [ -n "$PGID" ]; then
        # The image already ships uid/gid 1000; addgroup/adduser are only needed
        # when the host user differs. -S keeps them system accounts with no shell.
        if [ -n "$PGID" ]; then
            addgroup -g "$PGID" -S bandsnatch-run 2>/dev/null || true
            GROUP_NAME=$(getent group "$PGID" | cut -d: -f1 || echo bandsnatch-run)
        else
            GROUP_NAME=bandsnatch
        fi
        if [ -n "$PUID" ]; then
            adduser -u "$PUID" -S -G "$GROUP_NAME" -H -s /sbin/nologin bandsnatch-run 2>/dev/null || true
        fi

        output="${BS_OUTPUT_FOLDER:-/music}"
        state="${BS_STATE:-$output/.bandsnatch-state.db}"

        # Create only the directories this script owns. The output folder is
        # expected to belong to PUID/PGID already; this ensures the state
        # directory exists for the state database and its lock file.
        mkdir -p "$output" "$(dirname "$state")"

        # Not recursive by default: a recursive chown across a large media share
        # can take minutes on every container start and wakes spun-down array
        # disks. Opt in only when ownership is wrong.
        chown "${PUID:-0}:${PGID:-0}" "$output" 2>/dev/null || true
        chown "${PUID:-0}:${PGID:-0}" "$(dirname "$state")" 2>/dev/null || true
        if [ "${CHOWN_RECURSIVE:-0}" = "1" ]; then
            log "CHOWN_RECURSIVE=1: recursively taking ownership of $output"
            chown -R "${PUID:-0}:${PGID:-0}" "$output" 2>/dev/null || true
        fi
    fi
}

# ---------------------------------------------------------------------------
# Scheduling
# ---------------------------------------------------------------------------
: "${RUN_ONCE:=0}"
: "${INTERVAL:=86400}"
: "${RUN_AT:=}"
: "${JITTER:=}"

# `sleep` is a child, so a bare `sleep` would leave PID 1 exiting on SIGTERM
# while the sleep kept running. Backgrounding it and `wait`ing lets the signal
# interrupt the wait and run the trap.
#
# The sync runs backgrounded for the same reason: a shell defers traps while a
# foreground child runs, so `docker stop` would never reach bandsnatch. It would
# run until Docker's grace period expires and SIGKILL kills it - possibly
# between the two renames of a directory swap, stranding a release. `exec` in
# `run_sync` makes `$run_pid` the bandsnatch process (or the su-exec that
# becomes it), so the forwarded signal arrives.
shutdown=0
run_pid=
trap 'shutdown=1; if [ -n "$run_pid" ]; then kill -TERM "$run_pid" 2>/dev/null || true; fi' TERM INT

interruptible_sleep() {
    sleep "$1" &
    wait $! || true
}

run_sync() {
    if [ -n "$PUID" ] || [ -n "$PGID" ]; then
        exec su-exec "${PUID:-0}:${PGID:-0}" "$BIN" run ${EXTRA_ARGS:-}
    fi
    exec "$BIN" run ${EXTRA_ARGS:-}
}

# Seconds until the next occurrence of local-time hour RUN_AT, plus optional
# jitter so a fleet of containers does not all hit Bandcamp on the hour.
seconds_until_hour() {
    now_h=$(date +%H)
    now_m=$(date +%M)
    now_s=$(date +%S)
    # 10# forces base 10 on the zero-padded fields from `date`.
    elapsed=$(( (10#$now_h * 3600) + (10#$now_m * 60) + (10#$now_s) ))
    target=$(( (10#$1 * 3600) ))
    delta=$(( target - elapsed ))
    if [ "$delta" -le 0 ]; then
        delta=$(( delta + 86400 ))
    fi
    if [ -n "$JITTER" ]; then
        delta=$(( delta + (RANDOM % JITTER) ))
    fi
    printf '%s' "$delta"
}

# Accept "3" or "03"; anything else is a mistake worth stopping for before the
# first run rather than after it.
validate_schedule() {
    if [ -n "$RUN_AT" ]; then
        case "$RUN_AT" in
            *[!0-9]*)
                log "RUN_AT must be an hour between 0 and 23 (got '$RUN_AT')"
                exit 1
                ;;
        esac
        if [ "$RUN_AT" -gt 23 ]; then
            log "RUN_AT must be an hour between 0 and 23 (got '$RUN_AT')"
            exit 1
        fi
    fi
}

describe_schedule() {
    if [ -n "$RUN_AT" ]; then
        # 10# reads the hour as base 10, so "08" is not taken as octal.
        hour=$((10#$RUN_AT))
        if [ -n "$JITTER" ]; then
            printf 'daily at %02d:00 local time, with up to %s seconds of jitter' "$hour" "$JITTER"
        else
            printf 'daily at %02d:00 local time' "$hour"
        fi
    else
        printf 'every %s seconds' "$INTERVAL"
    fi
}

next_delay() {
    if [ -n "$RUN_AT" ]; then
        seconds_until_hour "$RUN_AT"
    else
        printf '%s' "$INTERVAL"
    fi
}

prepare_dirs
validate_schedule

if [ "$RUN_ONCE" = "1" ]; then
    log "schedule: RUN_ONCE=1, one run and then exit"
else
    log "schedule: $(describe_schedule)"
fi

while :; do
    started=$(date +%s)
    log "run starting: $(drop_privs "$BIN" --version 2>/dev/null || echo bandsnatch) against ${BS_OUTPUT_FOLDER:-/music}"

    # BS_* variables are read by clap directly; EXTRA_ARGS carries flags with no
    # env binding, so it is word-split.
    run_sync &
    run_pid=$!
    status=0
    wait "$run_pid" || status=$?
    run_pid=
    elapsed=$(human_duration $(( $(date +%s) - started )))

    if [ "$shutdown" = "1" ]; then
        log "interrupted during a run after $elapsed (status $status)"
        break
    fi
    if [ "$status" = "0" ]; then
        log "run finished cleanly in $elapsed"
    else
        # A failed sync must not kill the container; the next tick retries.
        log "run failed after $elapsed with exit status $status"
    fi

    if [ "$RUN_ONCE" = "1" ]; then
        log "RUN_ONCE=1, exiting"
        break
    fi

    delay=$(next_delay)
    log "next run at $(at_epoch $(( $(date +%s) + delay ))) (waiting ${delay}s)"
    interruptible_sleep "$delay" || true

    if [ "$shutdown" = "1" ]; then
        log "shutting down"
        break
    fi
done
