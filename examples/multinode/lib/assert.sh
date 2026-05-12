# shellcheck shell=bash
# Tiny assertion helpers sourced by every scenario script under
# examples/multinode/scenarios/. Each scenario follows the same shape:
#
#   #!/usr/bin/env bash
#   set -Eeuo pipefail
#   . "$(dirname "$0")/../lib/assert.sh"
#   ... run command, capture output ...
#   assert_eq "$got_exit" 0 "spawn exit code"
#   echo OK
#
# Helpers exit non-zero with a clear message on failure; success is silent.

set -Eeuo pipefail

# assert_eq <got> <expected> <label>
# Fail if "$got" != "$expected".
assert_eq() {
    local got="$1" want="$2" label="${3:-value}"
    if [ "$got" != "$want" ]; then
        echo "FAIL: $label: got '$got', want '$want'" >&2
        return 1
    fi
}

# assert_contains <haystack> <needle> [<label>]
# Fail if <needle> is not a literal substring of <haystack>.
assert_contains() {
    local hay="$1" needle="$2" label="${3:-output}"
    if [[ "$hay" != *"$needle"* ]]; then
        echo "FAIL: $label does not contain '$needle'" >&2
        echo "----- $label -----" >&2
        printf '%s\n' "$hay" >&2
        echo "------------------" >&2
        return 1
    fi
}

# assert_exit <got_exit> <expected> [<label>]
# Like assert_eq but specialised for shell exit codes (numeric).
assert_exit() {
    local got="$1" want="$2" label="${3:-exit}"
    if [ "$got" -ne "$want" ]; then
        echo "FAIL: $label: got exit $got, want $want" >&2
        return 1
    fi
}

# assert_nonzero_exit <got_exit> [<label>]
# Fail iff the captured exit code is 0.
assert_nonzero_exit() {
    local got="$1" label="${2:-exit}"
    if [ "$got" -eq 0 ]; then
        echo "FAIL: $label expected non-zero exit, got 0" >&2
        return 1
    fi
}
