#!/usr/bin/env bash
#
# Build the Windows front end on the Windows VM and print what it said here.
#
# Until this existed the only oracle for anything under `apps/windows` was a CI
# round trip: minutes, after a commit and a push, for questions like whether a
# header is spelled right. This makes the compiler answer in seconds and before
# the commit, which is the whole point. CI stays the gate; this is the loop.
#
# It sends the working tree as it is, uncommitted changes included, so there is
# nothing to commit first and nothing to undo afterwards.
#
#   apps/windows/tools/vm-build.sh            # staticlib, both C++ programs, checks
#   apps/windows/tools/vm-build.sh --quick    # skip cargo, reuse the staticlib
#
#   DBEAVER_VM=host        ssh host to use          (default: macshot-vm)
#   DBEAVER_VM_ROOT=path   the clone inside the VM  (default: C:/src/dbeaver)
#
# The default host is the entry the other project on this machine already put in
# `~/.ssh/config`, pointing at the same UTM guest. A second name for one machine
# would be two things to keep in step for no gain.
#
# ── One-time setup, inside the Windows guest ────────────────────────────────
#
# Most of it is already done if `macshot-vm` answers: OpenSSH server, the Mac's
# key in `C:\ProgramData\ssh\administrators_authorized_keys` (that file, not
# `~/.ssh/authorized_keys`, for an admin account), and git's bash as the ssh
# shell — that last one is not optional, because git's transport sends
# `git-receive-pack 'C:/path'` and assumes the remote strips the quotes, which
# cmd.exe does not.
#
# What this project adds:
#
#   winget install --id Rustlang.Rustup -e
#   # and the C++ toolset, which Rust needs a linker from and `cl` comes with:
#   winget install --id Microsoft.VisualStudio.2022.BuildTools -e
#   # and clang, which is not an alternative to that one:
#   winget install --id LLVM.LLVM -e
#
# The last is only needed because this guest is arm64. `ring` builds its C with
# whatever cc-rs found and then overrides it — `if target.os == WINDOWS &&
# target.arch == AARCH64 && !compiler.is_like_clang() { c.compiler("clang") }`,
# under a FIXME — so on this machine the MSVC that everything else uses is asked
# for and then thrown away. Without clang installed the build stops at `failed
# to find tool "clang"`, which reads like a broken toolchain rather than like one
# crate's hardcoded preference. CI never meets it: its runner is x86_64.
#
# The repository does not need cloning by hand — this script creates it on the
# first run and pushes into it.
# ────────────────────────────────────────────────────────────────────────────
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/vm-wake.sh"

VM="${DBEAVER_VM:-macshot-vm}"
ROOT="${DBEAVER_VM_ROOT:-C:/src/dbeaver}"

quick=false
for argument in "$@"; do
    case "$argument" in
    --quick) quick=true ;;
    *)
        echo "unknown option: $argument" >&2
        exit 2
        ;;
    esac
done

cd "$(git rev-parse --show-toplevel)"

if ! vm_wake "$VM" || ! ssh -o BatchMode=yes -o ConnectTimeout=10 "$VM" "git --version" >/dev/null 2>&1; then
    echo "cannot reach $VM over ssh, or git is not on its PATH." >&2
    echo "the setup steps are in the header of this script." >&2
    exit 1
fi

# Created rather than cloned. A clone would come from GitHub and drag twenty
# years of the upstream Java history across the internet; this pushes the same
# history over the host-to-guest link instead, once, and never mentions a
# remote the guest would then be able to drift towards.
if ! ssh "$VM" "test -d $ROOT/.git"; then
    echo "→ creating $ROOT on $VM"
    ssh "$VM" "git init --quiet $ROOT"
fi

# A ref outside refs/heads/, so the guest never has it checked out and the push
# cannot be refused for updating the current branch. The guest resets onto it.
echo "→ sending $(git rev-parse --short HEAD) to $VM"
git push --quiet --force "$VM:$ROOT" "HEAD:refs/vm/head"

# clean without -x: `target/` is ignored, and wiping it turns every run into a
# cold build of DuckDB's C++. The guest's tree is disposable in every other
# respect.
ssh "$VM" "git -C $ROOT reset --quiet --hard refs/vm/head && git -C $ROOT clean -qfd"

# Through a scratch index rather than `git diff HEAD`, because that one cannot
# see a file git has never been told about — and a new file is what this kind of
# work adds most often. `add -A` here writes to the copy, so the real index and
# anything staged in it are untouched.
scratch="$(mktemp -t dbeaver-vm-index)"
trap 'rm -f "$scratch"' EXIT
cp "$(git rev-parse --git-dir)/index" "$scratch"

if ! GIT_INDEX_FILE="$scratch" git add -A 2>/dev/null; then
    echo "cannot read the working tree." >&2
    exit 1
fi

if ! GIT_INDEX_FILE="$scratch" git diff --cached --quiet HEAD; then
    echo "→ applying uncommitted changes"
    GIT_INDEX_FILE="$scratch" git diff --cached HEAD --binary \
        | ssh "$VM" "git -C $ROOT apply --whitespace=nowarn -"
fi

# The guest side is `vm-checks.cmd`, in the repository rather than on this
# command line. It has the compiler environment to set up, and a command line
# carrying that has to survive local bash, ssh, the guest's bash and then cmd —
# whose rule for `/c` is to strip the first and last quote it sees, which takes
# the quotes off `C:\Program Files (x86)\...` and breaks the path at its first
# space. Naming a file has no quotes in it to lose.
#
# Called from its own directory, by name, with no path on it — and that is
# forced rather than chosen. MSYS strips backslashes while assembling a Windows
# command line, so `apps\windows\tools\x.cmd` arrives as `appswindowstoolsx.cmd`;
# with forward slashes cmd reads `apps` as the command and `/windows` as a
# switch to it. The batch file changes back to the repository root itself.
#
# `MSYS_NO_PATHCONV=1` stops git's bash rewriting `/c` into a path under the Git
# installation. It is the alternative to spelling the switch `//c`, not a
# companion to it: with the conversion off, `//c` reaches cmd as `//c`, which it
# does not recognise, and it opens an interactive shell instead of running
# anything — a hang that looks like the guest being slow.
flags=""
if [ "$quick" = true ]; then
    flags=" --quick"
fi

echo "→ building"
set +e
output="$(ssh "$VM" "cd $ROOT/apps/windows/tools && MSYS_NO_PATHCONV=1 cmd /c vm-checks.cmd$flags" 2>&1)"
status=$?
set -e

echo "$output"

if [ $status -ne 0 ]; then
    echo
    echo "── errors ──────────────────────────────────────────────────────────"
    # Deduplicated and stripped of the path prefix: cl reports the same
    # diagnostic once per translation unit that saw it, and three copies of one
    # error reads as three problems.
    echo "$output" | grep -E "error [A-Z]+[0-9]+" | sed 's|^.*[\\/]dbeaver[\\/]||' | sort -u
fi

exit $status
