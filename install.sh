#!/bin/sh
# Installs Unharnessed for the terminal, on macOS and Linux:
#   curl -fsSL https://unharnessed.dev/install.sh | sh
# Run again, it updates. It downloads the release's archive for this machine
# (scripts/unharnessed-package.sh builds it), checks it against its SHA-256,
# and puts it in a folder of its own, named by its version:
#   ~/.local/share/unharnessed/<version>/   the agent, its kernel's runtime, the built-in skills
#   ~/.local/bin/unharnessed                a link to the agent
# The version a running session uses is kept until the next update. Then it
# builds the agent's Python kernel, which needs uv (https://docs.astral.sh/uv/):
# without uv it says how to get it, and the kernel is built at the first run.
# It writes nowhere else: PATH is yours to set, and it says so when it must be.
#
# Its options are in usage() below; after a pipe they follow `sh -s --`.
set -eu

REPOSITORY=unharnessed-agent/Unharnessed
UV_INSTALL='curl -LsSf https://astral.sh/uv/install.sh | sh'

usage() {
  cat << 'END'
Installs Unharnessed for the terminal, or updates it.

  sh install.sh [--version <v>] [--no-kernel]
  sh install.sh --uninstall

  --version <v>   the release to install (default: the latest)
  --no-kernel     do not build the Python kernel now
  --uninstall     remove the agent and its link; ~/.unharnessed (your
                  conversations, keys and settings) is kept

  UNHARNESSED_VERSION=<v>         like --version
  UNHARNESSED_INSTALL_DIR=<dir>   instead of ~/.local/share/unharnessed
  UNHARNESSED_BIN_DIR=<dir>       instead of ~/.local/bin
  UNHARNESSED_INSTALL_FROM=<url or folder>
                                  where the archives are, instead of the
                                  GitHub release
END
}

say() { printf '%s\n' "$*"; }
fail() {
  printf 'install.sh: %s\n' "$*" >&2
  exit 1
}
has() { command -v "$1" > /dev/null 2>&1; }

# The platform's name in the archive's: darwin-arm64, linux-x64, ...
platform() {
  case "$(uname -s)" in
    Darwin) system=darwin ;;
    Linux) system=linux ;;
    MINGW* | MSYS* | CYGWIN*) fail "on Windows, install with install.ps1 in PowerShell: irm https://raw.githubusercontent.com/$REPOSITORY/main/install.ps1 | iex" ;;
    *) fail "there is no build for $(uname -s)" ;;
  esac
  machine=$(uname -m)
  # A terminal under Rosetta says x86_64 on Apple silicon.
  if [ "$system" = darwin ] && [ "$machine" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2> /dev/null || true)" = 1 ]; then
    machine=arm64
  fi
  case "$machine" in
    arm64 | aarch64) say "$system-arm64" ;;
    x86_64 | amd64) say "$system-x64" ;;
    *) fail "there is no build for $system on $machine" ;;
  esac
}

# fetch <from> <to>: a URL downloaded, or a file copied.
fetch() {
  case "$1" in
    http://* | https://*)
      if has curl; then
        curl -fsSL --retry 2 -o "$2" "$1"
      elif has wget; then
        wget -q -O "$2" "$1"
      else
        fail "curl or wget is needed to download $1"
      fi
      ;;
    *) cp "$1" "$2" ;;
  esac
}

sha256() {
  if has sha256sum; then
    sha256sum "$1" | cut -d ' ' -f 1
  elif has shasum; then
    shasum -a 256 "$1" | cut -d ' ' -f 1
  else
    fail "sha256sum or shasum is needed to check the download"
  fi
}

# The folder a link of ours points into, or nothing.
linked() {
  [ -L "$link" ] || return 0
  to=$(readlink "$link")
  case "$to" in
    "$root"/*/unharnessed) dirname -- "$to" ;;
  esac
}

uninstall() {
  if [ -L "$link" ] && [ -n "$(linked)" ]; then
    rm -f "$link"
    say "Removed $link"
  elif [ -e "$link" ] || [ -L "$link" ]; then
    say "$link is not this installer's: left as it is"
  fi
  if [ -d "$root" ]; then
    rm -rf "$root"
    say "Removed $root"
  fi
  say "Your conversations, keys and settings are in ~/.unharnessed, and the Python kernel too: they are kept."
}

on_path() {
  case ":$PATH:" in
    *":$bin:"*) return 0 ;;
  esac
  return 1
}

# How to put the link's folder on PATH, in the shell the user logs into.
path_line() {
  case "$bin" in
    "$HOME"/*) named="\$HOME/${bin#"$HOME"/}" ;;
    *) named=$bin ;;
  esac
  case "${SHELL:-}" in
    */fish) say "  fish_add_path $bin" ;;
    */zsh) say "  echo 'export PATH=\"$named:\$PATH\"' >> ~/.zshrc && exec zsh" ;;
    */bash) say "  echo 'export PATH=\"$named:\$PATH\"' >> ~/.bashrc && exec bash" ;;
    *) say "  export PATH=\"$named:\$PATH\"" ;;
  esac
}

kernel() {
  uv=
  if has uv; then
    uv=$(command -v uv)
  elif [ -x "$HOME/.local/bin/uv" ]; then
    uv=$HOME/.local/bin/uv
  fi
  if [ -z "$uv" ]; then
    say
    say "The agent runs its Python in a kernel that uv builds, and uv is not installed. Get it with:"
    say "  $UV_INSTALL"
    say "The kernel is then built the first time the agent runs Python."
    return 0
  fi
  say "Building the Python kernel (once per version of its runtime)..."
  if ! (unset UNHARNESSED_PACKAGE_DIR; "$dest/unharnessed" --unharnessed-bootstrap) > "$incoming/kernel.log" 2>&1; then
    sed 's/^/  /' "$incoming/kernel.log" >&2
    say "The Python kernel is not built yet: the agent tries again the first time it runs Python."
  fi
}

