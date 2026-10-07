# R2 action cache

The project's `corgi.toml` pins the public read location in Zed Industries.
Readers need no credentials:

```toml
[cache]
read-url = "https://pub-cf9929c86d0144829e53d5e9f66d3060.r2.dev"
endpoint = "https://9a5426ad4c05881db7cff829de5f13e4.r2.cloudflarestorage.com"
bucket = "corgi-build-cache"
```

`read-url` is the public bucket root, without the versioned prefix.
`endpoint` and `bucket` are needed only for publication. The public URL must
serve the same bucket directly, without redirects.

Use a trusted bucket: cached artifacts are executable code, and successful
test records can suppress test execution. Blob hashes detect corruption, not
an unauthorized publisher. Limit write credentials to trusted CI/jobs. Public
read access exposes build outputs, source read-set paths/hashes, action
configuration, and captured compiler/build-script diagnostics; do not use a
public cache for projects whose artifacts or declared environment contain
secrets.

## Populate and consume

Ordinary `corgi build` and `corgi test` only read remotely. They remain usable
offline: missing, invalid, or unavailable remote data is a cache miss. Up to
four downloads run alongside compilation, never ahead of ready local work.
A completed download can replace a queued action, but never a running action.
Outstanding downloads are cancelled when compilation finishes.

```sh
corgi build
corgi cache build
corgi cache test
```

The explicit `cache` commands execute normally and then publish relevant
successful results, including local cache hits. The first two commands above
therefore upload without recompiling. Publication failures return a nonzero
exit status. Successful actions from an otherwise failed build/test invocation
can still be published. Only canonical, unfiltered successful test runs are
cached, matching the local test cache; `test --force` bypasses cached passes.

Supply write credentials through the publisher's secret environment:

- `CORGI_R2_ACCESS_KEY_ID`
- `CORGI_R2_SECRET_ACCESS_KEY`

The transport uses curl's AWS SigV4 support (`aws:amz:auto:s3`). Credentials are
sent to curl on stdin, not command-line arguments or committed configuration.

The **Populate Corgi build cache** GitHub Actions workflow runs only on manual
dispatch. Configure the two variables above as repository Actions secrets using
an R2 **Object Read & Write** token scoped only to `corgi-build-cache`. The job
bootstraps Corgi from its checkout, then runs `cache build` and `cache test`
without changing the package version or publishing a release.

The workflow's `store-path` input defaults to `/Users/Shared/corgi`. For a
non-destructive cold-cache check, dispatch it with a fresh absolute path and use
that same canonical path as `CORGI_STORE` locally. This avoids moving or
deleting the ordinary local cache. Both checkouts must use the same revision
and equivalent Git origin URLs: origin contributes to local package identity.
Independently built Corgi executables can share results when their cache-format
and compiler-driver versions agree; executable bytes are not cache inputs.

## Identity and storage

For local packages, remote keys hash the entire Rust source set enumerated for
the compiler sandbox (relative paths and contents), rather than the whole
package directory. They also include manifests, declared inputs, action
configuration, and dependency remote keys. Unrelated non-Rust files, such as
documentation or generated fixture executables, do not affect these keys unless
declared as inputs. Registry/Git packages retain whole-package hashing because
their actions may read their entire package.

Source enumeration is unchanged: it recursively includes `.rs` files beneath
the package root, excluding `.git` and the root's `target` directory. There is no
new nested-package boundary or Git-ignore filtering. A permitted but unread
Rust file still affects the remote key; its contents need not affect the precise
local key. Local keys retain their source-layout guard for additions/removals,
verified read sets, and precise dependency identities.

Remote keys are computed before compiling dependencies. Result records retain
the producer's precise local action key, read set, and dependency identities.
Imports install local action records and manifests plus a local association
with the remote key, so subsequent builds do not need remote discovery.

Toolchains, hosts, targets, profiles, and declared environment must match.
Checkouts may live at different paths. The canonical store path must match:
existing artifacts can embed `OUT_DIR` and `CARGO_BIN_EXE_*` paths, so the
remote identity deliberately includes the store path rather than pretending
those artifacts are relocatable. The default macOS store is
`/Users/Shared/corgi`; configure compatible CI machines accordingly.

Objects live under a versioned namespace:

```text
corgi/v1/actions/<remote-key>.json
corgi/v1/blobs/<sha256>
```

Downloads are SHA-256 verified before atomic local installation. Records are
accepted only with all referenced outputs present, including build-script
archives and split-debug objects. Publication checks blob existence even when
the record already exists, repairs missing blobs, then publishes records.
Complete existing records remain unchanged; concurrent initial publishers use
conditional writes.

## Lifecycle policy

Configure **30-day expiry for every object under `corgi/v1/` externally in R2**.
Apply the same fixed TTL to action records and blobs. Corgi does not configure
this policy, touch remote objects to extend their lifetime, or run a remote
garbage-collection service. A record can outlive one of its blobs; readers
treat that as a miss and publishers repair the missing blob without rewriting
the record.

The `corgi-build-cache` bucket has the `expire-after-30-days` lifecycle rule,
enabled for every object prefix. The bucket's default incomplete multipart
upload cleanup rule is separate and does not refresh or expire completed
cache objects.
