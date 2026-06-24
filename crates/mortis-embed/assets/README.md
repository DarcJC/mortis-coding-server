# Embedded SVN binaries

This directory is embedded into the `mortis-code-server` executable at build
time (via `rust-embed`). At runtime the per-OS subdirectory is extracted to a
cache directory and used to run `svn`, so the server is self-contained.

Layout (one subdirectory per platform tag `"<os>-<arch>"`):

```
assets/svn/
  windows-x86_64/     svn.exe + required *.dll (e.g. from SlikSVN)
  linux-x86_64/       bin/svn + lib/*.so (a relocatable build)
```

The executable must be named `svn.exe` (Windows) or `bin/svn` (Linux). On Linux
the accompanying shared libraries are placed on `LD_LIBRARY_PATH`; on Windows the
extracted directory is prepended to `PATH` so the DLLs resolve.

If a platform subdirectory contains no `svn` executable (as shipped here — only
this README and `.gitkeep` files are committed), the server falls back to a
system-installed `svn` on `PATH`. Populate these directories (see
`scripts/fetch-svn.*`) to make SVN support fully self-contained.
