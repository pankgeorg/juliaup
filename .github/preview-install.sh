#!/bin/sh
# Installs the preview build of juliaup published by the fork's `preview`
# release over the juliaup found on PATH, installing the official juliaup
# first when there is none. Safe to run as `curl ... | sh`.

set -eu

main() {
    repo="${JULIAUP_PREVIEW_REPO:-pankgeorg/juliaup}"
    base="https://github.com/${repo}/releases/download/preview"

    tmp=$(mktemp -d)
    trap 'rm -rf "$tmp"' EXIT

    fetch "$base/RELEASECHANNELJULIAUPVERSION" "$tmp/version"
    version=$(cat "$tmp/version")
    target=$(detect_target "$repo")
    say "installing juliaup $version for $target"

    bindir=$(juliaup_bindir "$tmp")
    [ -w "$bindir" ] || fail "cannot write to $bindir; this juliaup seems to come from a package manager. Unpack $base/juliaup-$version-$target.tar.gz over it by hand."

    fetch "$base/juliaup-$version-$target.tar.gz" "$tmp/juliaup.tar.gz"
    mkdir "$tmp/bin"
    tar -xzf "$tmp/juliaup.tar.gz" -C "$tmp/bin"
    "$tmp/bin/juliaup" --version > /dev/null < /dev/null || fail "the downloaded juliaup does not run on this machine"

    # The launcher's periodic self-update would otherwise reinstall the
    # upstream release, since its version differs from this one.
    for setting in startupselfupdateinterval backgroundselfupdateinterval; do
        "$bindir/juliaup" config "$setting" 0 > /dev/null 2> "$tmp/config.err" < /dev/null \
            || say "warning: could not set $setting to 0: $(tail -n 1 "$tmp/config.err"); the official release may come back through self-update"
    done

    for bin in juliaup julialauncher julia; do
        mv -f "$tmp/bin/$bin" "$bindir/$bin"
    done

    say "installed $("$bindir/juliaup" --version < /dev/null) in $bindir"
    say "to go back to the official release: juliaup self update"
}

say() {
    printf 'preview-install: %s\n' "$*" >&2
}

fail() {
    say "$*"
    exit 1
}

fetch() {
    if command -v curl > /dev/null 2>&1; then
        curl -fsSL --retry 3 "$1" -o "$2" < /dev/null
    elif command -v wget > /dev/null 2>&1; then
        wget -q "$1" -O "$2" < /dev/null
    else
        fail "need curl or wget"
    fi
}

# The build target the preview release has for this machine. Linux gets
# the musl builds, as the official installer does.
detect_target() {
    os=$(uname -s)
    cpu=$(uname -m)
    case "$os" in
        Linux)
            case "$cpu" in
                x86_64 | amd64) echo x86_64-unknown-linux-musl ;;
                aarch64 | arm64) echo aarch64-unknown-linux-musl ;;
                *) fail "no preview build for Linux on $cpu" ;;
            esac
            ;;
        Darwin)
            if [ "$cpu" = x86_64 ] && [ "$(sysctl -n hw.optional.arm64 2> /dev/null)" = 1 ]; then
                cpu=arm64
            fi
            case "$cpu" in
                x86_64) echo x86_64-apple-darwin ;;
                arm64) echo aarch64-apple-darwin ;;
                *) fail "no preview build for macOS on $cpu" ;;
            esac
            ;;
        *)
            fail "no preview build for $os from this script; see https://github.com/$1/releases/tag/preview"
            ;;
    esac
}

# Follows symlinks to the real file, as `readlink -f` would where available.
resolve() {
    path=$1
    while [ -L "$path" ]; do
        link=$(readlink "$path")
        case "$link" in
            /*) path=$link ;;
            *) path=$(dirname "$path")/$link ;;
        esac
    done
    echo "$path"
}

# The directory holding the juliaup to replace. Without one, the official
# installer runs first so that the depot, PATH and default channel are set up
# the usual way.
juliaup_bindir() {
    if existing=$(command -v juliaup 2> /dev/null); then
        dirname "$(resolve "$existing")"
        return
    fi
    say "juliaup is not installed; running the official installer first"
    fetch https://install.julialang.org "$1/juliaup-init.sh"
    sh "$1/juliaup-init.sh" --yes < /dev/null 1>&2
    installed="${HOME}/.juliaup/bin/juliaup"
    [ -x "$installed" ] || fail "expected the official installer to leave $installed behind"
    dirname "$installed"
}

main "$@"
