# Vendored dependency: voxrt

**Upstream:** https://github.com/lmaksym/voxrt
**Commit:** `4fab626` (2026-10-12)
**Version:** 0.2.1
**Licence:** Apache-2.0

Vendored because voxrt is a single-maintainer project (63 commits, 1 star at the time
of writing) and is the entire speech engine of this application. Pinning means an
upstream force-push, deletion or behavioural change cannot break a shipped build.

Only `crates/voxrt` (the library) is vendored. The CLI, FFI and Python bindings are
not used: `crates/sotto-stt` links the library directly.

**Do not edit vendored files.** If a local patch is needed, record it here with the
reason and re-apply it on upgrade.

## Local patches

- **2026-11-23 — workspace members reduced to `crates/voxrt`.** Only the library is
  vendored; the upstream workspace lists `crates/voxrt-cli`, `crates/voxrt-ffi` and
  `bindings/python`, which are not shipped. Their missing manifests made tools that
  walk nested workspaces (`cargo fmt`, `cargo metadata`) fail. Nothing in the
  library itself was changed.
