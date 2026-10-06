//! Remote keys are conservative plan-time identities, not local action keys.
//! Records carry the precise identity and read set discovered by their producer.
//! A local copy of that association makes an imported root reusable even when
//! none of its dependency artifacts or read-set manifests were downloaded.
use super::*;
use std::sync::atomic::AtomicBool;

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    remote_key: String,
    action_key: String,
    spec: ActionSpec,
    /// Remote identities in spec dependency order; graph indices are not portable.
    producers: Vec<String>,
    result: ActionResult,
    blobs: Vec<String>,
}

#[derive(Serialize, Deserialize)]
struct PassRecord {
    remote_key: String,
    harness_action: String,
    result: TestPass,
    blobs: Vec<String>,
}

fn blobs(result: &ActionResult) -> Vec<String> {
    let mut hashes = result
        .outputs
        .iter()
        .map(|o| o.hash.clone())
        .collect::<Vec<_>>();
    hashes.extend(result.out_dir.iter().map(|archive| archive.hash.clone()));
    hashes.sort();
    hashes.dedup();
    hashes
}

fn is_hash(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn dependencies(spec: &ActionSpec) -> &[PlannedActionDependency] {
    match spec {
        ActionSpec::Compile(spec) => &spec.dependencies,
        ActionSpec::BuildScriptRun(spec) => &spec.dependencies,
    }
}

fn make_record(ctx: &Ctx, index: usize, action: &ResolvedAction, result: ActionResult) -> Record {
    Record {
        remote_key: ctx.remote_keys[index].clone(),
        action_key: action.key.clone(),
        producers: dependencies(&action.spec)
            .iter()
            .map(|dep| ctx.remote_keys[dep.producer_unit].clone())
            .collect(),
        spec: action.spec.clone(),
        blobs: blobs(&result),
        result,
    }
}

pub(super) fn compute_keys(ctx: &Ctx) -> Result<Vec<String>> {
    // Hash the full source tree, not a previously discovered read set.
    // External declared inputs are already hashed in the action template.
    let mut sources = HashMap::new();
    for unit in &ctx.units {
        if let std::collections::hash_map::Entry::Vacant(entry) = sources.entry(unit.pkg) {
            let package = &ctx.meta.packages[unit.pkg];
            let hash = if package.source.is_some() {
                ctx.immutable_source_hash(unit.pkg)?
            } else {
                // Tree hashing records symlink targets. Also hash the bytes of
                // permitted Rust inputs, including files reached through links.
                sha256_hex(&serde_json::to_vec(&(
                    ctx.store.hash_dir_cached(&package.root(), None)?,
                    ctx.store
                        .hash_file_set_cached(&package.root(), &ctx.source_files_for(unit.pkg)?)?,
                ))?)
            };
            entry.insert(hash);
        }
    }
    fn visit(
        ctx: &Ctx,
        index: usize,
        sources: &HashMap<usize, String>,
        keys: &mut [String],
    ) -> Result<()> {
        if !keys[index].is_empty() {
            return Ok(());
        }
        for dependency in &ctx.units[index].deps {
            visit(ctx, dependency.unit, sources, keys)?;
        }
        let spec = action_spec_with_producer_keys(
            ctx,
            index,
            &ctx.action_plans[index].template,
            |producer| {
                Some((
                    keys[producer].clone(),
                    ctx.action_plans[producer].main_output.clone(),
                ))
            },
        )?
        .context("remote dependency key missing")?;
        let mut producer_keys = ctx.units[index]
            .deps
            .iter()
            .map(|dep| {
                (
                    &keys[dep.unit],
                    dep.role.report_name(),
                    dep.role.extern_name(),
                )
            })
            .collect::<Vec<_>>();
        producer_keys.sort();
        // Store paths can be embedded in OUT_DIR and CARGO_BIN_EXE values.
        // Do not pretend artifacts can be relocated to a different store.
        keys[index] = sha256_hex(&serde_json::to_vec(&(
            crate::remote::NAMESPACE,
            &ctx.store.root,
            &sources[&ctx.units[index].pkg],
            spec,
            producer_keys,
        ))?);
        Ok(())
    }
    let mut keys = vec![String::new(); ctx.units.len()];
    for index in 0..keys.len() {
        visit(ctx, index, &sources, &mut keys)?;
    }
    Ok(keys)
}

fn alias_path(ctx: &Ctx, index: usize) -> PathBuf {
    ctx.store
        .root
        .join("remote-actions")
        .join(crate::remote::NAMESPACE)
        .join(format!("{}.json", ctx.remote_keys[index]))
}

fn validate(ctx: &Ctx, index: usize, record: &mut Record) -> Result<()> {
    anyhow::ensure!(
        record.remote_key == ctx.remote_keys[index],
        "remote key mismatch"
    );
    anyhow::ensure!(is_hash(&record.action_key), "invalid action key");
    anyhow::ensure!(
        sha256_hex(&serde_json::to_vec(&record.spec)?) == record.action_key,
        "precise action key mismatch"
    );
    let actual = match &mut record.spec {
        ActionSpec::Compile(spec) => &mut spec.dependencies,
        ActionSpec::BuildScriptRun(spec) => &mut spec.dependencies,
    };
    anyhow::ensure!(
        actual.len() == record.producers.len(),
        "invalid producer list"
    );
    for (dependency, remote_key) in actual.iter_mut().zip(&record.producers) {
        anyhow::ensure!(is_hash(&dependency.producer), "invalid producer key");
        let planned = dependencies(&ctx.action_plans[index].template)
            .iter()
            .find(|planned| {
                ctx.remote_keys[planned.producer_unit] == *remote_key
                    && planned.filename == dependency.filename
                    && planned.extern_name == dependency.extern_name
            })
            .context("remote producer mismatch")?;
        dependency.producer_unit = planned.producer_unit;
    }
    // Reconstruct from our own configuration plus the recorded producer
    // identities. This also verifies embedded CARGO_BIN_EXE paths, rather than
    // accepting arbitrary paths from an otherwise matching record.
    let mut expected = action_spec_with_producer_keys(
        ctx,
        index,
        &ctx.action_plans[index].template,
        |producer| {
            let dependency = dependencies(&record.spec)
                .iter()
                .find(|dep| dep.producer_unit == producer)?;
            Some((
                dependency.producer.clone(),
                ctx.action_plans[producer].main_output.clone(),
            ))
        },
    )?
    .context("remote producer identity missing")?;
    if let Some(read_set) = record.spec.local_read_set_mut() {
        let root = ctx.meta.packages[ctx.units[index].pkg].root();
        let allowed = ctx
            .package_read_inputs(ctx.units[index].pkg, PackageReadPhase::Compile)?
            .paths
            .into_iter()
            .filter_map(|path| path.strip_prefix(&root).ok().map(Path::to_path_buf))
            .collect::<Vec<_>>();
        let paths = read_set
            .iter()
            .map(|(path, _)| PathBuf::from(path))
            .collect::<Vec<_>>();
        anyhow::ensure!(
            paths.iter().all(|path| allowed.contains(path)),
            "invalid read-set path"
        );
        let current = ctx.store.hash_file_set_cached(&root, &paths)?;
        anyhow::ensure!(&current == read_set, "read-set content mismatch");
        *expected
            .local_read_set_mut()
            .context("unexpected remote read set")? = read_set.clone();
    }
    anyhow::ensure!(
        serde_json::to_value(expected)? == serde_json::to_value(&record.spec)?,
        "remote action configuration mismatch"
    );
    anyhow::ensure!(
        record.blobs == blobs(&record.result),
        "blob manifest mismatch"
    );
    anyhow::ensure!(
        record.blobs.iter().all(|hash| is_hash(hash)),
        "invalid blob hash"
    );
    let mut names = HashSet::new();
    for output in &record.result.outputs {
        let mut components = Path::new(&output.name).components();
        anyhow::ensure!(
            matches!(components.next(), Some(std::path::Component::Normal(_)))
                && components.next().is_none()
                && !output.name.contains('\0')
                && names.insert(&output.name),
            "invalid output name"
        );
    }
    if matches!(ctx.units[index].kind, Kind::Bsr) {
        anyhow::ensure!(
            record.result.bs.is_some() && record.result.out_dir.is_some(),
            "incomplete build script"
        );
    } else {
        anyhow::ensure!(
            ctx.action_plans[index]
                .outputs
                .iter()
                .all(|name| names.contains(name)),
            "incomplete outputs"
        );
    }
    Ok(())
}

fn remember(ctx: &Ctx, index: usize, record: &Record) -> Result<()> {
    ctx.store
        .save_action(&record.action_key, &serde_json::to_vec(&record.result)?)?;
    let mut spec = record.spec.clone();
    if let Some(read_set) = spec.local_read_set_mut() {
        let files = std::mem::take(read_set);
        ctx.store.save_manifest_entry(
            &sha256_hex(&serde_json::to_vec(&spec)?),
            &sha256_hex(&serde_json::to_vec(&files)?),
            &crate::store::ReadSetManifestEntry { files },
        )?;
    }
    ctx.store
        .write_atomic(&alias_path(ctx, index), &serde_json::to_vec(record)?)
}

pub(super) fn lookup_local(ctx: &Ctx, index: usize) -> Result<Option<UnitResult>> {
    if ctx.remote_keys.is_empty() {
        return Ok(None);
    }
    let Some(mut record) = fs::read(alias_path(ctx, index))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
    else {
        return Ok(None);
    };
    if validate(ctx, index, &mut record).is_err() {
        return Ok(None);
    }
    let action = resolve_action(record.spec, KeyResolution::VerifiedManifest)?;
    lookup_cached_unit(ctx, index, action, "imported")
}

pub(super) fn fetch(ctx: &Ctx, index: usize, cancel: &Arc<AtomicBool>) -> Result<Option<Record>> {
    let cache = ctx.remote.as_ref().context("remote cache disabled")?;
    let Some(bytes) = cache.fetch_record(&ctx.remote_keys[index], cancel)? else {
        return Ok(None);
    };
    let mut record: Record = serde_json::from_slice(&bytes)?;
    validate(ctx, index, &mut record)?;
    for hash in &record.blobs {
        if cache.fetch_blob(&ctx.store, hash, cancel)?.is_none() {
            return Ok(None);
        }
    }
    if let Some(archive) = &record.result.out_dir {
        // Verify archive structure before accepting the action, too: a valid
        // digest alone doesn't make an invalid or unsafe tar installable.
        let stage = ctx.store.tmp_path("remote-out-dir-check");
        fs::create_dir_all(&stage)?;
        let checked = crate::out_dir_archive::extract_out_dir(
            fs::File::open(ctx.store.cache_path(&archive.hash))?,
            &stage,
        );
        fs::remove_dir_all(stage).ok();
        checked?;
    }
    Ok(Some(record))
}

pub(super) fn install(ctx: &Ctx, index: usize, record: Record) -> Result<UnitResult> {
    remember(ctx, index, &record)?;
    let action = resolve_action(record.spec, KeyResolution::VerifiedManifest)?;
    lookup_cached_unit(ctx, index, action, "remote")?
        .context("complete remote result could not be installed")
}

pub(super) fn fetch_pass(
    ctx: &Ctx,
    index: usize,
    cancel: &Arc<AtomicBool>,
) -> Result<Option<(String, u64)>> {
    let remote_key = test_pass_key(&ctx.remote_keys[index])?;
    let cache = ctx.remote.as_ref().context("remote cache disabled")?;
    let Some(bytes) = cache.fetch_record(&remote_key, cancel)? else {
        return Ok(None);
    };
    let record: PassRecord = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        record.remote_key == remote_key
            && is_hash(&record.harness_action)
            && record.blobs.is_empty()
            && record.result.passed,
        "invalid test pass"
    );
    // The scheduler checks this identity against the actual built/imported
    // harness before publishing it into the local successful-test cache.
    Ok(Some((record.harness_action, record.result.test_count)))
}

