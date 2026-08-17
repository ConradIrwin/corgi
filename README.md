# dcargo — proof of concept

A Cargo.toml-compatible build tool for Rust that makes builds **deterministic**
(bazel-style), **lock-free** under concurrency, and cached in a **central
content-addressed store** shared by any number of worktrees.

```
dcargo build [--dir DIR] [-v]      # store at ~/.cache/dcargo, override with $DCARGO_STORE
```

## How it works

Cargo is used only for what it is genuinely good at — resolution (`cargo fetch`
+ `cargo metadata --filter-platform`). Compilation is planned and executed by
dcargo, invoking `rustc` directly so that every input is explicit.

The dependency graph is decomposed into **units**: lib compile, proc-macro
compile, build.rs compile, build.rs *run*, and bin compile+link. Each unit is
an **action** keyed by a SHA-256 over a canonical JSON of every input:

- rustc verbose version + host triple + tool version
- package identity (name, version, registry source — never a local path)
- a Merkle hash of the package's source tree: **relative paths + file bytes
  only** (mtimes, inode numbers, absolute paths never enter the key)
- resolved features, edition, crate type, profile flags
- the **content hashes of dependency artifacts** (keys are recursive)
- build-script outputs consumed (cfgs, envs, link flags, OUT_DIR key)
- the exact environment the action will see

Results live in a machine-wide store:

```
~/.cache/dcargo/
  cas/<sha256>          content-addressed artifacts (rlibs, dylibs, bins)
  actions/<key>.json    action key -> output hashes + build-script directives
  pool/lib<name>-<key16>.rlib   hardlinks into cas, named for rustc -L/--extern
  outdirs/<key>/out     published build.rs OUT_DIRs at stable paths
  tmp/                  scratch; everything is published by atomic rename
```

### The three blockers, addressed

| blocker | fix |
|---|---|
| cargo keys its cache on mtimes | action keys are pure functions of file *content* and dependency *content hashes* |
| build.rs | compiled and run as two separately cached actions; stdout directives parsed & stored; OUT_DIR published at a stable content-keyed path |
| env! inclusions | rustc and build scripts run with `env_clear()` + an explicit env that is part of the action key; untracked env vars simply do not exist for the compiler |

### Determinism details

- compiles run with `cwd = package root` and a *relative* source path, plus
  `--remap-path-prefix` for cargo-home/workspace/package roots
- `-Cmetadata`/`-Cextra-filename` derive from the action key (stable symbol
  hashes, unique artifact file names — no collisions in the shared pool)
- `-Cdebuginfo=0` for now (dodges macOS .dSYM/OSO path issues)
- on macOS the proc-macro dylib install name is pinned
  (`-Wl,-install_name,/dc/...`) — ld64 otherwise embeds the temp output path
- verified: two cold builds in different directories with different store
  locations produce **bit-identical** binaries

### Lock-freedom

There are no locks and no daemon. Every store mutation is
write-to-temp-then-`rename()` (atomic on POSIX):

- CAS insert: rename over an identical file is harmless
- action publish: last writer wins with identical content
- pool hardlink / OUT_DIR publish: `EEXIST` means someone else won the race —
  use theirs

Two concurrent cold builds of the same tree both succeed; in the worst case
they duplicate some work, they never block or corrupt. (Verified empirically.)

## Verified behaviour

- re-saving files without changes (new mtimes/inodes): `0 executed, 17 cached`
  (stock cargo recompiles)
- same repo cloned to a different directory: `0 executed`, byte-identical bin
- fresh empty store, different directory: full rebuild, **byte-identical bin**
- two simultaneous cold builds sharing a store: both succeed, cross-pollinate
- edit + revert: revert is free — the old artifacts are still in the CAS

## Sandboxing (always on, macOS)

Every rustc invocation and build-script run executes under a deny-by-default
seatbelt profile (`sandbox-exec`): reads limited to system dirs, the
toolchain, the store, and the package being built; writes limited to the
action's own out/scratch dirs; no network. Children (cc, ld, rustc probes)
inherit the sandbox, and since proc-macros run *inside* the sandboxed rustc,
they are confined too. Undeclared inputs turn from silent impurities into
loud `PermissionDenied` build errors.