main() {
  version=${UNHARNESSED_VERSION:-}
  with_kernel=1
  remove=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --version)
        [ $# -ge 2 ] || fail "--version takes a version, like 0.0.6"
        version=$2
        shift
        ;;
      --no-kernel) with_kernel=0 ;;
      --uninstall) remove=1 ;;
      -h | --help)
        usage
        exit 0
        ;;
      *) fail "unknown option $1 (--version <v>, --no-kernel, --uninstall)" ;;
    esac
    shift
  done
  [ -n "${HOME:-}" ] || fail "HOME is not set"
  root=${UNHARNESSED_INSTALL_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/unharnessed}
  bin=${UNHARNESSED_BIN_DIR:-$HOME/.local/bin}
  if [ -d "$root" ]; then root=$(CDPATH='' cd -- "$root" && pwd); fi
  if [ -d "$bin" ]; then bin=$(CDPATH='' cd -- "$bin" && pwd); fi
  link=$bin/unharnessed
  if [ "$remove" = 1 ]; then
    uninstall
    exit 0
  fi

  target=$(platform)
  has tar || fail "tar is needed to unpack the archive"
  archive=unharnessed-$target.tar.gz
  version=${version#v}
  if [ -n "${UNHARNESSED_INSTALL_FROM:-}" ]; then
    from=${UNHARNESSED_INSTALL_FROM%/}
  elif [ -n "$version" ]; then
    from=https://github.com/$REPOSITORY/releases/download/vscode-v$version
  else
    from=https://github.com/$REPOSITORY/releases/latest/download
  fi
  if [ -e "$link" ] && [ ! -L "$link" ]; then
    fail "$link is a file this installer did not put there: move it away, or set UNHARNESSED_BIN_DIR to another folder"
  fi

  # Unpacked inside the install's folder: moving it into place is then one rename.
  mkdir -p "$root" "$bin"
  root=$(CDPATH='' cd -- "$root" && pwd)
  bin=$(CDPATH='' cd -- "$bin" && pwd)
  link=$bin/unharnessed
  incoming=$root/.incoming.$$
  rm -rf "$incoming"
  mkdir "$incoming"
  # A refused first install leaves no folder of its own (rmdir removes it only empty).
  trap 'rm -rf "$incoming"; rmdir "$root" 2> /dev/null || true' EXIT
  trap 'exit 130' INT
  trap 'exit 143' TERM

  say "Downloading $from/$archive"
  if [ -n "$version" ]; then release="release $version"; else release="the latest release"; fi
  fetch "$from/$archive" "$incoming/$archive" \
    || fail "no archive at $from/$archive: $release has no terminal build for $target"
  fetch "$from/$archive.sha256" "$incoming/$archive.sha256" \
    || fail "no checksum at $from/$archive.sha256"
  wanted=$(cut -d ' ' -f 1 < "$incoming/$archive.sha256")
  got=$(sha256 "$incoming/$archive")
  [ -n "$wanted" ] && [ "$wanted" = "$got" ] \
    || fail "the archive is not the one the release published (SHA-256 $got, expected ${wanted:-none}): nothing was installed"

  mkdir "$incoming/unpacked"
  tar -xzf "$incoming/$archive" -C "$incoming/unpacked" || fail "the archive does not unpack"
  app=$incoming/unpacked/unharnessed
  [ -f "$app/unharnessed" ] && [ -f "$app/unharnessed-runtime/pyproject.toml" ] && [ -d "$app/skills" ] \
    || fail "the archive is not an Unharnessed build: nothing was installed"
  # The build says its own version, which also shows it runs on this machine.
  said=$(unset UNHARNESSED_PACKAGE_DIR; "$app/unharnessed" --version 2> "$incoming/version.err") \
    || fail "this build does not run on this machine ($target): $(head -n 3 "$incoming/version.err")"
  case "$said" in
    "" | *[!0-9A-Za-z.+-]*) fail "the build says a version that names no folder: '$said'" ;;
  esac
  if [ -n "$version" ] && [ "$said" != "$version" ]; then
    fail "asked for version $version, the archive at $from is $said: nothing was installed"
  fi

  before=$(linked)
  dest=$root/$said
  if [ -e "$dest" ]; then
    mv "$dest" "$incoming/replaced"
  fi
  mv "$app" "$dest"
  ln -s "$dest/unharnessed" "$incoming/link"
  mv -f "$incoming/link" "$link"

  # The version the link named until now may be running: it stays until the next update.
  for other in "$root"/*/; do
    other=${other%/}
    [ -f "$other/unharnessed" ] || continue
    [ "$other" = "$dest" ] || [ "$other" = "$before" ] || rm -rf "$other"
  done

  if [ "$before" = "$dest" ]; then
    say "Unharnessed $said is installed again in $dest"
  elif [ -n "$before" ]; then
    say "Unharnessed $said is installed in $dest (it was $(basename -- "$before"))"
  else
    say "Unharnessed $said is installed in $dest"
  fi
  [ "$with_kernel" = 0 ] || kernel

  say
  if on_path; then
    seen=$(command -v unharnessed || true)
    if [ -n "$seen" ] && [ "$seen" != "$link" ]; then
      say "Another unharnessed comes first on your PATH: $seen"
      say "This one is $link"
    else
      say "Run it in a project's folder:  unharnessed"
    fi
  else
    say "$bin is not on your PATH. Add it:"
    path_line
    say "Then run it in a project's folder:  unharnessed"
  fi
}

# Nothing runs before the closing brace is read: a download cut short does nothing.
{ main "$@"; }
