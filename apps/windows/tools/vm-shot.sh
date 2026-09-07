#!/usr/bin/env bash
#
# Photograph the Windows VM's desktop, after optionally starting something on it.
#
# The other half of vm-build.sh. That one answers "does it compile"; this one
# answers "what does it look like", and for a front end whose whole job is to
# match another front end's appearance, that is the question that decides
# whether the work is done. Without it every layout, weight and alignment bug
# costs a round trip through a person with a screenshot tool.
#
#   apps/windows/tools/vm-shot.sh                                  # the desktop as it is
#   apps/windows/tools/vm-shot.sh --start 'C:\src\dbeaver\target\dbclient.exe'
#   apps/windows/tools/vm-shot.sh --click 900,470                  # click there, then photograph
#   apps/windows/tools/vm-shot.sh --click 900,470 --keys '{DOWN}{RIGHT}'
#   apps/windows/tools/vm-shot.sh --wait 3 /tmp/grid.png           # slower; somewhere specific
#
# Points are in physical pixels, which is what the capture is in too — read one
# off the last screenshot and it lands where it looked like it would. Several
# points in one --click are separated by semicolons and pressed in order.
# SendKeys notation for --keys: ^ is Ctrl, + is Shift, % is Alt, and the named
# keys are {DOWN} {UP} {LEFT} {RIGHT} {ENTER} {ESC} {TAB}.
#
#   DBEAVER_VM=host   ssh host to use (default: macshot-vm)
#
# ── Why a scheduled task ─────────────────────────────────────────────────────
# An ssh session gets its own window station, and there is no desktop on it. A
# capture taken from there is a blank bitmap, and CopyFromScreen throws a
# Win32Exception on the way to producing it. The task is registered with /IT,
# which runs it as the logged-on user in the session that actually has the
# screen, and that is the only thing that makes the picture real. It follows
# that somebody has to be logged in to the guest — a locked screen is not the
# same thing, and the failure mode is an empty file rather than an error.
#
# Dragging, scrolling and held modifiers are not here. macshot's copy of this
# script has all of it — ~/git/macshot/windows/tools/vm-shot.sh — and each piece
# is worth taking the day this front end has something that needs it. The click
# and the keys came over when the grid grew a selection, because a selection is
# invisible until something points at it: a photograph of the window as it opens
# says nothing about the feature it was taken to look at.
# ─────────────────────────────────────────────────────────────────────────────
set -euo pipefail

. "$(dirname "${BASH_SOURCE[0]}")/vm-wake.sh"

VM="${DBEAVER_VM:-macshot-vm}"
TASK=dbeaver-vm-shot

start=""
wait_for=2
click=""
keys=""
destination=""

while [ $# -gt 0 ]; do
    case "$1" in
    --start)
        start="$2"
        shift 2
        ;;
    --wait)
        wait_for="$2"
        shift 2
        ;;
    # Accumulated rather than replaced, so two presses can be asked for in one
    # run. Between two runs the window would be started again and whatever the
    # first press did would be gone.
    --click)
        click="${click:+$click;}$2"
        shift 2
        ;;
    --keys)
        keys="$2"
        shift 2
        ;;
    -*)
        echo "unknown option: $1" >&2
        exit 2
        ;;
    *)
        destination="$1"
        shift
        ;;
    esac
done

if [ -z "$destination" ]; then
    destination="${TMPDIR:-/tmp}/dbeaver-vm.png"
fi

if ! vm_wake "$VM"; then
    echo "cannot reach $VM over ssh. the setup steps are in vm-build.sh's header." >&2
    exit 1
fi

# A locked guest photographs as a blank rectangle rather than as an error. The
# task still runs, in the right session, on the Default desktop — but the screen
# is showing Winlogon's, so CopyFromScreen succeeds and copies nothing. The file
# it writes has the desktop's dimensions and a plausible size, and the only way
# to tell it from a real capture by eye is to notice that a Windows desktop does
# not normally have no taskbar. Caught here so it is a sentence instead.
if ssh "$VM" "MSYS_NO_PATHCONV=1 tasklist /fi 'IMAGENAME eq LogonUI.exe' /nh" 2>/dev/null |
    grep -qi logonui; then
    echo "$VM is at the lock screen, and a capture taken from behind it is blank." >&2
    echo "unlock the guest in UTM, then run this again." >&2
    exit 1
fi

home="$(ssh "$VM" 'echo $HOME')"
remote_script="$home/dbeaver-vm-shot.ps1"
remote_image="$home/dbeaver-vm-shot.png"

# One instance. Every run of this leaves a window open on the guest's desktop,
# and without this the fifth screenshot is of five overlapping windows with the
# oldest on top — which reads as the app having drawn the wrong thing.
if [ -n "$start" ]; then
    image_name="${start##*\\}"
    ssh "$VM" "MSYS_NO_PATHCONV=1 taskkill /f /im '$image_name'" >/dev/null 2>&1 || true
fi

# Rewritten every run rather than checked for. It is a few hundred bytes, and a
# stale copy would be a silently wrong answer instead of a loud one.
ssh "$VM" "cat > '$remote_script'" <<'PS1'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing

