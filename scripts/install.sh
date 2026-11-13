#!/usr/bin/env bash
# Install a verified release bundle into the PostgreSQL selected by pg_config.
# Compatible with the Bash 3.2 supplied by macOS. No database SQL is executed.
set -euo pipefail

die() { printf 'Installation failed: %s\n' "$*" >&2; exit 1; }
usage() {
    printf '%s\n' 'Usage: ./install.sh --pg-config /path/to/pg_config [--destdir /staging-root] [--dry-run]'
}

pg_config=''
destdir=''
dry_run=false
while (( $# )); do
    case "$1" in
        --pg-config|--destdir)
            (( $# >= 2 )) || die "Missing value for $1"
            if [[ $1 == --pg-config ]]; then pg_config=$2; else destdir=$2; fi
            shift 2 ;;
        --dry-run) dry_run=true; shift ;;
        --help|-h) usage; exit 0 ;;
        *) usage >&2; die "Unknown argument: $1" ;;
    esac
done
[[ -n $pg_config ]] || die 'Pass --pg-config explicitly to select the PostgreSQL installation.'
[[ -x $pg_config ]] || die "pg_config is not executable: $pg_config"
[[ -z $destdir || $destdir == /* ]] || die '--destdir must be an absolute path.'
bundle=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)

# Do not allow checksum paths to read outside the extracted bundle.
[[ -f $bundle/SHA256SUMS && ! -L $bundle/SHA256SUMS ]] || die 'SHA256SUMS is missing.'
checksum_files=$'\n'
while IFS= read -r record; do
    [[ $record =~ ^[0-9a-f]{64}[[:space:]][[:space:]]([A-Za-z0-9_./+-]+)$ ]] || die 'Invalid checksum record.'
    relative=${BASH_REMATCH[1]}
    [[ $relative != /* && $relative != *'..'* ]] || die 'Invalid checksum path.'
    [[ $checksum_files != *$'\n'"$relative"$'\n'* ]] || die 'Duplicate checksum path.'
    checksum_files="$checksum_files$relative"$'\n'
    [[ -f $bundle/$relative && ! -L $bundle/$relative ]] || die "Missing regular file: $relative"
done < "$bundle/SHA256SUMS"
for required in package.metadata manifest.json install.sh; do
    [[ $checksum_files == *$'\n'"$required"$'\n'* ]] || die "Artifact missing from SHA256SUMS: $required"
done
if command -v sha256sum >/dev/null 2>&1; then
    (cd "$bundle" && sha256sum --check SHA256SUMS >/dev/null) || die 'Bundle checksum verification failed.'
elif command -v shasum >/dev/null 2>&1; then
    (cd "$bundle" && shasum -a 256 --check SHA256SUMS >/dev/null) || die 'Bundle checksum verification failed.'
else
    die 'Install sha256sum or shasum to verify this bundle.'
fi

format='' extension='' version='' postgres_major='' system='' architecture='' platform='' library='' minimum_glibc=''
seen=' '
while IFS='=' read -r key value; do
    [[ $seen != *" $key "* ]] || die "Duplicate metadata field: $key"
    seen="$seen$key "
    case "$key" in
        format) format=$value ;;
        extension) extension=$value ;;
        version) version=$value ;;
        postgres_major) postgres_major=$value ;;
        system) system=$value ;;
        architecture) architecture=$value ;;
        platform) platform=$value ;;
        library) library=$value ;;
        minimum_glibc) minimum_glibc=$value ;;
        *) die "Unknown metadata field: $key" ;;
    esac
done < "$bundle/package.metadata"
[[ $format == 1 && $extension == geometry_pyramid ]] || die 'Unrecognized bundle format.'
[[ $version =~ ^[A-Za-z0-9][A-Za-z0-9.+-]*$ && $postgres_major =~ ^[1-9][0-9]+$ ]] || die 'Invalid package version.'
case "$postgres_major" in
    15|16|17|18) ;;
    *) die "Unsupported PostgreSQL major: $postgres_major; this release supports 15 through 18." ;;
esac
[[ $platform =~ ^[a-z0-9][a-z0-9.-]*$ ]] || die 'Invalid platform label.'
[[ $architecture == x86_64 || $architecture == aarch64 ]] || die 'Invalid package architecture.'

case "$(uname -s)" in
    Linux) host_system=linux ;;
    Darwin) host_system=macos ;;
    *) die 'This installer supports Linux and macOS. Use install.ps1 for a Windows bundle.' ;;
esac
[[ $system == "$host_system" ]] || die "Bundle requires $system; host is $host_system."
expected_library=geometry_pyramid.so
if [[ $system == macos ]] && (( postgres_major >= 16 )); then expected_library=geometry_pyramid.dylib; fi
[[ $library == "$expected_library" ]] || die 'Unexpected library filename.'
target_version=$("$pg_config" --version)
[[ $target_version =~ ^PostgreSQL[[:space:]]+([0-9]+)(\.|$|[[:space:]]) ]] || die "Invalid pg_config version: $target_version"
[[ ${BASH_REMATCH[1]} == "$postgres_major" ]] || die "This bundle requires PostgreSQL $postgres_major; target is $target_version."

# Verify the server binary, not just the shell architecture (e.g. Rosetta).
bindir=$("$pg_config" --bindir)
postgres=$bindir/postgres
[[ -f $postgres && -x $postgres ]] || die "PostgreSQL server binary not found: $postgres"
hex_at() { od -An -v -tx1 -j "$2" -N "$3" "$1" | tr -d ' \n'; }
magic=$(hex_at "$postgres" 0 4)
server_architecture=''
if [[ $system == linux && $magic == 7f454c46 ]]; then
    case "$(hex_at "$postgres" 18 2)" in
        3e00) server_architecture=x86_64 ;;
        b700) server_architecture=aarch64 ;;
    esac
elif [[ $system == macos && $magic == cffaedfe ]]; then
    case "$(hex_at "$postgres" 4 4)" in
        07000001) server_architecture=x86_64 ;;
        0c000001) server_architecture=aarch64 ;;
    esac
fi
[[ $server_architecture == "$architecture" ]] || die 'PostgreSQL server architecture does not match the bundle (or its binary format is unsupported).'

if [[ $system == linux && $minimum_glibc != none ]]; then
    [[ $minimum_glibc =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]] || die 'Invalid minimum glibc version.'
    host_libc=$(getconf GNU_LIBC_VERSION 2>/dev/null) || die "This bundle needs glibc >= $minimum_glibc."
    [[ $host_libc == glibc\ * ]] || die "This bundle needs glibc >= $minimum_glibc."
    host_glibc=${host_libc#glibc }
    awk -v current="$host_glibc" -v required="$minimum_glibc" 'BEGIN {
        split(current, a, "."); split(required, b, ".");
        for (i=1; i<=3; i++) { if (a[i]+0 > b[i]+0) exit 0; if (a[i]+0 < b[i]+0) exit 1; }
        exit 0;
    }' || die "This bundle needs glibc >= $minimum_glibc; host has $host_glibc."
fi
libdir=$("$pg_config" --pkglibdir)
sharedir=$("$pg_config" --sharedir)
[[ $libdir == /* && $sharedir == /* ]] || die 'pg_config must return absolute installation paths.'
libdir=${destdir%/}$libdir
extdir=${destdir%/}$sharedir/extension
[[ -f $bundle/extension/geometry_pyramid--$version.sql ]] || die 'Current-version SQL is missing.'

sources=("$bundle/lib/$library")
destinations=("$libdir/$library")
for sql in "$bundle/extension/"geometry_pyramid--*.sql; do
    [[ -f $sql && ! -L $sql && ${sql##*/} =~ ^geometry_pyramid--[A-Za-z0-9.+-]+\.sql$ ]] || die 'Invalid SQL artifact.'
    sources+=("$sql")
    destinations+=("$extdir/${sql##*/}")
