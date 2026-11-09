#!/usr/bin/env bash
# Use a disposable PostgreSQL installation whose extension directories are writable.
set -euo pipefail

if [[ $# != 2 ]]; then
    echo "Usage: bash tests/package_smoke.sh CANDIDATE_PACKAGE LEGACY_PACKAGE" >&2
    exit 2
fi

pg_config=${PG_CONFIG:?Set PG_CONFIG to a disposable PostgreSQL installation}
bindir=$("$pg_config" --bindir)
libdir=$("$pg_config" --pkglibdir)
extdir=$("$pg_config" --sharedir)/extension
test_dir=$(dirname "${BASH_SOURCE[0]}")
suffix=so
if [[ $(uname -s) == Darwin ]]; then
    suffix=dylib
fi

install_package() {
    local package=$1
    install -m 755 "$package$libdir/geometry_pyramid.$suffix" "$libdir/"
    install -m 644 "$package$extdir/"geometry_pyramid* "$extdir/"
}

cluster=$(mktemp -d /tmp/gp-package.XXXXXX)
cleanup() {
    local status=$?
    if (( status != 0 )) && [[ -f "$cluster/server.log" ]]; then
        tail -n 40 "$cluster/server.log" >&2
    fi
    if [[ -f "$cluster/data/postmaster.pid" ]]; then
        "$bindir/pg_ctl" -D "$cluster/data" -m fast -w stop || exit 1
    fi
    rm -rf -- "$cluster"
}
trap cleanup EXIT
export PGHOST=$cluster PGPORT=25432 PGDATABASE=postgres PGUSER
PGUSER=$(id -un)

"$bindir/initdb" -D "$cluster/data" --auth=trust --no-locale -E UTF8
"$bindir/pg_ctl" -D "$cluster/data" -l "$cluster/server.log" \
    -o "-k $cluster -p $PGPORT -c listen_addresses='' -c jit=off" -w start
"$bindir/createdb" gp_upgrade

install_package "$2"
"$bindir/psql" -X -d gp_upgrade -v ON_ERROR_STOP=1 -v seed=true -f "$test_dir/package_upgrade.sql"
# The seed connection has exited, so the next backend loads the candidate library.
install_package "$1"
"$bindir/psql" -X -d gp_upgrade -v ON_ERROR_STOP=1 -v seed=false -f "$test_dir/package_upgrade.sql"

"$bindir/psql" -X -v ON_ERROR_STOP=1 -c 'CREATE EXTENSION postgis; CREATE EXTENSION geometry_pyramid'
"$bindir/psql" -X -v ON_ERROR_STOP=1 -f "$test_dir/session_safety.sql"
