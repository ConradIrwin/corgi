# corgi — proof of concept

A Cargo.toml-compatible build tool for Rust that makes builds **deterministic**
(bazel-style), **lock-free** under concurrency, and cached in a **central
content-addressed store** shared by any number of worktrees.

```
corgi build [--dir DIR] [-p PACKAGE] [-v] # store at ~/.cache/corgi, override with $CORGI_STORE
corgi fmt [--dir DIR] [-p PACKAGE] [--workspace] [--check] [-- RUSTFMT_ARGS...]
```

Installing corgi requires Rust 1.90 or newer:

```
cargo install corgi-build
```

## How it works

Cargo is used only for what it is genuinely good at — resolution (`cargo fetch`
+ `cargo metadata --filter-platform`). Compilation is planned and executed by
corgi, invoking `rustc` directly so that every input is explicit.

`corgi fmt` is intentionally lighter weight: it installs the SHA-256-verified
`rustfmt-preview` component matching the exact project toolchain, then delegates
target discovery and formatting to that pinned `cargo fmt`. It does not resolve
the build graph or interact with the build cache.

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
/Users/Shared/corgi/
  cache/<xx>/<sha256>   content-addressed artifacts (rlibs, dylibs, bins)
  cache/<xx>/<key>.json action records in the same tree, Go-style: name is the
                        INPUT key, content is the answer (outputs, directives)
  pool/lib<name>-<key16>.rlib   hardlinks into cache, named for rustc -L/--extern
  outdirs/<key>/out     build.rs OUT_DIRs, built in place; .ok sentinel = done
  tools/<name>-<ver>/   every pinned immutable component: [tools] downloads,
                        rust toolchains (rust-<ver>-<host>), and per-target
                        std (rust-std-<ver>-<target>, reached via bare -L --
                        never mutating the toolchain dir)
  hints/                stat-hint memo files (source/export); pure
                        accelerators, freely deletable, never affect keys
  tmp/                  scratch; everything is published by atomic rename
