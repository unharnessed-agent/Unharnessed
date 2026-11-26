#!/bin/sh
# Builds the archive install.sh installs: the terminal app for one platform.
#   scripts/unharnessed-package.sh [target] [out-dir]
# `target` is one of: darwin-arm64 darwin-x64 linux-x64 linux-arm64 win32-x64
# (default: this machine; Windows builds under Git Bash, for install.ps1); `out-dir` defaults to target/package/. The agent is built for it with
# cargo, like vscode/scripts/package.mjs does (a platform other than this
# machine's needs its Rust target and linker), and archived with what it looks
# for beside itself, the same files the extension ships in its bin/:
#   unharnessed/unharnessed            the agent (unharnessed.exe on Windows)
#   unharnessed/package.json           the version `unharnessed --version` says
#   unharnessed/unharnessed-runtime/   the kernel's Python package
#   unharnessed/skills/                the built-in skills
#   unharnessed/LICENSE
# Of the runtime and the skills, the files git tracks are shipped.
# It writes <out-dir>/unharnessed-<target>.tar.gz and, beside it, its
# .sha256. The version is the extension's (vscode/package.json), whose
# release the archive is attached to.
#   UNHARNESSED_BIN=<file>           archive this binary, build nothing
#   UNHARNESSED_PACKAGE_VERSION=<v>  the version to say, instead
set -eu

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

this_machine() {
  case "$(uname -s)" in
    Darwin) system=darwin ;;
    Linux) system=linux ;;
    MINGW* | MSYS* | CYGWIN*) system=win32 ;;
    *) echo "no archive is built on $(uname -s)" >&2; exit 1 ;;
  esac
  case "$(uname -m)" in
    arm64 | aarch64) echo "$system-arm64" ;;
    x86_64 | amd64) echo "$system-x64" ;;
    *) echo "no archive is built for $(uname -m)" >&2; exit 1 ;;
  esac
}

target=${1:-$(this_machine)}
out=${2:-$repo/target/package}
case "$target" in
  darwin-arm64) triple=aarch64-apple-darwin ;;
  darwin-x64) triple=x86_64-apple-darwin ;;
  linux-x64) triple=x86_64-unknown-linux-musl ;;
  linux-arm64) triple=aarch64-unknown-linux-musl ;;
  win32-x64) triple=x86_64-pc-windows-msvc ;;
  *) echo "unknown target $target; one of: darwin-arm64 darwin-x64 linux-x64 linux-arm64 win32-x64" >&2; exit 1 ;;
esac
case "$target" in
  win32-*) exe=unharnessed.exe ;;
  *) exe=unharnessed ;;
esac

version=${UNHARNESSED_PACKAGE_VERSION:-$(sed -n 's/^  "version": "\(.*\)",$/\1/p' "$repo/vscode/package.json" | head -n 1)}
[ -n "$version" ] || { echo "no version in vscode/package.json" >&2; exit 1; }

built=${UNHARNESSED_BIN:-}
if [ -z "$built" ]; then
  # This machine's own platform builds into target/release, like `cargo build --release`.
  host=$(cd "$repo" && rustc -vV | sed -n 's/^host: //p')
  if [ "$triple" = "$host" ]; then
    (cd "$repo" && cargo build --release --locked -p uh-cli --bin unharnessed)
    built=$repo/target/release/$exe
  else
    (cd "$repo" && cargo build --release --locked -p uh-cli --bin unharnessed --target "$triple")
    built=$repo/target/$triple/release/$exe
  fi
fi
[ -f "$built" ] || { echo "no binary at $built" >&2; exit 1; }

mkdir -p "$out"
out=$(CDPATH='' cd -- "$out" && pwd)
stage=$(mktemp -d "${TMPDIR:-/tmp}/uh-package.XXXXXX")
trap 'rm -rf "$stage"' EXIT
app=$stage/unharnessed
mkdir "$app"
cp "$built" "$app/$exe"
chmod 755 "$app/$exe"
cp "$repo/LICENSE" "$app/LICENSE"
printf '{"name":"unharnessed","version":"%s"}\n' "$version" > "$app/package.json"
(cd "$repo" && git ls-files -- unharnessed-runtime/pyproject.toml unharnessed-runtime/src skills) | while IFS= read -r file; do
  mkdir -p "$app/$(dirname -- "$file")"
  cp "$repo/$file" "$app/$file"
done
[ -f "$app/unharnessed-runtime/pyproject.toml" ] || { echo "the kernel's runtime is not among the files git tracks" >&2; exit 1; }

archive=unharnessed-$target.tar.gz
# COPYFILE_DISABLE: macOS's tar would add a ._ file beside each file.
COPYFILE_DISABLE=1 tar -czf "$out/$archive" -C "$stage" unharnessed
if command -v sha256sum > /dev/null 2>&1; then
  (cd "$out" && sha256sum "$archive" > "$archive.sha256")
else
  (cd "$out" && shasum -a 256 "$archive" > "$archive.sha256")
fi
echo "$out/$archive"
