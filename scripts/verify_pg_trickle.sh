#!/usr/bin/env bash
set -euo pipefail

readonly EXPECTED_REPOSITORY="MetroTraffic/pg-trickle"
readonly EXPECTED_REVISION="048c180e0b5e83a0f2214f3eabd7d069b6abea49"
readonly EXPECTED_VERSION="0.82.0"

repo_root="$(git rev-parse --show-toplevel)"

if [[ -n "${PG_TRICKLE_DIR:-}" ]]; then
    pg_trickle_dir="${PG_TRICKLE_DIR}"
elif git -C "${repo_root}/../pg-trickle" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    pg_trickle_dir="${repo_root}/../pg-trickle"
else
    pg_trickle_dir="${repo_root}/vendor/pg-trickle"
fi

if ! git -C "${pg_trickle_dir}" rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    echo "pg-trickle checkout not found at ${pg_trickle_dir}" >&2
    echo "initialize vendor/pg-trickle or set PG_TRICKLE_DIR" >&2
    exit 1
fi

origin_url="$(git -C "${pg_trickle_dir}" remote get-url origin)"
case "${origin_url}" in
    https://github.com/MetroTraffic/pg-trickle|https://github.com/MetroTraffic/pg-trickle.git|git@github.com:MetroTraffic/pg-trickle|git@github.com:MetroTraffic/pg-trickle.git)
        ;;
    *)
        echo "unexpected pg-trickle origin: ${origin_url}" >&2
        echo "expected GitHub repository ${EXPECTED_REPOSITORY}" >&2
        exit 1
        ;;
esac

actual_revision="$(git -C "${pg_trickle_dir}" rev-parse HEAD)"
if [[ "${actual_revision}" != "${EXPECTED_REVISION}" ]]; then
    echo "unexpected pg-trickle revision: ${actual_revision}" >&2
    echo "expected ${EXPECTED_REVISION}" >&2
    exit 1
fi

actual_version="$(sed -n 's/^version = "\([^"]*\)"/\1/p' "${pg_trickle_dir}/Cargo.toml" | head -n 1)"
if [[ "${actual_version}" != "${EXPECTED_VERSION}" ]]; then
    echo "unexpected pg-trickle version: ${actual_version:-<missing>}" >&2
    echo "expected ${EXPECTED_VERSION}" >&2
    exit 1
fi

declare -a required_api=(
    "fn create_stream_table("
    "fn drop_stream_table("
    "fn refresh_stream_table("
)

for symbol in "${required_api[@]}"; do
    if ! grep -R -F -q --include='*.rs' "${symbol}" "${pg_trickle_dir}/src/api"; then
        echo "pinned pg-trickle checkout is missing required API symbol: ${symbol}" >&2
        exit 1
    fi
done

printf 'pg-trickle verified: %s@%s (v%s)\n' \
    "${EXPECTED_REPOSITORY}" "${EXPECTED_REVISION}" "${EXPECTED_VERSION}"