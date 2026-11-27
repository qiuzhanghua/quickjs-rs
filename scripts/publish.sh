#!/usr/bin/env bash
# Publish preflight: check every crate is packageable, and in what order.
#
# `cargo publish` verifies a crate by building it from the tarball, which means its
# path dependencies must already be on the registry. For a first release that is only
# true in dependency order, so this script both checks the metadata and prints the
# order — the order is not advisory, publishing out of it fails.
#
# Usage:
#   scripts/publish.sh              # preflight only (no upload)
#   scripts/publish.sh --upload     # preflight, then publish each crate for real
#
# Two things about this machine's setup, both of which have to be handled explicitly:
#
#  * `ALL_PROXY` points at a local proxy that crates.io answers 403 to, so it is
#    unset below; the network is reached directly.
#  * `~/.cargo/config.toml` replaces the `crates-io` source with a mirror. cargo
#    refuses to publish through a replaced source, so every upload names
#    `--registry crates-io` — which targets the real registry, not the mirror.
set -euo pipefail

cd "$(dirname "$0")/.."

# Dependency order. `quickjs-cli` and `quickjs-conformance` are `publish = false`:
# the CLI would fight the C `qjs` for the same command name, and the conformance
# harness is a dev-dependency that is stripped from every published manifest.
ORDER=(quickjs-gc quickjs-parse quickjs-compile quickjs-pure)

# Whether a cargo failure is the "my dependency is not on the registry yet" case,
# which is *expected* on a first release of a version and must not be reported as a
# failure.
#
# Two spellings, because cargo's wording changed: older versions say "no matching
# package named", current ones say "failed to select a version for the requirement".
# Matching only the old text made every preflight red from the moment a crate gained a
# dependency its published version did not have — the check existed, it just could not
# recognise the state it was built to tolerate.
dependency_not_published() {
    grep -qE "no matching package named|failed to select a version for the requirement"
}

upload=0
dirty=()
for arg in "$@"; do
    case "$arg" in
        --upload) upload=1 ;;
        # A release is packaged from a clean tree, because the tarball records the
        # commit it came from. This exists for checking the script itself.
        --allow-dirty) dirty=(--allow-dirty) ;;
        *) echo "unknown option: $arg" >&2; exit 2 ;;
    esac
done
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)

# The proxy this machine sets up is not a route to crates.io, and cargo would spend a
# minute timing out against it. Publishing goes direct.
unset ALL_PROXY HTTP_PROXY HTTPS_PROXY http_proxy https_proxy all_proxy

echo "== metadata"
for crate in "${ORDER[@]}"; do
    manifest="crates/$crate/Cargo.toml"
    name=$(sed -n 's/^name = "\(.*\)"/\1/p' "$manifest" | head -1)
    for field in description license repository readme; do
        if ! grep -qE "^$field" "$manifest"; then
            echo "  $crate: missing $field" >&2
            exit 1
        fi
    done
    if ! [[ -f "crates/$crate/README.md" ]]; then
        echo "  $crate: no crate-specific README.md" >&2
        exit 1
    fi
    if ! [[ -f "crates/$crate/LICENSE" ]]; then
        echo "  $crate: no LICENSE" >&2
        exit 1
    fi
    # A path *dependency* without a version is rejected at upload time, so catch it here.
    # Scoped to `[dependencies]`-like sections and stopping at `[dev-dependencies]`:
    # a dev-dependency may name a path alone, because cargo strips dev-dependencies from
    # the published manifest, so `quickjs-conformance` is legitimately versionless.
    # Not `grep -q … | grep -v …`: `-q` prints nothing, so the second `grep` never saw a
    # line and this check could not fire at all. It has to be a filter that still prints.
    if sed -nE '/^\[dev-dependencies\]/q
                /^[[:space:]]*quickjs-[a-z]+ = \{ path/ { /version/!p }' "$manifest" | grep -q .; then
        echo "  $crate: a path dependency has no version" >&2
        exit 1
    fi
    printf '  %-18s %s\n' "$crate" "$version"
done

echo "== package"
for crate in "${ORDER[@]}"; do
    # `--no-verify` here: verification builds the tarball, which needs the earlier
    # crates to be on the registry already. The dry-run below does verify, in order.
    if ! package_log=$(cargo package --no-verify ${dirty[@]+"${dirty[@]}"} -p "$crate" 2>&1); then
        # Before the first release every crate but the first fails here, because its
        # path dependency has no published version to resolve against yet.
        if printf '%s' "$package_log" | dependency_not_published; then
            printf '  %-18s skipped (a dependency is not published yet)\n' "$crate"
            continue
        fi
        echo "$package_log" >&2
        exit 1
    fi
    tarball=$(ls -t "target/package/$crate"-*.crate | head -1)
    files=$(tar tzf "$tarball" | wc -l | tr -d ' ')
    printf '  %-18s %s (%s files)\n' "$crate" "$(basename "$tarball")" "$files"
    for required in README.md LICENSE; do
        # The listing is captured before grepping: `tar tzf … | grep -q` makes `grep`
        # exit as soon as it matches, `tar` then dies of SIGPIPE (141), and `pipefail`
        # turns that into "the file is missing" — a false alarm on every single crate.
        listing=$(tar tzf "$tarball")
        if ! printf '%s\n' "$listing" | grep -q "/$required$"; then
            echo "  $crate: $required is not in the tarball" >&2
            exit 1
        fi
    done
done

echo "== dry-run (this is what a real publish verifies)"
for crate in "${ORDER[@]}"; do
    printf '  %-18s ' "$crate"
    if cargo publish --dry-run ${dirty[@]+"${dirty[@]}"} -p "$crate" --registry crates-io >/tmp/preflight.log 2>&1; then
        echo "ok"
    else
        # Before the first release, every crate but the first fails here for the same
        # reason: its dependencies are not on the registry yet. That is expected.
        if dependency_not_published < /tmp/preflight.log; then
            echo "skipped (a dependency is not published yet)"
        else
            echo "FAILED"
            tail -20 /tmp/preflight.log >&2
            exit 1
        fi
    fi
done

if [[ $upload -eq 0 ]]; then
    echo
    echo "Preflight passed. Re-run with --upload to publish, in this order:"
    printf '  %s\n' "${ORDER[@]}"
    exit 0
fi

if [[ -z "${CARGO_REGISTRY_TOKEN:-}" ]] && ! [[ -f "${CARGO_HOME:-$HOME/.cargo}/credentials.toml" ]]; then
    echo "No crates.io token: set CARGO_REGISTRY_TOKEN or run 'cargo login'." >&2
    exit 1
fi

echo "== publish"
for crate in "${ORDER[@]}"; do
    echo "  $crate"
    cargo publish -p "$crate" --registry crates-io
    # crates.io needs a moment before the next crate can resolve this one.
    sleep 20
done
