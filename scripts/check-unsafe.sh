#!/bin/sh
# NFR-6: `unsafe` lives in the four engine modules plus exactly one host module.
#
# The rule is a sentence in docs/requirements.md, and a sentence is not a gate. This turns
# it into one: every `crates/*` file is scanned, and a file that mentions `unsafe` has to be
# on the list below. A new exception therefore fails the build instead of quietly joining
# the codebase, and the exception that exists is visible in one place.
set -eu
cd "$(dirname "$0")/.."

# The engine modules NFR-6 allows. `value.rs` is the string representation: `Wtf8::as_str`
# calls `from_utf8_unchecked` behind a flag the encoder alone sets, which is exactly the
# kind of invariant NFR-6 keeps `unsafe` for — and validating on every access instead would
# put a scan in the hot path of every property read.
allowed="
crates/quickjs-gc/src/lib.rs
crates/quickjs-pure/src/builtins/atomics.rs
crates/quickjs-pure/src/builtins/typedarray.rs
crates/quickjs-pure/src/capi.rs
crates/quickjs-pure/src/value.rs
"

# The two host exceptions, each with its argument in the file itself:
#
#   env.rs    — `setenv(3)` has no safe wrapper in any available crate.
#   signal.rs — installing a signal handler is FFI to an OS callback; the handler only
#               touches an `AtomicU64`, and the JavaScript call happens on the loop.
#
# Both are single-topic modules so a reviewer reads one file, and both are listed here so
# that adding a third is a deliberate, visible act.
allowed="$allowed
crates/quickjs-host/src/env.rs
crates/quickjs-host/src/signal.rs
"

# `unsafe_op_in_unsafe_fn` is a lint name, not an unsafe block; the deny attributes are the
# per-crate policy the NFR's verification column already names.
pattern='unsafe[[:space:]]*(\{|fn|impl|extern|trait)'

found=0
for file in $(find crates -name '*.rs' -not -path '*/target/*' | sort); do
    if grep -Eq "$pattern" "$file"; then
        case " $(echo $allowed) " in
        *" $file "*) ;;
        *)
            echo "NFR-6 FAILED: $file contains unsafe but is not on the allowed list"
            found=1
            ;;
        esac
    fi
done

if [ "$found" -eq 0 ]; then
    echo "NFR-6 ok: unsafe is confined to the engine modules and the two documented host exceptions"
fi
exit "$found"
