#!/usr/bin/env bash
# Vendors a relocatable Linux svn client into the embedded-assets directory so
# the server ships a self-contained SVN backend.
#
# Strategy: bundle the system-installed `svn` plus the shared libraries it links
# (resolved via ldd) into:
#   crates/mortis-embed/assets/svn/linux-x86_64/{bin/svn, lib/*.so*}
# At runtime the server puts lib/ on LD_LIBRARY_PATH. After running, rebuild.
#
# Note: this produces a best-effort relocatable bundle. If it proves fragile on
# a target distro, simply leave the assets empty and rely on a system `svn`
# (the server falls back automatically).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
dest="$here/../crates/mortis-embed/assets/svn/linux-x86_64"
mkdir -p "$dest/bin" "$dest/lib"

svn_bin="$(command -v svn || true)"
if [[ -z "$svn_bin" ]]; then
  echo "error: no system svn found; install subversion first (e.g. apt install subversion)" >&2
  exit 1
fi

echo "Bundling svn from $svn_bin"
cp -L "$svn_bin" "$dest/bin/svn"

# Copy each non-system shared library svn depends on.
ldd "$svn_bin" | awk '/=> \// { print $3 }' | while read -r lib; do
  case "$lib" in
    /lib/ld-*|/lib64/ld-*|*/libc.so*|*/libpthread.so*|*/libdl.so*|*/libm.so*)
      # leave core libc/runtime to the host loader
      ;;
    *)
      cp -L "$lib" "$dest/lib/" 2>/dev/null || true
      ;;
  esac
done

echo "Done -> $dest"
echo "Contents:"
ls -1 "$dest/bin" "$dest/lib"