```

### The three blockers, addressed

| blocker | fix |
|---|---|
| cargo keys its cache on mtimes | action keys are pure functions of file *content* and dependency *content hashes* |
| build.rs | compiled and run as two separately cached actions; stdout directives parsed & stored; OUT_DIR published at a stable content-keyed path |
| env! inclusions | rustc and build scripts run with `env_clear()` + an explicit env that is part of the action key; untracked env vars simply do not exist for the compiler |

### Determinism details

- compiles run with `cwd = package root` and a *relative* source path; the
  workspace root is remapped to `.`, so debug info carries workspace-relative
  paths (lldb run from the workspace root needs no source-map)
- dependency sources live in the store itself (`cargo-home/` under the
  canonical store path): their real paths are machine-independent, so they
  need no remapping and debuggers step into deps with no configuration
- `-Cmetadata`/`-Cextra-filename` derive from the action key (stable symbol
  hashes, unique artifact file names — no collisions in the shared pool)
- full debug info on target-side units under dev; host-side units (build
  scripts, proc-macros, their exclusive deps) stay at zero like cargo's
  build-override default. Darwin linking units compile with
  `-Csplit-debuginfo=unpacked` and `-Wl,-oso_prefix` so debug-map entries
  are recorded relative to the workspace root; the CGU objects export next
  to the binary (`target/debug/*.rcgu.o`, cargo's own convention). Modern
  ld records zero OSO timestamps, so gc's use-touching never stales them
- std sources install as a side-car (`tools/rust-src-<ver>`, never inside
  the sysroot — rustc would devirtualize std paths and flip every key).
  Everything you write and depend on debugs with zero configuration; for
  source display *inside* std add the one optional line (editors can
  inject it automatically):

  ```
  settings set target.source-map /rustc/$(rustc -vV | sed -n 's/commit-hash: //p') \
      /Users/Shared/corgi/tools/rust-src-<ver>/lib/rustlib/src/rust
  ```
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

### Warm-build fast path (plan cache + stat hints)

A no-op build must feel instant, so the expensive warm-path work is cached
at two layers:

- **Plan cache**: `cargo fetch` + `cargo metadata` + `cargo build --unit-graph`
  (~0.8s even when nothing changed) are skipped entirely when nothing that
  shapes them changed. A pointer keyed on toolchain/target/profile and the
  build-dir + tools manifests leads to an entry that re-validates the rest by
  content: every workspace manifest, `Cargo.lock`, `.cargo/config*`, and the
  member-glob directory listings (so a new `crates/*` subdir invalidates).
  The cached cargo JSON lives in the CAS; entry paths are workspace-relative
  and absolute paths inside the JSON are re-rooted on load, so a bit-identical
  checkout in a different directory shares the plan.
- **Source-hash hints**: per-package Merkle hashing consults a per-directory
  hint file (size/mtime/inode per file) and only re-reads files whose stat
  changed — same digest as a full read, just cheaper. Registry/git package
  hashes are immutable and cached once, keyed by `source|name|version`.

Lockfile handling never bothers the user: cargo runs with `--locked` (which
never writes), and if it rejects a missing/stale `Cargo.lock`, corgi reruns
once unlocked so cargo brings the lock up to date, then continues — the plan
fingerprint hashes the lock *after* that step. corgi itself never writes the
lock.

- **Export hints**: exported artifacts (the ~1 GiB zed binary) are verified
  by a stat hint instead of a full content re-read; tampered or deleted
  outputs are still detected and repaired from the CAS.

Measured no-op: cloud worker (465 units) 1.9s -> **0.11s**; zed (1,678 units)
3.7s -> **0.15s**. `CORGI_TIMING=1` prints a phase/hash-work breakdown.

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
(no opt-out: one mode, hard errors). Two macOS
gotchas cost the initial integration 4x: xcrun's SDK lookup needs the
*canonical* darwin per-user temp/cache dirs (`/private/var/folders/...`)
writable or every link takes a ~1.5s uncached fallback, and we now resolve
`SDKROOT` once up front instead of letting each link shell out to xcrun
(which was also an untracked input). Known hole: those darwin cache dirs are
shared mutable state; Bazel/Nix accept the same tradeoff.

## Canonical OUT_DIR paths (in-place builds + flock + sentinel)

Seatbelt cannot remount paths (it is allow/deny only; macOS has no per-process
mount namespaces), so path canonicalization is done differently:

1. **Canonical store location**: the store lives directly at
   `/Users/Shared/corgi` (world-writable, admin-free, same path on every
   Mac), so embedded OUT_DIR strings are machine-independent with no
   indirection at all — nothing for `realpath()` to see through. Relocating
   the store (`CORGI_STORE=...`) falls back to a symlink alias at the
   canonical path, and the two modes produce bit-identical artifacts.
   Upgrade path: an `/etc/synthetic.conf` firmlink (`/corgi`, the Nix
   approach) for a root-level name. Note `/Users/Shared` is world-writable:
   fine for a single-dev machine, but a genuinely multi-user store wants a
   root-owned location plus a daemon (the Nix model).
2. **In-place OUT_DIR builds**: build scripts run directly at the final
   `outdirs/<action-key>/out`, so every path a tool embeds — literally or
   *derived* — is the canonical location on every machine. (An earlier
   design staged under a random dir and byte-patched embedded path strings
   before an atomic publish; the zed audit killed it: cc names objects by a
   *hash* of their input path, and no string patch can rewrite a hash.)
   Atomicity comes from a `.ok` sentinel written last (atomic rename); a
   dir without the sentinel is a crash leftover and is wiped by the next
   builder. Mutual exclusion comes from `flock`, which the kernel releases
   on any process death — clean exit, panic, SIGKILL — so no stale-lock
   state can survive a crash. The lock is per-action: it is only ever
   contended by a concurrent build doing the identical work, and the loser
   wakes up to a finished sentinel (verified: two simultaneous cold builds
   sharing a fresh store, plus kill-recovery by sentinel deletion).

Verified: a crate whose build.rs bakes OUT_DIR into generated code produces
bit-identical binaries from stores at different locations, and the embedded
path reads `/Users/Shared/corgi/outdirs/<key>/out` everywhere — including
inside cc-generated archive member names, which the byte-patch era got wrong.

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

## Toolchain provenance

Pinning is mandatory and concrete: `rust-toolchain.toml` must name an exact
version (`1.94.1`, `nightly-2026-03-25`); floating channels and missing pins
are hard errors, and there is no ambient-rustup fallback. corgi installs
rustc + rust-std + cargo from static.rust-lang.org into the store
(sha256-verified, atomic rename), so a machine needs no Rust installed at
all. The host triple is resolved from corgi's own build constants (needed
before a rustc exists, to pick tarballs) and cross-checked against the
pinned rustc's self-reported `host:`.

A toolchain can be installed straight into the store from
static.rust-lang.org (sha256-verified, no rustup) and used via `RUSTC=`;
sandbox rules, remaps, and keys adapt automatically. Found in the process:
identical rustc *versions* with different sysroot *content* (rust-src
installed vs not) emit different bits — 13/14 artifacts — under identical
keys. Sysroot content identity (rust-src presence) is now folded into every
action key. Managed toolchains without rust-src are preferable: std paths
stay in upstream's canonical `/rustc/<commit>/` form on every machine.

## Scoped workspace settings (corgi.toml)

One optional file at the workspace root; every setting names the packages
it applies to, and **only those packages' actions key on it** — the file
itself is never hashed into keys, so comments, reordering, or probe
command-text edits rebuild nothing (and don't even cost a replan; only
`[universe.*]` shapes the plan).

```toml
[tools.cmake]              # sha256-pinned, unpacked into the store
version  = "4.1.2"
url      = "https://github.com/Kitware/CMake/releases/..."
sha256   = "3be85f..."
bin      = "cmake-4.1.2-macos-universal/CMake.app/Contents/bin/cmake"
env      = "CMAKE"
packages = ["webrtc-sys", "wasmtime-c-api-impl"]   # omit = every build script

[env.ZED_COMMIT_SHA]       # plan-time probe, run outside the sandbox
command  = "git rev-parse HEAD"
packages = ["zed", "cli", "remote_server"]          # required
profiles = ["release"]                              # omit = all profiles

[universe.wasm32-unknown-unknown]                   # feature unification set
packages = ["cloud_worker", "github_worker"]
```

Blast radii by construction: bump a tool pin and exactly the scoped
packages' build scripts re-run; a probe's *value* keys the scoped actions
(its command text never does), and a release-scoped probe doesn't run at
all under dev — so a new git commit can't bust the dev cache. Bare-name
spawns (`Command::new("cmake")`) resolve through a per-subset shim dir on
the action PATH. Per-crate settings live elsewhere, in each crate's own
manifest: `[package.metadata.corgi] extra-inputs = [...]`.

## The dev loop: incremental namespace, jobserver, timings

Local units compile with rustc's own -Cincremental against store-managed
state, in a separate key namespace (incr:true): functionally equivalent,
not bit-reproducible, never mixed with the clean namespace that audit
(and CI, via --no-incremental) builds in. This required -Cmetadata to
become a source-free unit identity (cargo's own scheme) so symbol hashes
survive edits; -Cextra-filename keeps the full key16 for unique pool
names, with SVH disambiguating same-identity candidates.

A GNU-make jobserver (rustc participates natively; build scripts inherit
it, so cc/cmake cooperate) caps machine-wide codegen threads at ~NCPU —
without it, 18 concurrent rustcs at codegen-units=16 meant ~290 runnable
threads on 18 cores and ~2x per-unit inflation. Blob hashing uses the
sha2 crate's hardware intrinsics (~2.6 GB/s measured; 5x over software).

`--timings` writes target/corgi-timings/corgi-timing-<ts>.html: a
gantt of executed units with front-end/codegen splits, per-unit phase
columns (rustc, ingest bytes+time, key, cache, validate), phase totals,
and a top-5 stderr summary. Measured editor-edit loop on zed after all
of the above: 19.4-23.6s vs cargo's 13.0-18.5s (was 27-53s), with the
edited crate and the final link at or better than cargo's times; the
residual gap is dependents' codegen reuse, under investigation.

## Lints and clippy

`[lints]` / `[workspace.lints]` are resolved by corgi at plan time
(cargo exposes them through no API — checked: metadata, unit graph, and
build-plan, which is removed) and treated as inputs like any probe value:
the manifests never enter keys, only the resolved flags do, per member.
Rust-tool lints key every mode; clippy:: lints key clippy actions only —
they're inert to rustc, so editing them never busts check or build
caches (cargo can't say the same).

`corgi clippy` is check mode with clippy-driver as the executor for
local packages' checked units, in their own key namespace (`--cfg
clippy` is code-visible, so sharing rmetas with check would be a lie);
the whole dependency layer shares check's rmetas. clippy.toml is a
hashed input (clippy-driver reports it in dep-info; verified against the
hermeticity validator). Diagnostics replay from the store: a warm
full-workspace `corgi clippy` on zed is 0.2s, and check/clippy
alternation is 0-executed in both directions — no fingerprint thrash.

## Root-manifest blast radius (readable = keyed, by construction)

Actions cannot read the workspace root Cargo.toml at all, and it is hashed
into nothing. Every section reaches builds only through cargo's *resolved*
outputs — metadata, lockfile, per-unit profiles in the unit graph — which
key exactly the units they affect. Measured on zed (1135-line root
manifest, 1678 units): a comment edit or a `[workspace.metadata.*]` tweak
rebuilds **nothing**; `[profile.dev.package.zed] codegen-units = 8`
re-executes exactly the zed package's 3 units. A crate that genuinely
needs the raw manifest bytes fails hard (sandbox deny + dep-info
validation) and declares them via `extra-inputs`, which hashes the file
into exactly that crate. Probed empirically: zero such readers in zed or
cloud today.

## Input containment (include!/mod coverage)

Everything a compile can read is either hashed or refused. In-package reads
(`mod`, `include!`, `include_str!`, `include_bytes!`) are covered by the
whole-package content hash; `OUT_DIR` reads are keyed transitively via the
build-script action key; `env!` reads are keyed because the env is explicit
(an undeclared var fails the compile). rustc's dep-info is then checked
after every compile: any file read outside the package / OUT_DIR / sysroot
is a hard "hermeticity violation" error naming the file, because it would
not be part of the action key. Build-script *file* reads have no dep-info
equivalent — they are bounded by the sandbox and the package hash instead.

## Cross-compilation (--target)

`corgi build --target wasm32-unknown-unknown` builds cdylib deployables:
rust-std for the target is fetched/verified into the pinned toolchain,
units split into host (proc-macros, build scripts + their deps) and target
platforms, and the target triple joins the action keys. Resolution now
comes from cargo's own `--unit-graph` (via RUSTC_BOOTSTRAP=1, planning
only): exact per-platform, per-subtree features and dep edges — replacing
the hand-rolled graph walk and cfg evaluator. The Apple tool group
(ar/ranlib/xcrun/xcodebuild/sh + the Metal cryptex) is permanently allowed
and keyed collectively via one Xcode identity (`xcodebuild -version`) in
the toolchain hash.

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
- `[profile.*]` honored per unit straight from cargo's unit graph
  (inheritance, `build-override`, per-package overrides, and platform
  defaults are cargo-resolved — corgi never re-derives them from
  Cargo.toml). Not yet honored: `lto` (warned, built without), `rpath`,
  `incremental`; darwin linking units always use split-debuginfo=unpacked
  with `-oso_prefix` (determinism requires it); build scripts always see
  `DEBUG=false` (C compiled with -g would embed machine-local paths)
- no target-platform cfg evaluation beyond `--filter-platform`, no dev-deps /
  tests / cdylibs, single rustc invocation per crate (no pipelining), warnings
  from cached dep actions are not replayed
- `links`/DEP_* env propagation implemented but untested (symbolicate has no
  native deps); `-L` from build scripts propagates to the final link
