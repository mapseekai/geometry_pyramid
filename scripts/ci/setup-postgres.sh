#!/usr/bin/env bash
# Prepare a disposable GitHub Actions PostgreSQL/PostGIS installation.
set -euo pipefail

pg_major=${GP_PG_MAJOR:?Set GP_PG_MAJOR to a PostgreSQL major version}
case "$pg_major" in
    13|14|15|16|17|18) ;;
    *) echo "Unsupported PostgreSQL major version: $pg_major" >&2; exit 2 ;;
esac

work_root=${RUNNER_TEMP:-${TMPDIR:-/tmp}}
mkdir -p "$work_root"

persist_env() {
    local name=$1 value=$2
    export "$name=$value"
    if [[ -n ${GITHUB_ENV:-} ]]; then
        printf '%s=%s\n' "$name" "$value" >> "$GITHUB_ENV"
    fi
}

prepend_path() {
    export PATH="$1:$PATH"
    if [[ -n ${GITHUB_PATH:-} ]]; then
        printf '%s\n' "$1" >> "$GITHUB_PATH"
    elif [[ -n ${GITHUB_ENV:-} ]]; then
        printf 'PATH=%s\n' "$PATH" >> "$GITHUB_ENV"
    fi
}

setup_linux() {
    # The matrix uses native Ubuntu runners, including the arm64 runner.
    # PGDG supplies each PostgreSQL major and its matching PostGIS build.
    # shellcheck disable=SC1091
    . /etc/os-release
    if [[ ${ID:-} != ubuntu || -z ${VERSION_CODENAME:-} ]]; then
        echo "Linux setup expects an Ubuntu runner with VERSION_CODENAME." >&2
        exit 2
    fi

    sudo install -d -m 0755 /etc/apt/keyrings
    curl --fail --silent --show-error --location --retry 3 \
        https://www.postgresql.org/media/keys/ACCC4CF8.asc \
        --output "$work_root/geometry-pyramid-pgdg.asc"
    sudo gpg --dearmor --yes --output /etc/apt/keyrings/postgresql.gpg \
        "$work_root/geometry-pyramid-pgdg.asc"
    printf 'deb [arch=%s signed-by=/etc/apt/keyrings/postgresql.gpg] https://apt.postgresql.org/pub/repos/apt %s-pgdg main\n' \
        "$(dpkg --print-architecture)" "$VERSION_CODENAME" \
        | sudo tee /etc/apt/sources.list.d/pgdg.list >/dev/null
    sudo apt-get update
    sudo apt-get install --yes --no-install-recommends \
        build-essential clang libclang-dev pkg-config libssl-dev \
        "postgresql-$pg_major" "postgresql-server-dev-$pg_major" \
        "postgresql-$pg_major-postgis-3" "postgresql-$pg_major-postgis-3-scripts"

    PG_CONFIG="/usr/lib/postgresql/$pg_major/bin/pg_config"
    cargo pgrx init "--pg$pg_major" "$PG_CONFIG"

    # pgrx regression tests and package smoke tests install into this disposable
    # server. Only its extension directories need to be writable by the runner.
    sudo chown "$(id -u):$(id -g)" \
        "$("$PG_CONFIG" --pkglibdir)" "$("$PG_CONFIG" --sharedir)/extension"
}

