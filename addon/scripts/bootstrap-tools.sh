#!/usr/bin/env bash
# One-time bootstrap for the gitignored local CI toolchain (Windows host).
#
#   bash scripts/bootstrap-tools.sh
#
# Fetches the pinned Windows binaries scripts/ci_local.sh expects from the
# official GitHub releases, verifying pinned sha256 checksums:
#   StyLua 2.6.0               -> tools/stylua/stylua.exe
#   lua-language-server 3.19.1 -> tools/lls/ (bin/lua-language-server.exe + runtime)
#
# The Unix gates of ci_local.sh (luacheck 1.2.0, busted 2.2.0, plain Lua 5.1)
# run inside WSL. Nothing is vendored for them: this script only CHECKS that
# the two rocks are installed there and otherwise prints the commands to run.
#
# Idempotent: a tool already present at the pinned version is left alone.
set -euo pipefail
cd "$(dirname "$0")/.."

STYLUA_VERSION=2.6.0
STYLUA_URL="https://github.com/JohnnyMorganz/StyLua/releases/download/v${STYLUA_VERSION}/stylua-windows-x86_64.zip"
STYLUA_SHA256=fd13b4014e3b779c244bbc03f2c282e357f15bc526de6a6fa5ecbc447538313c

LLS_VERSION=3.19.1
LLS_URL="https://github.com/LuaLS/lua-language-server/releases/download/${LLS_VERSION}/lua-language-server-${LLS_VERSION}-win32-x64.zip"
LLS_SHA256=fdb9a59108cf62517813c97fa5549b0e16d1ef0688306bac728b08434db7e4cd

STYLUA_EXE=tools/stylua/stylua.exe
LLS_EXE=tools/lls/bin/lua-language-server.exe

command -v curl >/dev/null 2>&1 || {
    echo "error: curl is required (bundled with Git for Windows)" >&2
    exit 2
}

# sha256 of a file: sha256sum when present (Git Bash), else certutil (always
# available on Windows). Both are normalized to lower case.
sha256_of() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d' ' -f1
    else
        certutil -hashfile "$(cygpath -w "$1")" SHA256 | sed -n 2p | tr -d ' \r' | tr 'A-F' 'a-f'
    fi
}

unzip_into() {
    # unzip_into <zip> <target-dir>
    if command -v unzip >/dev/null 2>&1; then
        unzip -o -q "$1" -d "$2"
    else
        # PowerShell gets the paths through the environment, as Windows paths:
        # arguments that look like POSIX paths are rewritten on the way there.
        # The extraction goes straight through .NET instead of through the
        # Expand-Archive cmdlet: with PowerShell 7 installed, Windows PowerShell
        # 5.1 resolves that cmdlet to the 7.x module, fails with "Cannot find
        # an overload for ExtractToDirectory" - and still exits 0.
        ZSRC="$(cygpath -w "$1")" ZDST="$(cygpath -w "$2")" powershell.exe -NoProfile -NonInteractive -Command '
            $ErrorActionPreference = [System.Management.Automation.ActionPreference]::Stop
            try {
                try { Add-Type -AssemblyName System.IO.Compression.FileSystem } catch {}
                $zip = [System.IO.Compression.ZipFile]::OpenRead($env:ZSRC)
                try {
                    foreach ($entry in $zip.Entries) {
                        $target = [System.IO.Path]::Combine($env:ZDST, $entry.FullName)
                        if ([string]::IsNullOrEmpty($entry.Name)) {
                            [System.IO.Directory]::CreateDirectory($target) | Out-Null
                        } else {
                            [System.IO.Directory]::CreateDirectory([System.IO.Path]::GetDirectoryName($target)) | Out-Null
                            [System.IO.Compression.ZipFileExtensions]::ExtractToFile($entry, $target, $true)
                        }
                    }
                } finally {
                    $zip.Dispose()
                }
            } catch {
                [Console]::Error.WriteLine($_.Exception.Message)
                exit 1
            }
        '
    fi
}

# The version an installed tool prints, or nothing.
tool_version() {
    if [ -x "$1" ]; then
        "$1" --version 2>/dev/null | tr -d '\r' | head -n 1 || true
    fi
}

# The Windows binaries are useless on Linux; say so instead of downloading
# forty megabytes into a mount the tools could never run from.
if [[ "$(uname -s)" == Linux* ]]; then
    echo "bootstrap-tools: nothing to fetch on Linux (stylua and lua-language-server are Windows builds here)"
    echo "  the Lua 5.1 gates need, as this user:"
    echo "    luarocks install --local luacheck 1.2.0-1"
    echo "    luarocks install --local busted 2.2.0-1"
    exit 0