Measured overhead (M-series MBP): +4.5ms per process spawn, ~3% on a pure
compile, ~1% on a full cold build — so it is enabled unconditionally
(`DCARGO_NO_SANDBOX=1` exists as a debugging kill-switch only). Two macOS
gotchas cost the initial integration 4x: xcrun's SDK lookup needs the
*canonical* darwin per-user temp/cache dirs (`/private/var/folders/...`)
writable or every link takes a ~1.5s uncached fallback, and we now resolve
`SDKROOT` once up front instead of letting each link shell out to xcrun
(which was also an untracked input). Known hole: those darwin cache dirs are
shared mutable state; Bazel/Nix accept the same tradeoff.

## Canonical OUT_DIR paths (store alias + staging byte-patch)

Seatbelt cannot remount paths (it is allow/deny only; macOS has no per-process
mount namespaces), so path canonicalization is done with two tricks instead:

1. **Canonical store location**: the store lives directly at
   `/Users/Shared/dcargo` (world-writable, admin-free, same path on every
   Mac), so embedded OUT_DIR strings are machine-independent with no
   indirection at all — nothing for `realpath()` to see through. Relocating
   the store (`DCARGO_STORE=...`) falls back to a symlink alias at the
   canonical path, and the two modes produce bit-identical artifacts.
   Upgrade path: an `/etc/synthetic.conf` firmlink (`/dcargo`, the Nix
   approach) for a root-level name. Note `/Users/Shared` is world-writable:
   fine for a single-dev machine, but a genuinely multi-user store wants a
   root-owned location plus a daemon (the Nix model).
2. **Staging byte-patch**: build scripts run with OUT_DIR at
   `outdirs/<random-64-hex>/out` — the same length as the final
   `outdirs/<action-key>/out`. Before the atomic publish, generated files are
   byte-patched `staging-id -> key` (length-preserving, safe for binary
   files), so paths embedded *during* the run also become canonical, without
   giving up lock-free publishing.

Verified: a crate whose build.rs bakes OUT_DIR into generated code now
produces bit-identical binaries from stores at different locations, and the
embedded path reads `/Users/Shared/dcargo/outdirs/<key>/out` everywhere.

## Exec allowlist = the cache key (toolchain identity)

The sandbox only permits executing binaries that are either **dispatchers**
(`/usr/bin/cc`, the rustup shim — they pick a tool but do not shape output)
or **keyed tools**: rustc (via `rustc -vV`), build-script binaries (via
content hash), and the Xcode clang/ld + SDK (via `cc --version`, `ld -v`,
`xcrun --show-sdk-version`, hashed into linking and build-script action
keys). Anything else is `EPERM`. `xcrun` no longer runs during builds at all
(SDKROOT is resolved once up front). Verified: the allowlisted build
produces bit-identical artifacts to an unrestricted one.

Cargo `[profile.*]` tables and `.cargo/config.toml` (RUSTFLAGS etc.) are not
read yet — only built-in dev/release profiles exist; when supported, every
knob folds into the action key like any other input.

## Known gaps / future work (PoC scope)

- toolchain identity beyond `rustc -vV` is not hashed (cc/ld versions, PATH);
  a real version needs a hermetic toolchain definition à la bazel
- OUT_DIR paths and `CARGO_MANIFEST_DIR` are stable per-machine but not
  across machines; for S3 sync, OUT_DIRs should be tarred into the CAS and
  path-rewritten (or `--env-set` used on nightly), and registry sources
  addressed by their Cargo.lock checksum instead of an extracted dir hash
- build scripts run unsandboxed (their *inputs* are hashed conservatively —
  the whole package dir — but a hostile script could read outside it);
  `rerun-if-changed` narrowing is intentionally ignored (content hashing
  supersedes it, conservatively)
- one fixed profile (opt-level=0, no debuginfo); no `--release` flag yet
- no target-platform cfg evaluation beyond `--filter-platform`, no dev-deps /
  tests / cdylibs, single rustc invocation per crate (no pipelining), warnings
  from cached dep actions are not replayed
- `links`/DEP_* env propagation implemented but untested (symbolicate has no
  native deps); `-L` from build scripts propagates to the final link
