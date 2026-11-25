#!/usr/bin/env bash
# Local CI parity: one command that runs every addon gate of
# .github/workflows/ci.yml (the `addon` job) with the same pinned tool
# versions. Run it from anywhere:
#
#   bash addon/scripts/ci_local.sh
#
# Gates and where they run (every one of them from addon/):
#   stylua --check .          tools/stylua (Windows binary, 2.6.0)
#   luacheck .                real luacheck 1.2.0 inside WSL (it reads
#                             .luacheckrc only from the current directory)
#   busted --verbose          real busted 2.2.0 inside WSL
#   TOC coverage guard        plain Lua 5.1 inside WSL (it walks the folder)
#   compat manifest guard     plain Lua 5.1 inside WSL
#   lua-language-server       tools/lls (Windows binary, 3.19.1); its report
#                             is judged by assert_lls_clean.lua inside WSL
#
# The Windows binaries come from scripts/bootstrap-tools.sh; the WSL rocks
# (luacheck 1.2.0, busted 2.2.0 for Lua 5.1) are user rocks under ~/.luarocks.
# Nothing here is vendored and nothing is downloaded.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ "$(uname -s)" == Linux* ]]; then
    # Already inside WSL/another Linux: run the Unix gates natively.
    eval "$(luarocks path --lua-version=5.1)"
    export PATH="$HOME/.luarocks/bin:$PATH"
    echo "== luacheck (pinned 1.2.0)"
    luacheck .
    echo "== busted (pinned 2.2.0)"
    busted --verbose
    echo "== TOC coverage"
    lua scripts/check_toc.lua AuctiMoggerDatabase/AuctiMoggerDatabase.toc
    echo "== compat manifest guard"
    lua scripts/check_compat.lua
    echo "== stylua + LuaLS are pinned as Windows binaries here (tools/, see bootstrap-tools.sh):"
    echo "   run those two gates from Windows, or in CI"
    exit 0
fi

STYLUA=tools/stylua/stylua.exe
LLS=tools/lls/bin/lua-language-server.exe
if [ ! -x "$STYLUA" ] || [ ! -x "$LLS" ]; then
    echo "error: $STYLUA and $LLS are required; run first: bash scripts/bootstrap-tools.sh" >&2
    exit 2
fi

command -v wsl.exe >/dev/null 2>&1 || {
    echo "error: WSL is required for the luacheck/busted/guard gates" >&2
    exit 2
}

# Translate the Windows cwd (D:/Dev/... ) to the WSL mount (/mnt/d/Dev/...).
win="$(pwd -W 2>/dev/null || pwd)"
lin="/mnt/$(printf '%s' "${win:0:1}" | tr 'A-Z' 'a-z')${win:2}"

# Run a command inside WSL at the addon mount, with the user rock tree active.
wsl_run() {
    wsl.exe -e bash -lc 'eval "$(luarocks path --lua-version=5.1)"; export PATH="$HOME/.luarocks/bin:$PATH"; cd "'"$lin"'" || exit 1; '"$1"
}

# Fail fast, with the fix, when the pinned rocks are not what CI installs.
rocks=$(wsl_run 'lua -v 2>&1 | sed -n 1p; luacheck --version 2>/dev/null | sed -n 1p; busted --version 2>/dev/null | sed -n 1p' | tr -d '\r') || {
    echo "error: cannot run commands inside WSL" >&2
    exit 2
}
lua_have=$(printf '%s\n' "$rocks" | sed -n 1p)
luacheck_have=$(printf '%s\n' "$rocks" | sed -n 2p)
busted_have=$(printf '%s\n' "$rocks" | sed -n 3p)
if [[ "$lua_have" != *"Lua 5.1"* || "$luacheck_have" != *"1.2.0"* || "$busted_have" != *"2.2.0"* ]]; then
    cat >&2 <<EOF
error: the Lua gates need Lua 5.1, luacheck 1.2.0 and busted 2.2.0 inside WSL (the CI pins).
  found: ${lua_have:-lua: not found}
         ${luacheck_have:-luacheck: not found}
         ${busted_have:+busted }${busted_have:-busted: not found}
Install the two rocks as the WSL user, then re-run:
  wsl -e bash -lc 'luarocks install --local luacheck 1.2.0-1'
  wsl -e bash -lc 'luarocks install --local busted 2.2.0-1'
EOF
    exit 2
fi

echo "== stylua --check (2.6.0)"
"$STYLUA" --check .

echo "== luacheck (pinned 1.2.0, WSL)"
wsl_run 'luacheck .'

echo "== busted (pinned 2.2.0, WSL)"
wsl_run 'busted --verbose'

echo "== TOC coverage (WSL)"
wsl_run 'lua scripts/check_toc.lua AuctiMoggerDatabase/AuctiMoggerDatabase.toc'

echo "== compat manifest guard (WSL)"
wsl_run 'lua scripts/check_compat.lua'

echo "== lua-language-server (3.19.1, checklevel=Warning)"
# Same invocation as ci.yml: stdout IS the report. (--out= writes a different
# summary file that assert_lls_clean cannot parse and that reports 0 problems
# even when stdout lists diagnostics.)
"$LLS" --check=. --checklevel=Warning > ./lls.log 2>&1 || true

echo "== assert_lls_clean (WSL)"
wsl_run 'lua scripts/assert_lls_clean.lua ./lls.log'

echo "ci_local: all gates passed"
