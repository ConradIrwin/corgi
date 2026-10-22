Your high-energy–lap-dog inspired rust toolchain.

Corgi exists to explore a different trade-off space to cargo, in particular we aggressively try and decrease the latency and resource usage of the rust toolchain by:
* Using one unified cache per machine, instead of per worktree.
* Doing workspace-hack inspired feature unification to reduce incidental rebuilds with `-p`.
* Running build.rs and proc-macros in a sandbox with no env, or network; and only a whitelist of files to make it much harder to accidentally bust their caches.
* Caching successful test runs to avoid re-runs (agent harness seem to love to run one package’s tests, and then the whole workspace, even if most of it hasn’t changed at all).

Unlike bazel or buck2, which go much further along this path, a key tenet of corgi is that it is (mostly) compatible with cargo - we still use cargo for dependency resolution, and you can use corgi locally while relying on cargo in CI (or on your risk-averse collaborator’s machines).

Currently corgi is macOS only, but PRs are welcome to make this cross-platform.

## Usage

Use as you would cargo, except corgi…

```
cargo install corgi-build
corgi [ run | build | test | bench | check | clippy | fmt ] 
```

Corgi does not yet support `install` (just use cargo) or path-inherited subcommands like `cargo make`.

## Configuration

Corgi is primarily configured by your existing rust files:
* Cargo.toml
* Cargo.lock
* rust-toolchain.toml
* .cargo/config.toml

For most crates, you will not need to configure corgi at all. But, there are *some* things you do need to do.

### tools
Because corgi sandboxes build.rs, any dependencies you need from the network must be pre-downloaded. 

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
# (and if set, which releases).
# This lets zed's test builds not re-build on every commit
profiles = ["release"]
```

### Extra inputs
Rust build.rs can read whatever files it likes from where-ever, this makes it hard to share builds between machines - and hard to cache builds well.

If your build needs to read a file (beyond `.rs` files in the crate’s directory), you must declare them:

```
[extra-inputs]
extension_host = ["../extension_api/wit/**"]
agent_prompt = ["src/prompt.md"]
```

### Feature unification roots

Cargo by default picks default features based on the packaged passed to `-p` on the command line. This makes it very easy to have feature drift (depending on which package is compiled, what flags the dependencies are provided with).

Corgi copies workspace-hack, and asserts that this is almost certainly not what you actually want, and by default all features are unified across all crates.

To avoid this you can add crates that you want to exclude from feature unification to your `corgi.toml`

```
[roots.wasm]
packages = ["delta_runner"]
```

When running a corgi command, you can specify which feature universe to exist in with `--root`.

## Distributed caching

You can accelerate builds using corgi's support for remote caching. Currently this works with Cloudflare R2, but other backends are welcome.

```
[cache]
read-url = "https://9a5426ad4c05881db7cff829de5f13e4.r2.cloudflarestorage.com/corgi-build-cache"
```

To authenticate local clients with the cache you can store authentication in `~/.config/corgi/credentials`

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

In most cases it's useful to cache both a build *and* a test run, because any differences in features or dependencies in your `Cargo.toml` dev-dependencies will mean that you will very likely build crates with a different set of features.

Uploading test results allows for future runs of `corgi test` to avoid running tests for unchanged crates.

## And more..?

Corgi is still in early days, but I’m actively using it to improve build times and reduce cache size.

If you’d like to use it and need help, please file an issue - I’d love to work with you to make it work.

If you are using it, and it’s working, I’d love to know - file an issue or send me an [email](mailto:me@cirw.in).

If you have amazing ideas for making reproducibility better or performance faster I’d love to hear from you. (Maybe even we could make some rustc changes so we don’t need quite such an intense disk layout…)

### AI disclosure

The code is almost all AI written (but I wrote the README by hand!)