setup_macos() {
    # Homebrew's postgis formula targets selected PostgreSQL majors. Build the
    # same PostGIS release against pgrx's selected PG13-PG18 installation instead.
    local formula prefix pkg_paths= include_flags= library_flags=
    local pg_root postgis_work source_archive
    local postgis_version=3.6.4
    # OSGeo source checksum recorded by Homebrew/homebrew-core commit
    # 2569dfb36eb87666972474bbf5054aa5742f683a, Formula/p/postgis.rb.
    local postgis_sha256=ed8dc6679f1e06f7b113592b04cde2a7e00f1b1e681294c8ca2204058990cec6

    export HOMEBREW_NO_AUTO_UPDATE=1
    brew install bison flex gettext icu4c openssl@3 readline zlib pkgconf \
        geos proj json-c libxml2 protobuf-c llvm

    for formula in bison flex gettext protobuf-c; do
        prepend_path "$(brew --prefix "$formula")/bin"
    done
    for formula in icu4c openssl@3 readline zlib geos proj json-c libxml2 protobuf-c; do
        prefix=$(brew --prefix "$formula")
        pkg_paths="${pkg_paths:+$pkg_paths:}$prefix/lib/pkgconfig"
    done
    persist_env PKG_CONFIG_PATH "$pkg_paths${PKG_CONFIG_PATH:+:$PKG_CONFIG_PATH}"
    persist_env LIBCLANG_PATH "$(brew --prefix llvm)/lib"

    # Both Intel and Apple Silicon use their actual Homebrew prefixes. In
    # particular, PG17+ needs Homebrew ICU's pkg-config metadata to configure.
    for formula in gettext openssl@3 readline zlib; do
        prefix=$(brew --prefix "$formula")
        include_flags="$include_flags -I$prefix/include"
        library_flags="$library_flags -L$prefix/lib"
    done
    persist_env CPPFLAGS "${CPPFLAGS:-}$include_flags"
    persist_env LDFLAGS "${LDFLAGS:-}$library_flags"

    cargo pgrx init "--pg$pg_major" download
    PG_CONFIG=$(cargo pgrx info pg-config "$pg_major")
    pg_root=$(dirname "$("$PG_CONFIG" --bindir)")
    prepend_path "$("$PG_CONFIG" --bindir)"

    postgis_work=$(mktemp -d "$work_root/geometry-pyramid-postgis.XXXXXX")
    source_archive="$postgis_work/postgis-$postgis_version.tar.gz"
    curl --fail --silent --show-error --location --retry 3 \
        "https://download.osgeo.org/postgis/source/postgis-$postgis_version.tar.gz" \
        --output "$source_archive"
    printf '%s  %s\n' "$postgis_sha256" "$source_archive" | shasum -a 256 --check
    tar -xzf "$source_archive" -C "$postgis_work"
    (
        cd "$postgis_work/postgis-$postgis_version"
        # The same prefix as PostgreSQL also gives Darwin's bundle loader the
        # right postgres executable. protobuf-c is required for ST_AsMVT.
        CXXFLAGS="${CXXFLAGS:-} -std=c++17" ./configure \
            --prefix="$pg_root" \
            --with-pgconfig="$PG_CONFIG" \
            --with-geosconfig="$(brew --prefix geos)/bin/geos-config" \
            --with-projdir="$(brew --prefix proj)" \
            --with-jsondir="$(brew --prefix json-c)" \
            --with-xml2config="$(brew --prefix libxml2)/bin/xml2-config" \
            --with-protobuf --with-protobufdir="$(brew --prefix protobuf-c)" \
            --without-raster --without-topology --without-sfcgal \
            --without-address-standardizer
        if ! grep -Eq '^#define[[:space:]]+HAVE_LIBPROTOBUF[[:space:]]+1' postgis_config.h; then
            echo "PostGIS was configured without the protobuf-c support required for MVT." >&2
            exit 1
        fi
        # PostGIS's recursive SQL-generation targets share temporary filenames.
        # Serialize the top-level build to avoid competing writers/deletions.
        make -j1
        make install
    )
    rm -rf -- "$postgis_work"
}

case "$(uname -s)" in
    Linux) setup_linux ;;
    Darwin) setup_macos ;;
    *) echo "Use setup-postgres.ps1 on Windows." >&2; exit 2 ;;
esac

actual_version=$("$PG_CONFIG" --version)
actual_major=${actual_version#PostgreSQL }
actual_major=${actual_major%%.*}
if [[ $actual_major != "$pg_major" ]]; then
    echo "Expected PostgreSQL $pg_major, found $actual_version at $PG_CONFIG." >&2
    exit 1
fi

ext_dir="$("$PG_CONFIG" --sharedir)/extension"
lib_dir=$("$PG_CONFIG" --pkglibdir)
if [[ ! -f $ext_dir/postgis.control || ! -w $ext_dir || ! -w $lib_dir ]]; then
    echo "PostGIS or writable extension directories are missing for $PG_CONFIG." >&2
    exit 1
fi

persist_env PG_CONFIG "$PG_CONFIG"
persist_env GP_PG_MAJOR "$pg_major"
persist_env PG_PG_MAJOR "$pg_major"
prepend_path "$("$PG_CONFIG" --bindir)"
printf 'Prepared %s with PostGIS; PG_CONFIG=%s\n' "$actual_version" "$PG_CONFIG"
