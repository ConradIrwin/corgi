Your high-energy–lap-dog inspired rust toolchain.

Corgi ([github](https://github.com/ConradIrwin/corgi)) aims to make compiling medium-to-large rust projects faster by:
* Using one build cache per machine, instead of per worktree.
* Doing [workspace-hack](https://docs.rs/cargo-hakari/latest/cargo_hakari/about/index.html) inspired feature unification to reduce incidental rebuilds with `-p`.
* Running build.rs and proc-macros in a sandbox with no env, or network; and only a whitelist of files to make it much harder to accidentally bust their caches.
* Running tests per process and in parallel (like [nextest](https://nexte.st)).
* Caching successful test runs.
* (Optionally) Downloading known good build artifacts from your own R2.

Unlike bazel or buck2, which go much further along this path, a key tenet of corgi is that it is (mostly) compatible with cargo - we still use cargo for dependency resolution, and you can use corgi locally while relying on cargo in CI (or on your risk-averse collaborators’ machines).

Currently corgi works on macOS and NixOS. Support for normal Linux distributions and Windows are in progress, PRs are welcome!

## Usage

Use as you would cargo, except corgi…

```
cargo install corgi-build
corgi [ run | build | test | bench | check | clippy | fmt ]
```

Corgi does not yet support `install` (just use cargo) or third-party subcommands like `cargo make`.

## Configuration

Corgi is primarily configured by your existing rust files:
* Cargo.toml
* Cargo.lock
* rust-toolchain.toml
* .cargo/config.toml

For most crates, you will not need to configure corgi at all. But there are *some* things you do need to do.

### tools
Because corgi runs build.rs in a sandbox, any dependencies you need from the network must be pre-downloaded.

Zed and similar codebases frequently use tools like `protoc` to compile things. In the cargo world these are ambiently used from the path. To keep builds reproducible, corgi requires that you explicitly define these.

```
# corgi.toml

[tools.protoc]
# user-visible version
version = "35.1"
# where to get an archive of it from
url = "https://github.com/protocolbuffers/protobuf/releases/download/v35.1/protoc-35.1-osx-aarch_64.zip"
# sha256 to verify it got the right version
sha256 = "193289af0470c6a1aada357d4fba0bbf8d78bfaac8b5e42ca30af2ef75583de2"
# path relative to the root of the archive
bin = "bin/protoc"
# how to provide the tool to build.rs (the PROTOC env var)
env = "PROTOC"
# which packages use this tool
packages = ["proto", "livekit_api"]

```

### env vars
A common footgun in our use of cargo was incidental environment variable changes causing rebuilds. Corgi only passes a whitelist of environment variables to rustc, build.rs and proc-macros.

```
[env.ZED_COMMIT_SHA]
# how to calculate the env var (run outside the sandbox)
command = "git rev-parse HEAD"
# which packages need this
packages = ["zed", "cli", "remote_server"]
# (and if set, which profiles).
# This lets zed's test builds not re-build on every commit
profiles = ["release"]
```

### Extra inputs
Rust build.rs can read whatever files it likes from wherever; this makes it hard to share builds between machines - and hard to cache builds well.

If your build needs to read files (beyond `.rs` files in the crate’s directory), you must declare them:

```
[extra-inputs]
extension_host = ["../extension_api/wit/**"]
agent_prompt = ["src/prompt.md"]
```

Note particularly that if you use [rust-embed](https://crates.io/crates/rust-embed) you must enable its `deterministic-timestamps` feature. Otherwise cross-machine caching will not work.

### Feature unification roots

Cargo by default picks default features based on the packages passed to `-p` on the command line. This makes it very easy to have feature drift (depending on which package is compiled, what flags the dependencies are provided with).

Corgi copies workspace-hack, and assumes that this is almost certainly not what you actually want. By default all features are unified across all crates.

To avoid this you can add crates that you want to exclude from feature unification to your `corgi.toml`:

```
[roots.wasm]
packages = ["delta_runner"]
```

When you build these crates they are unified with themselves and their dependencies, but not with the rest of the workspace. If you want to build a package with the flags that exist in a specific universe, pass `--root wasm` to corgi.

## Distributed caching

You can accelerate builds using corgi's support for remote caching. Currently this works with Cloudflare R2, but other backends are welcome.

```
[cache]
read-url = "https://9a5426ad4c05881db7cff829de5f13e4.r2.cloudflarestorage.com/corgi-build-cache"
```

To authenticate local clients with the cache you can store authentication in `~/.config/corgi/credentials`:

```
["https://9a5426ad4c05881db7cff829de5f13e4.r2.cloudflarestorage.com/corgi-build-cache"]
access-key-id = "..."
secret-access-key = "..."
```

To populate the cache, run the following commands in your CI provider:

```
export CORGI_R2_WRITE_URL=...
export CORGI_R2_ACCESS_KEY_ID=...
export CORGI_R2_SECRET_ACCESS_KEY=...

corgi cache build
corgi cache test
```

This will run a build, and run all the tests, and upload all the intermediate artifacts and test results to R2.

In most cases it's useful to cache both a build *and* a test run. Just tests are insufficient because cargo builds
test targets with dev-dependencies enabled so they aren't usable for development (:eyeroll:). 

Uploading test results allows for future runs of `corgi test` to avoid running tests for unchanged crates.

## And more..?

Corgi is still in early days, but we're actively using it to improve build times and reduce cache size while working on [Delta](https://delta.dev).

If you’d like to use it and need help, please [file an issue](https://github.com/ConradIrwin/corgi/issues) - I’d love to work with you to make it work.

If you are using it, and it’s working, I’d love to know - [file an issue](https://github.com/ConradIrwin/corgi/issues) or send me an [email](mailto:me@cirw.in).

If you have amazing ideas for making reproducibility better or performance faster I’d love to hear from you. (Maybe we could even make some rustc and cargo changes so this tool becomes obsolete…)

### AI disclosure

The code is almost all AI-written (but I wrote the README by hand!)
