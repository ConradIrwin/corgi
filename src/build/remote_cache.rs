//! Remote keys are conservative plan-time identities, not local action keys.
//! Records carry the precise identity and read set discovered by their producer.
//! A local copy of that association makes an imported root reusable even when
//! none of its dependency artifacts or read-set manifests were downloaded.
use super::*;

#[derive(Serialize, Deserialize)]
pub(super) struct Record {
    remote_key: String,
    action_key: String,
    spec: ActionSpec,
    result: ActionResult,
    pub(super) blobs: Vec<String>,
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

fn make_record(ctx: &Ctx, index: usize, action: &ResolvedAction, result: ActionResult) -> Record {
    Record {
        remote_key: ctx.remote_keys[index].clone(),
        action_key: action.key.clone(),
        spec: action.spec.clone(),
        blobs: blobs(&result),
        result,
    }
}

pub(super) fn compute_keys(ctx: &Ctx) -> Result<Vec<String>> {
    // Local compiles can read the enumerated Rust sources, not arbitrary files
    // in the package directory. Hash that entire set, not a learned read set.
    // Manifests and declared inputs are already hashed in the action template.
    let mut sources = HashMap::new();
    for unit in &ctx.units {
        if let std::collections::hash_map::Entry::Vacant(entry) = sources.entry(unit.pkg) {
            let package = &ctx.meta.packages[unit.pkg];
            let hash = if package.source.is_some() {
                ctx.immutable_source_hash(unit.pkg)?
            } else {
                sha256_hex(&serde_json::to_vec(&ctx.store.hash_file_set_cached(
                    &package.root(),
                    &ctx.source_files_for(unit.pkg)?,
                )?)?)
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

pub(super) fn alias_path(ctx: &Ctx, index: usize) -> PathBuf {
    ctx.store
        .root
        .join("remote-actions")
        .join(crate::remote::NAMESPACE)
        .join(format!("{}.json", ctx.remote_keys[index]))
}

fn validate(ctx: &Ctx, index: usize, record: &Record) -> Result<()> {
    anyhow::ensure!(
        record.remote_key == ctx.remote_keys[index],
        "remote key mismatch"
    );
    anyhow::ensure!(is_hash(&record.action_key), "invalid action key");
    anyhow::ensure!(
        sha256_hex(&serde_json::to_vec(&record.spec)?) == record.action_key,
        "precise action key mismatch"
    );
    // The remote key already covers configuration and dependency identities.
    // Validate read sets before making them available to precise local lookups.
    if let Some(read_set) = record.spec.local_read_set() {
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
    }
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
    anyhow::ensure!(
        ctx.action_plans[index].accepts_result(&record.result),
        "invalid action result"
    );
    Ok(())
}

fn remember(ctx: &Ctx, index: usize, record: &Record) -> Result<()> {
    ctx.save_action_result(index, &record.action_key, &record.result)?;
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
    if ctx.remote_keys.is_empty() || !ctx.action_plans[index].cache_policy.reads() {
        return Ok(None);
    }
    let Some(record) = fs::read(alias_path(ctx, index))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
    else {
        return Ok(None);
    };
    if validate(ctx, index, &record).is_err() {
        return Ok(None);
    }
    let action = resolve_action(record.spec, KeyResolution::VerifiedManifest)?;
    lookup_cached_unit(ctx, index, action, "imported")
}

pub(super) fn decode_record(
    ctx: &Ctx,
    index: usize,
    bytes: &[u8],
    download: &mut crate::report::RemoteDownload,
) -> Result<Record> {
    download.record_ns = Some(ctx.report.elapsed_ns());
    let record: Record = serde_json::from_slice(bytes).context("decoding action record")?;
    validate(ctx, index, &record).context("validating action record")?;
    download.blob_count = record.blobs.len();
    download.blobs_start_ns = Some(ctx.report.elapsed_ns());
    Ok(record)
}

pub(super) fn verify_downloaded(ctx: &Ctx, record: &Record) -> Result<()> {
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
        checked.context("validating remote OUT_DIR archive")?;
    }
    Ok(())
}

pub(super) fn install(ctx: &Ctx, index: usize, record: Record) -> Result<UnitResult> {
    remember(ctx, index, &record)?;
    let action = resolve_action(record.spec, KeyResolution::VerifiedManifest)?;
    lookup_cached_unit(ctx, index, action, "remote")?
        .context("complete remote result could not be installed")
}

/// Publish the relevant graph, not just actions executed in this invocation.
/// Requested local hits prune execution, but must not hide reusable dependency
/// results from an explicit cache-population command.
pub(super) fn publish(ctx: &Ctx, results: &[OnceLock<UnitResult>]) -> Result<()> {
    let cache = ctx.remote.as_ref().context("remote cache disabled")?;
    let mut count = 0;
    for index in dependency_closure(ctx, requested_units(ctx)) {
        let plan = &ctx.action_plans[index];
        if !plan.cache_policy.writes()
            || (!plan.cache_policy.reads() && results[index].get().is_none())
        {
            continue;
        }
        let publication = (|| -> Result<()> {
            let mut record = if let Some(result) = results[index].get() {
                Some(make_record(ctx, index, &result.action, result.res.clone()))
            } else if let Some(action) = &ctx.action_plans[index].candidate {
                ctx.lookup_action(index, &action.key)?
                    .ok()
                    .map(|result| make_record(ctx, index, action, result))
            } else {
                fs::read(alias_path(ctx, index))
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Record>(&bytes).ok())
            };
            if let Some(candidate) = &record {
                let validation = validate(ctx, index, candidate).and_then(|()| {
                    anyhow::ensure!(
                        candidate
                            .blobs
                            .iter()
                            .all(|hash| ctx.store.cache_path(hash).is_file()),
                        "local artifact blobs missing"
                    );
                    Ok(())
                });
                if let Err(error) = validation {
                    if results[index].get().is_some() {
                        return Err(error);
                    }
                    // Stale aliases for skipped work are not publication inputs.
                    record = None;
                }
            }
            if let Some(record) = &record {
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
                cache.publish(&record.remote_key, &serde_json::to_vec(record)?, &refs)?;
                count += 1;
            }
            Ok(())
        })();
        publication
            .with_context(|| format!("cache publication failed for {}", describe(ctx, index)))?;
    }
    status!("Published", "{count} cache results");
    Ok(())
}
