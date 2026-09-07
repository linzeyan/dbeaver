#!/usr/bin/env bash
#
# The Windows C++ checks, in one place because two machines run them: a CI
# runner and the UTM guest `vm-build.sh` drives. This file exists the moment
# there is a second caller — ci.yml opens by saying that a CI which reimplements
# the local commands drifts from them, and a copy of these `cl` lines living in
# the workflow would be exactly that.
#
# What it does not do is build the staticlib, and that is not an omission. rustc
# finds the MSVC linker by itself, and putting MSVC on PATH first breaks it: Git
# ships a coreutils `link` in `/usr/bin` that wins the lookup, and cargo then
# dies inside `libduckdb-sys` with `/usr/bin/link: extra operand`, which reads
# like a DuckDB problem. So cargo runs before the compiler environment exists,
# in the caller, and by the time this runs both of its inputs are already there.
#
# Expects, in the working directory:
#   target/<profile>/dbffi.lib   the staticlib
#   target/natives.txt           `--print native-static-libs` output
# and `cl` on PATH.
set -o pipefail

profile="${1:-debug}"

if ! command -v cl >/dev/null 2>&1; then
    echo "cl is not on PATH: run this inside a Visual Studio environment" >&2
    exit 1
fi
if [ ! -f target/natives.txt ]; then
    echo "target/natives.txt is missing: the caller builds the staticlib and asks rustc" >&2
    exit 1
fi

# A Rust staticlib does not record that its contents need ws2_32 and friends —
# the same gap macOS covers by naming `c++` in Package.swift. The list is asked
# of the compiler rather than kept here, so it is measured on the machine that
# is about to use it rather than copied from another one.
libs=$(grep -o 'native-static-libs:.*' target/natives.txt | head -1 | cut -d: -f2-)
echo "system libraries:$libs"

# `-` rather than `/` for every cl flag. Under Git Bash an argument that starts
# with a slash is rewritten as a Windows path, so `/nologo` would arrive as a
# filename; cl accepts both spellings and only one of them survives the shell.
#
# `-MD` because the list above ends in `/defaultlib:msvcrt`: that is the dynamic
# CRT, and it is also what the MSVC side of `.linkedLibrary("c++")` turns out to
# be — DuckDB's C++ arrives with the CRT rather than as a library of its own. cl
# defaults to `-MT`, the static one, and a program this small links anyway; a
# real front end linking two CRTs is where the duplicate symbols start.
#
# `$libs` is unquoted on purpose: it is a list of libraries and has to split into
# one argument each. Quoting it hands the linker one argument with spaces in it.
build() {
    local source="$1"
    local output="$2"
    shift 2
    # shellcheck disable=SC2086
    cl -nologo -EHsc -MD -std:c++17 \
        -I apps/macos/Sources/CDbFfi/include \
        "$source" \
        "-Fe:$output" \
        -link "-LIBPATH:target/$profile" dbffi.lib "$@" $libs
}

build apps/windows/ffi-check/main.cpp ffi-check.exe || exit 1
./ffi-check.exe || exit 1

build apps/windows/DbClient/main.cpp dbclient.exe \
    d2d1.lib dwrite.lib windowscodecs.lib ole32.lib || exit 1
./dbclient.exe --verify-drivers || exit 1
./dbclient.exe --verify-grid || exit 1
