# R2 action cache

Pin the public read location in the project's `corgi.toml`. Readers need no
credentials. These are example values; use your own bucket and public domain:

```toml
[cache]
read-url = "https://corgi-cache.example.com"
endpoint = "https://ACCOUNT_ID.r2.cloudflarestorage.com"
bucket = "corgi-cache"
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

## Identity and storage

Remote keys conservatively hash complete package source trees (relative paths
and contents), declared inputs, action configuration, and dependency remote
keys. They are computed before compiling dependencies. Result records retain
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