/// Publish the relevant graph, not just actions executed in this invocation.
/// Requested local hits prune execution, but must not hide reusable dependency
/// results from an explicit cache-population command.
pub(super) fn publish(
    ctx: &Ctx,
    results: &[OnceLock<UnitResult>],
    test_passes: bool,
) -> Result<()> {
    let cache = ctx.remote.as_ref().context("remote cache disabled")?;
    let mut count = 0;
    for index in dependency_closure(ctx, requested_units(ctx)) {
        let publication = (|| -> Result<()> {
            let record = if let Some(result) = results[index].get() {
                Some(make_record(ctx, index, &result.action, result.res.clone()))
            } else if let Some(action) = &ctx.action_plans[index].candidate {
                ctx.lookup_action(&action.key)?
                    .ok()
                    .map(|result| make_record(ctx, index, action, result))
            } else {
                fs::read(alias_path(ctx, index))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            };
            let Some(mut record) = record else {
                return Ok(());
            };
            validate(ctx, index, &mut record)?;
            let paths = record
                .blobs
                .iter()
                .map(|hash| ctx.store.cache_path(hash))
                .collect::<Vec<_>>();
            let refs = record
                .blobs
                .iter()
                .zip(&paths)
                .map(|(hash, path)| crate::remote::BlobRef { hash, path })
                .collect::<Vec<_>>();
            cache.publish(&record.remote_key, &serde_json::to_vec(&record)?, &refs)?;
            count += 1;
            if test_passes && ctx.units[index].is_root && ctx.units[index].test_harness {
                if let Some(test_count) =
                    load_test_pass(&ctx.store, &test_pass_key(&record.action_key)?)
                {
                    let remote_key = test_pass_key(&record.remote_key)?;
                    let pass = PassRecord {
                        remote_key: remote_key.clone(),
                        harness_action: record.action_key,
                        result: TestPass {
                            passed: true,
                            test_count,
                        },
                        blobs: Vec::new(),
                    };
                    cache.publish(&remote_key, &serde_json::to_vec(&pass)?, &[])?;
                    count += 1;
                }
            }
            Ok(())
        })();
        publication
            .with_context(|| format!("cache publication failed for {}", describe(ctx, index)))?;
    }
    status!("Published", "{count} cache results");
    Ok(())
}