# Before anything asks how big the screen is. PowerShell is not DPI-aware, so on
# a scaled display Windows lies to it: the capture comes back at the virtualized
# size, softened by the scaler — the one thing a picture taken to judge glyph
# rendering must not be.
Add-Type -Namespace VmShot -Name Dpi -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
"@
[VmShot.Dpi]::SetProcessDPIAware() | Out-Null

# Read from a file rather than taken as parameters: a scheduled task's command
# line is fixed when the task is registered, so anything that varies per run has
# to arrive some other way.
$arguments = Get-Content (Join-Path $env:USERPROFILE "dbeaver-vm-shot.args") -ErrorAction SilentlyContinue
$Start = if ($arguments.Count -ge 1) { $arguments[0] } else { "" }
$Wait = if ($arguments.Count -ge 2 -and $arguments[1]) { [double]$arguments[1] } else { 2 }
$Click = if ($arguments.Count -ge 3) { $arguments[2] } else { "" }
$Keys = if ($arguments.Count -ge 4) { $arguments[3] } else { "" }

Add-Type -Namespace VmShot -Name Window -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr window);
"@

Add-Type -Namespace VmShot -Name Pointer -MemberDefinition @"
[DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
[DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint x, uint y, uint data, int extra);
"@

if ($Start) {
    # Brought to the front, because this guest is shared: another project's app
    # lives on the same desktop, and a photograph of whatever happened to be on
    # top is a photograph of that instead. Nothing is closed to make room — the
    # other window is somebody's, and covering it is enough.
    #
    # Waited for by its window rather than by the process. Start-Process returns
    # as soon as the process exists, which is before it has created anything
    # worth photographing, and a fixed sleep here is a race on a slow boot.
    $process = Start-Process -FilePath $Start -PassThru
    foreach ($attempt in 1..40) {
        Start-Sleep -Milliseconds 100
        $process.Refresh()
        if ($process.MainWindowHandle -ne 0) { break }
    }
    if ($process.MainWindowHandle -ne 0) {
        [VmShot.Window]::SetForegroundWindow($process.MainWindowHandle) | Out-Null
    }
}

if ($Click) {
    # Moved, then given a moment, then pressed. A press sent in the same breath
    # as the move arrives before the window has been told where the pointer is,
    # and lands on wherever it was before.
    foreach ($point in $Click.Split(";")) {
        $at = $point.Split(",")
        [VmShot.Pointer]::SetCursorPos([int]$at[0], [int]$at[1]) | Out-Null
        Start-Sleep -Milliseconds 120
        [VmShot.Pointer]::mouse_event(0x0002, 0, 0, 0, 0)
        [VmShot.Pointer]::mouse_event(0x0004, 0, 0, 0, 0)
        Start-Sleep -Milliseconds 200
    }
}

# After the pointer, because a key reaches whatever has focus and the click is
# what gives the window focus. Sent first it would go to whichever window the
# guest happened to be showing.
if ($Keys) {
    [System.Windows.Forms.SendKeys]::SendWait($Keys)
}

Start-Sleep -Seconds $Wait

$area = [System.Windows.Forms.SystemInformation]::VirtualScreen
$bitmap = New-Object System.Drawing.Bitmap $area.Width, $area.Height
$canvas = [System.Drawing.Graphics]::FromImage($bitmap)
$canvas.CopyFromScreen($area.Location, [System.Drawing.Point]::Empty, $area.Size)
$bitmap.Save((Join-Path $env:USERPROFILE "dbeaver-vm-shot.png"), [System.Drawing.Imaging.ImageFormat]::Png)
PS1

windows_script="$(ssh "$VM" "cygpath -w '$remote_script'")"

# scp's server side is a Windows binary and does not understand git-bash's /c/…
# form, so the copy at the end needs the path spelled the way Windows spells it.
windows_image="$(ssh "$VM" "cygpath -m '$remote_image'")"

# MSYS_NO_PATHCONV, because git's bash rewrites anything that looks like a POSIX
# path: /create and /tn arrive at schtasks as C:/Program Files/Git/create, and it
# fails on an argument nobody wrote.
#
# -WindowStyle Hidden, because the helper's own console would otherwise be on the
# desktop it is photographing, on top of the thing being looked at.
ssh "$VM" "MSYS_NO_PATHCONV=1 schtasks /create /tn $TASK \
    /tr 'powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File \"$windows_script\"' \
    /sc once /st 00:00 /it /f" >/dev/null

ssh "$VM" "printf '%s\n%s\n%s\n%s\n' '$start' '$wait_for' '$click' '$keys' \
    > '$home/dbeaver-vm-shot.args'"
ssh "$VM" "rm -f '$remote_image'; MSYS_NO_PATHCONV=1 schtasks /run /tn $TASK" >/dev/null

# Polled rather than slept for: the task is asynchronous, and a fixed sleep is
# either a wasted second or a race depending on how busy the guest is.
for _ in $(seq 1 60); do
    if ssh "$VM" "test -s '$remote_image'" 2>/dev/null; then
        break
    fi
    sleep 0.5
done

if ! ssh "$VM" "test -s '$remote_image'" 2>/dev/null; then
    echo "the capture task ran but produced nothing." >&2
    echo "is anyone logged in to the guest? /IT needs a session with a screen on it." >&2
    exit 1
fi

scp -q "$VM:$windows_image" "$destination"
echo "$destination"
