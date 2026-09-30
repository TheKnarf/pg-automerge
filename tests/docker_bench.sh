#!/usr/bin/env bash
# tests/bench_sql.sh against the Docker image (run via `mise run
# docker-bench-sql`, which builds it first): the extension's release build
# on the PGDG Postgres 18 (no assertions), instead of the pgrx-managed
# Postgres (built with --enable-cassert and RANDOMIZE_ALLOCATED_MEMORY).
# Starts a throwaway container (Postgres' default settings, like the pgrx
# cluster's, shm-size 256 MB), runs bench_sql.sh in tests/lib.sh's
# external mode, prints its table, and removes the container. Not part of
# any test run.
#
# Env: see tests/docker_lib.sh and tests/bench_sql.sh (BENCH_DOCS,
# BENCH_REPS, BENCH_CASES, BENCH_SETTINGS; the extension is the image's).

# shellcheck source=tests/docker_lib.sh
source "$(dirname "${BASH_SOURCE[0]}")/docker_lib.sh"

log "benchmark container from $IMAGE" >&2
start_ready bench bench --shm-size=256m
mapfile -t ENV < <(external_env bench)
env "${ENV[@]}" bash tests/bench_sql.sh
