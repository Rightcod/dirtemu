#!/usr/bin/env bash
# Runs victory.exe (or a cargo test binary) under Steam Proton on Linux,
# using vkd3d-proton (D3D12 -> Vulkan) and DXVK (DXGI).
# See docs/linux-proton.md.
#
#   scripts/proton-run.sh config
#   scripts/proton-run.sh precompile /path/to/game/<exe> --console series-s --jobs 3
#   scripts/proton-run.sh run /path/to/game/<exe> --console series-x --execute
#
# As cargo's Windows test runner:
#   CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUNNER=$PWD/scripts/proton-run.sh \
#       cargo xwin test --workspace --target x86_64-pc-windows-msvc
#
# Environment:
#   PROTON       Proton installation directory (default: Steam Proton Experimental / 11.0)
#   PREFIX       Proton prefix directory (default: ~/.local/share/dirtemu/proton)
#   VICTORY_EXE  exe to launch when the first argument is a victory subcommand
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"

find_proton() {
    local base name
    for base in \
        "$HOME/.steam/steam/steamapps/common" \
        "$HOME/.local/share/Steam/steamapps/common" \
        "$HOME/.var/app/com.valvesoftware.Steam/data/Steam/steamapps/common"; do
        for name in "Proton - Experimental" "Proton 11.0"; do
            if [[ -x "$base/$name/files/bin/wine" && -f "$base/$name/files/lib/wine/vkd3d-proton/x86_64-windows/d3d12core.dll" ]]; then
                printf '%s\n' "$base/$name"
                return 0
            fi
        done
    done
    return 1
}

if [[ -z "${PROTON:-}" ]]; then
    if ! PROTON="$(find_proton)"; then
        echo "Steam Proton (Experimental / 11.0) not found; install it in Steam or set PROTON=/path/to/Proton" >&2
        exit 1
    fi
fi
if [[ ! -x "$PROTON/files/bin/wine" ]]; then
    echo "Invalid PROTON directory: $PROTON (missing files/bin/wine)" >&2
    exit 1
fi

PREFIX="${PREFIX:-${XDG_DATA_HOME:-$HOME/.local/share}/dirtemu/proton}"

if [[ ! -f "$PREFIX/pfx/system.reg" ]]; then
    mkdir -p "$PREFIX"
    steam_root="${STEAM_COMPAT_CLIENT_INSTALL_PATH:-$(cd "$PROTON/../.." && pwd)}"
    STEAM_COMPAT_DATA_PATH="$PREFIX" \
    STEAM_COMPAT_CLIENT_INSTALL_PATH="$steam_root" \
    SteamAppId=0 SteamGameId=0 \
        python3 "$PROTON/proton" run cmd /c exit >/dev/null 2>&1 || true
fi

export WINEPREFIX="$PREFIX/pfx"
export WINEDLLOVERRIDES="d3d12,d3d12core,dxgi,d3d11,d3d10core=n,b;${WINEDLLOVERRIDES:-}"
export WINEDEBUG="${WINEDEBUG:--all}"
export WINEFSYNC=1
export VKD3D_SHADER_CACHE_PATH="${VKD3D_SHADER_CACHE_PATH:-$PREFIX/vkd3d-cache}"
mkdir -p "$VKD3D_SHADER_CACHE_PATH"

WINE="$PROTON/files/bin/wine"
if [[ -n "${VICTORY_EXE:-}" ]]; then
    exec "$WINE" "$VICTORY_EXE" "$@"
elif [[ "${1:-}" == *.exe && -f "${1:-}" ]]; then
    exec "$WINE" "$@"
else
    exec "$WINE" "$ROOT/target/x86_64-pc-windows-msvc/release/victory.exe" "$@"
fi