done
# Advertise the new default_version only after every library/SQL file is present.
sources+=("$bundle/extension/geometry_pyramid.control")
destinations+=("$extdir/geometry_pyramid.control")
for (( index=0; index<${#sources[@]}; index++ )); do
    relative=${sources[$index]#"$bundle/"}
    [[ $checksum_files == *$'\n'"$relative"$'\n'* ]] || die "Artifact missing from SHA256SUMS: $relative"
    printf '%s -> %s\n' "${sources[$index]##*/}" "${destinations[$index]}"
done
if $dry_run; then exit 0; fi

mkdir -p -- "$libdir" "$extdir"
temporaries=()
backups=()
commit_started=false
finish() {
    local status=$? index recovery_failed=false
    trap - EXIT
    set +e
    if (( status != 0 )) && $commit_started; then
        printf '%s\n' 'Installation interrupted; restoring the previous extension files.' >&2
        for (( index=${#temporaries[@]}-1; index>=0; index-- )); do
            # A successful rename consumes its temporary file. Checking this
            # also covers an interrupt just after mv but before the next line.
            if [[ ! -e ${temporaries[$index]} ]]; then
                if [[ -n ${backups[$index]} ]]; then
                    mv -f -- "${backups[$index]}" "${destinations[$index]}" || recovery_failed=true
                else
                    rm -f -- "${destinations[$index]}" || recovery_failed=true
                fi
            fi
        done
    fi
    if $recovery_failed; then
        printf '%s\n' 'Automatic rollback could not finish. Stop extension activity and restore these files; recovery materials have been kept:' >&2
        for (( index=0; index<${#backups[@]}; index++ )); do
            if [[ -n ${backups[$index]} && -e ${backups[$index]} ]]; then
                printf '  backup %s -> %s\n' "${backups[$index]}" "${destinations[$index]}" >&2
            elif [[ -z ${backups[$index]} && ! -e ${temporaries[$index]} ]]; then
                printf '  remove newly installed file %s\n' "${destinations[$index]}" >&2
            fi
        done
    else
        for (( index=0; index<${#temporaries[@]}; index++ )); do rm -f -- "${temporaries[$index]}"; done
        for (( index=0; index<${#backups[@]}; index++ )); do
            [[ -z ${backups[$index]} ]] || rm -f -- "${backups[$index]}"
        done
    fi
    exit "$status"
}
trap finish EXIT
# Prepare all new files before replacing any. Rename within each destination
# directory so existing backends keep their old mmap instead of modified bytes.
for (( index=0; index<${#sources[@]}; index++ )); do
    destination=${destinations[$index]}
    temporary=$(mktemp "${destination%/*}/.geometry_pyramid.XXXXXX")
    temporaries+=("$temporary")
    cp -- "${sources[$index]}" "$temporary"
    chmod 0644 "$temporary"
    if [[ -e $destination || -L $destination ]]; then
        [[ -f $destination && ! -L $destination ]] || die "Destination must be a regular file: $destination"
        backup=$(mktemp "${destination%/*}/.geometry_pyramid.backup.XXXXXX")
        backups+=("$backup")
        cp -p -- "$destination" "$backup"
    else
        backups+=('')
    fi
done
commit_started=true
for (( index=0; index<${#temporaries[@]}; index++ )); do
    mv -f -- "${temporaries[$index]}" "${destinations[$index]}"
done
printf 'Installed geometry_pyramid %s for %s.\n' "$version" "$target_version"
printf '%s\n' 'Enable or upgrade the extension in each intended database using an administrator; reconnect existing sessions after an upgrade.'
