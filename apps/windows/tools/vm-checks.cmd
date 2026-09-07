@echo off
rem The guest side of vm-build.sh: set up the compiler, then hand over to the
rem same checks CI runs.
rem
rem A file rather than a command line, because the command line does not
rem survive. It has to cross local bash, ssh, the guest's bash and then cmd, and
rem cmd's rule for `/c` is to strip the first and last quote it sees — which
rem takes the quotes off `C:\Program Files (x86)\...` and breaks the path at its
rem first space. Every layer of escaping that would fix that is a layer that has
rem to be got right again on the next edit.
rem
rem   vm-checks.cmd            build the staticlib, then run the checks
rem   vm-checks.cmd --quick    reuse the staticlib that is already there
rem
rem Run it from this directory, by name, with no path on it. That is not a
rem preference — a path cannot be handed to `cmd` from the guest's bash. With
rem backslashes, MSYS strips them while assembling the Windows command line and
rem `apps\windows\tools\x.cmd` arrives as `appswindowstoolsx.cmd`; with forward
rem slashes, cmd reads `apps` as the command and `/windows` as a switch to it.
rem So the caller changes directory and this changes it back.

rem UTF-8, or the errors come back in the guest's ANSI code page and reach the
rem Mac as bytes that are not valid UTF-8 — which reads as a broken pipe rather
rem than as a compiler saying something in Chinese.
chcp 65001 >nul

rem The repository root, worked out from where this file is rather than from
rem where it was called: everything below is relative to it.
cd /d "%~dp0..\..\.." || exit /b 1

rem `-products *` is load bearing: without it vswhere only reports the IDE
rem editions and says nothing at all about a Build Tools install, which is what
rem this machine has. That silence looks exactly like Visual Studio being absent.
set "VSWHERE=%ProgramFiles(x86)%\Microsoft Visual Studio\Installer\vswhere.exe"
for /f "usebackq tokens=*" %%i in (`"%VSWHERE%" -products * -latest -property installationPath`) do set "VSROOT=%%i"

if not defined VSROOT (
  echo no Visual Studio found by vswhere 1>&2
  exit /b 1
)
echo Visual Studio: %VSROOT%

rem arm64 rather than x64: the guest's host and target are both arm64, and the
rem x64 script would set up a cross compile to an architecture nothing here
rem wants. CI still answers for x86_64.
rem
rem Not redirected to nul. vcvars is chatty on success, but on failure the only
rem thing it produces is that chatter, and a setup step that hides why it could
rem not set anything up leaves the next command failing for a reason that has
rem nothing to do with the next command.
call "%VSROOT%\VC\Auxiliary\Build\vcvarsarm64.bat" || exit /b 1

if not defined VCINSTALLDIR (
  echo vcvarsarm64 returned success and set nothing 1>&2
  exit /b 1
)

rem clang, on top of the MSVC that everything else here uses. `ring` asks cc-rs
rem for a compiler and then overrides it — `if target.os == WINDOWS && target.arch
rem == AARCH64 && !compiler.is_like_clang() { c.compiler("clang") }`, under a
rem FIXME — so on an arm64 guest the answer vcvars just gave is thrown away and
rem the build stops at `failed to find tool "clang"`. CI is x86_64 and never
rem reaches that branch.
rem
rem Prepended here rather than by changing the guest's system PATH: LLVM's
rem installer leaves itself out of it under winget's silent install, and one
rem machine-wide edit made by hand is a thing the next person cannot see. The
rem guard means an LLVM that is on PATH already wins.
where clang >nul 2>&1 || set "PATH=%ProgramFiles%\LLVM\bin;%PATH%"

rem `ring` a third time, and a third time only because this is arm64: there it
rem depends on `windows-sys` 0.52 and on x86_64 it depends on nothing of the
rem sort, so `dbffi.lib` here asks for `windows.0.52.0.lib` — an import library
rem that lives inside a crate in cargo's registry rather than in the Windows SDK.
rem `--print native-static-libs` names it without saying where it is and rustc
rem has no `--print` that would, so the directory is looked up and handed over
rem through LIB, which is the variable vcvars uses for exactly this. checks.sh
rem is left alone: on CI's x86_64 that library is not in the link line at all.
if not defined CARGO_HOME set "CARGO_HOME=%USERPROFILE%\.cargo"
for /f "delims=" %%d in ('dir /b /s /a:d "%CARGO_HOME%\registry\src\windows_aarch64_msvc-*" 2^>nul') do set "LIB=%LIB%;%%d\lib"

rem No debug info, and it is the archive format that forces this rather than a
rem wish for speed. With it, `dbffi.lib` here comes out at 5.4 GB — most of it
rem DuckDB's C++ — and past 4 GB LLVM writes the archive with a `/SYM64/` symbol
rem table, which `link.exe` does not read. What it does instead of failing is
rem `LNK4003: invalid library format; library ignored`, a warning, followed by
rem every FFI symbol coming back unresolved, so the error names the header and
rem never mentions the size. CI's x86_64 build is under the line today; nothing
rem announces it when it stops being.
set "CARGO_PROFILE_DEV_DEBUG=0"

if "%1"=="--quick" goto checks

rem cargo runs inside this environment, which is the opposite of what ci.yml
rem does — and both are right. There, MSVC on PATH lets Git's coreutils `link`
rem in `/usr/bin` win the lookup and cargo dies inside `libduckdb-sys`. cmd has
rem no `/usr/bin` on its PATH, so that trap is not reachable from here, and one
rem environment for both cargo and the checks is the simpler arrangement.
cargo build -p dbffi || exit /b 1
rem Under `target/` because `vm-build.sh` runs `git clean` on the way in, and an
rem untracked file at the root would be swept away between one run's build and
rem the next run's `--quick`. That directory is ignored, which is what keeps it.
cargo rustc -p dbffi --lib --crate-type staticlib -- --print native-static-libs >target\natives.txt 2>&1
if errorlevel 1 (
  type target\natives.txt
  exit /b 1
)

:checks
bash apps/windows/tools/checks.sh
