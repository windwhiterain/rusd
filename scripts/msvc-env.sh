# Source this file in Git Bash on the Windows peer before running cargo:
#
#   source scripts/msvc-env.sh
#
# Fixes the gongga runner's environment on lichen-vm. Final PATH order is:
#
#   MSVC bin  >  Git usr/bin  >  original PATH
#
# - MSVC first: rustc must find MSVC link.exe, not Git's coreutils link.exe.
# - Git usr/bin before the original PATH: GNU sort/find/ls must win over the
#   Windows System32 namesakes (sort.exe, find.exe), and the runner's PATH
#   lacks usr/bin entirely.
# - LIB/INCLUDE: a plain Git Bash has no vcvars; the MSVC target needs them.
#
# Auto-detects VS 2022, the newest MSVC toolset and the newest Windows SDK.

# --- locate Git's usr/bin (for ls/sort/cygpath) -----------------------------
_git_bin="$(command -v git || true)"
GIT_USR_BIN=""
case "$_git_bin" in
  */mingw64/bin/git*) GIT_USR_BIN="${_git_bin%/mingw64/bin/git*}/usr/bin" ;;
  */cmd/git*)         GIT_USR_BIN="${_git_bin%/cmd/git*}/usr/bin" ;;
  */usr/bin/git*)     GIT_USR_BIN="${_git_bin%/usr/bin/git*}/usr/bin" ;;
esac
if [ -n "$GIT_USR_BIN" ] && [ -d "$GIT_USR_BIN" ]; then
  export PATH="$GIT_USR_BIN:$PATH"
fi

# --- locate VS 2022 + newest toolset + newest SDK ---------------------------
_rusd_newest() { ls -1 "$1" 2>/dev/null | sort -V | tail -n1; }

VS=""
for edition in Community BuildTools Professional Enterprise; do
  p="/c/Program Files/Microsoft Visual Studio/2022/$edition"
  if [ -d "$p/VC/Tools/MSVC" ]; then VS="$p"; break; fi
done

if [ -n "$VS" ]; then
  MSVC_VER="$(_rusd_newest "$VS/VC/Tools/MSVC")"
  MSVC="$VS/VC/Tools/MSVC/$MSVC_VER"
  SDK='/c/Program Files (x86)/Windows Kits/10'
  SDK_VER="$(_rusd_newest "$SDK/Lib")"
  _w() { cygpath -w "$1"; }
  LIB="$(_w "$MSVC/lib/x64");$(_w "$SDK/Lib/$SDK_VER/ucrt/x64");$(_w "$SDK/Lib/$SDK_VER/um/x64")"
  INCLUDE="$(_w "$MSVC/include");$(_w "$SDK/Include/$SDK_VER/ucrt");$(_w "$SDK/Include/$SDK_VER/um");$(_w "$SDK/Include/$SDK_VER/shared")"
  export LIB INCLUDE
  export PATH="$MSVC/bin/Hostx64/x64:$PATH"
  echo "rusd env: MSVC $MSVC_VER, Windows SDK $SDK_VER" >&2
else
  echo "rusd env: no VS 2022 MSVC toolset found; cargo may fail to link" >&2
fi