fi

# Downloads land in a private temp dir, never in the tree: a failed run must
# not leave a zip behind (the .gitignore stays free of download artifacts).
work=$(mktemp -d)
trap 'rm -f "$work"/*.zip; rmdir "$work" 2>/dev/null || true' EXIT

mkdir -p tools/stylua tools/lls

# install_tool <label> <version> <url> <sha256> <extract-into> <exe>
install_tool() {
    local label=$1 version=$2 url=$3 sha256=$4 into=$5 exe=$6
    local have
    have=$(tool_version "$exe")
    if [[ "$have" == *"$version"* ]]; then
        echo "[bootstrap-tools] $label $version already present: $exe"
        return
    fi
    local name
    name=$(basename "$url")
    local zip="$work/$name"
    echo "[bootstrap-tools] $label $version: downloading $name"
    curl -fL --retry 3 -o "$(cygpath -m "$zip")" "$url"
    local actual
    actual=$(sha256_of "$zip")
    if [ "$actual" != "$sha256" ]; then
        echo "error: sha256 mismatch for $name" >&2
        echo "  expected: $sha256" >&2
        echo "  actual:   $actual" >&2
        exit 1
    fi
    echo "[bootstrap-tools] $label $version: sha256 verified"
    unzip_into "$zip" "$into"
    rm -f "$zip"
    # Never report success on an extraction that silently produced nothing.
    local installed
    installed=$(tool_version "$exe")
    if [[ "$installed" != *"$version"* ]]; then
        echo "error: $exe does not report version $version after extraction (got: ${installed:-nothing})" >&2
        exit 1
    fi
    echo "[bootstrap-tools] $label $version: installed $exe -> $installed"
}

# The release zip holds stylua.exe at its root.
install_tool "stylua" "$STYLUA_VERSION" "$STYLUA_URL" "$STYLUA_SHA256" tools/stylua "$STYLUA_EXE"

# The release zip's root already has bin/ (exe + runtime dlls + main.lua);
# extracting it INTO tools/lls yields the tools/lls/bin/... layout ci_local.sh
# invokes.
install_tool "lua-language-server" "$LLS_VERSION" "$LLS_URL" "$LLS_SHA256" tools/lls "$LLS_EXE"

# --- WSL rocks: checked, never vendored, never downloaded silently ------------
if ! command -v wsl.exe >/dev/null 2>&1; then
    echo "[bootstrap-tools] NOTE: wsl.exe not found. ci_local.sh runs luacheck, busted and"
    echo "  the guards inside WSL; there they need, as the WSL user:"
    echo "    luarocks install --local luacheck 1.2.0-1"
    echo "    luarocks install --local busted 2.2.0-1"
else
    echo "[bootstrap-tools] checking the Lua 5.1 rocks inside WSL ..."
    # One line each: what `lua`, `luacheck` and `busted` report there, with the
    # user rock tree active exactly as ci_local.sh activates it.
    versions=$(wsl.exe -e bash -lc 'eval "$(luarocks path --lua-version=5.1)" 2>/dev/null; export PATH="$HOME/.luarocks/bin:$PATH"; lua -v 2>&1 | sed -n 1p; luacheck --version 2>/dev/null | sed -n 1p; busted --version 2>/dev/null | sed -n 1p' | tr -d '\r') || {
        echo "error: cannot run commands inside WSL" >&2
        exit 2
    }
    lua_have=$(printf '%s\n' "$versions" | sed -n 1p)
    luacheck_have=$(printf '%s\n' "$versions" | sed -n 2p)
    busted_have=$(printf '%s\n' "$versions" | sed -n 3p)
    if [[ "$lua_have" == *"Lua 5.1"* && "$luacheck_have" == *"1.2.0"* && "$busted_have" == *"2.2.0"* ]]; then
        echo "[bootstrap-tools] WSL: ${lua_have} / ${luacheck_have} / busted ${busted_have} - ready"
    else
        cat <<EOF
[bootstrap-tools] NOTE: the pinned Lua 5.1 tools are missing inside WSL.
  found: ${lua_have:-lua: not found}
         ${luacheck_have:-luacheck: not found}
         ${busted_have:+busted }${busted_have:-busted: not found}
  ci_local.sh runs luacheck and busted there. Install them as the WSL user:

    wsl -e bash -lc 'luarocks install --local luacheck 1.2.0-1'
    wsl -e bash -lc 'luarocks install --local busted 2.2.0-1'

EOF
    fi
fi

echo "bootstrap-tools: done - bash scripts/ci_local.sh runs the full gate"
