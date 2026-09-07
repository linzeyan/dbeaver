#!/usr/bin/env bash
#
# Make the Windows VM answer, starting it if it is not.
#
# Sourced rather than run: `vm-build.sh` begins by talking to the guest over ssh
# and fails the same way every time the machine is simply off — a connection
# timeout, which reads like a broken ssh configuration and sends whoever hit it
# into the setup notes instead of into UTM. Asking UTM to start it is one call,
# and `start` resumes a suspended machine as well as booting a stopped one, so it
# covers both ways the guest can be down.
#
#   DBEAVER_VM_UTM=name   the machine as UTM names it, not as ssh does
#                         (default: Windows)

UTMCTL=/Applications/UTM.app/Contents/MacOS/utmctl

vm_reachable() {
    ssh -o BatchMode=yes -o ConnectTimeout=5 "$1" true >/dev/null 2>&1
}

# 0 when the host is answering by the time this returns.
vm_wake() {
    local host="$1"
    local machine="${DBEAVER_VM_UTM:-Windows}"

    vm_reachable "$host" && return 0
    [ -x "$UTMCTL" ] || return 1

    echo "→ $host is not answering; starting $machine"
    "$UTMCTL" start "$machine" >/dev/null 2>&1 || true

    # Windows is slow between the machine running and sshd accepting, and a cold
    # boot is the slow case. Two minutes is longer than any boot measured here
    # and far shorter than the round trip through a person noticing it was off.
    local waited=0
    while [ "$waited" -lt 120 ]; do
        sleep 5
        waited=$((waited + 5))
        vm_reachable "$host" && return 0
    done

    return 1
}
