//! One priority queue shares four GET workers between records and their blobs.
//!
//! Only the coordinator owns queue and completion state. Workers perform one
//! transfer at a time; matching records insert their blobs at the same priority.

use super::{remote_cache, test_pass_key, Ctx};
use crate::report::{RemoteDownload, RemoteDownloadKind, RemoteDownloadOutcome};
use anyhow::Result;
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        mpsc,
    },
};

pub(super) enum Downloaded {
    Artifact(remote_cache::Record),
    Pass(remote_cache::PassRecord),
}

struct Job {
    priority: usize,
    index: usize,
    kind: JobKind,
}

enum JobKind {
    Record { key: String, pass: bool },
    Blob(String),
}

struct Pending {
    record: remote_cache::Record,
    download: RemoteDownload,
    remaining: usize,
}

pub(super) fn run(
    ctx: &Ctx,
    requests: VecDeque<(usize, bool)>,
    cancel: &AtomicBool,
    cancel_units: &[AtomicBool],
    wanted: impl Fn(usize, bool) -> bool,
    completed: impl Fn(usize, Result<Option<Downloaded>>, RemoteDownload),
) {
    if requests.is_empty() {
        return;
    }
    let cache = ctx
        .remote
        .as_ref()
        .expect("queued remote reads require a cache");
    let mut queue: VecDeque<_> = requests
        .into_iter()
        .enumerate()
        .map(|(priority, (index, pass))| {
            let key = if pass {
                test_pass_key(&ctx.remote_keys[index]).expect("test pass key serialization")
            } else {
                ctx.remote_keys[index].clone()
            };
            Job {
                priority,
                index,
                kind: JobKind::Record { key, pass },
            }
        })
        .collect();
    let mut pending: Vec<Option<Pending>> = (0..ctx.units.len()).map(|_| None).collect();
    std::thread::scope(|scope| {
        let (finished, completions) = mpsc::channel();
        let mut workers = Vec::new();
        for worker in 0..crate::remote::GET_CONCURRENCY {
            let (sender, jobs) = mpsc::channel::<(Job, u64)>();
            workers.push(sender);
            let finished = finished.clone();
            scope.spawn(move || {
                while let Ok((job, started)) = jobs.recv() {
                    let result = match &job.kind {
                        JobKind::Record { key, pass } => cache.fetch_record(
                            key,
                            if *pass {
                                cancel
                            } else {
                                &cancel_units[job.index]
                            },
                        ),
                        JobKind::Blob(hash) => cache
                            .fetch_blob(&ctx.store, hash, &cancel_units[job.index])
                            .map(|()| None),
                    };
                    if finished.send((worker, job, started, result)).is_err() {
                        break;
                    }
                }
            });
        }
        let mut idle: Vec<_> = (0..workers.len()).collect();
        loop {
            while !idle.is_empty() && !queue.is_empty() && !cancel.load(Relaxed) {
                let job = queue.pop_front().unwrap();
                let pass = matches!(&job.kind, JobKind::Record { pass: true, .. });
                if matches!(job.kind, JobKind::Blob(_)) && pending[job.index].is_none() {
                    continue;
                }
                if !wanted(job.index, pass) {
                    finish_artifact(
                        ctx,
                        job.index,
                        &mut pending,
                        cancel_units,
                        Err(crate::remote::ReadCancelled.into()),
                        &completed,
                    );
                    continue;
                }
                let worker = idle.pop().unwrap();
                workers[worker]
                    .send((job, ctx.report.elapsed_ns()))
                    .expect("GET worker stopped");
            }
            if idle.len() == workers.len() {
                break;
            }
            let (worker, job, started, result) = completions.recv().expect("GET workers stopped");
            idle.push(worker);
            let index = job.index;
            match job.kind {
                JobKind::Record { key, pass } => {
                    let mut download = RemoteDownload {
                        unit: ctx.report_unit_keys[index].clone(),
                        kind: if pass {
                            RemoteDownloadKind::TestPass
                        } else {
                            RemoteDownloadKind::Artifact
                        },
                        remote_key: key,
                        start_ns: started,
                        record_ns: None,
                        blobs_start_ns: None,
                        downloaded_ns: None,
                        end_ns: 0,
                        blob_count: 0,
                        superseded_local_ns: None,
                        outcome: RemoteDownloadOutcome::Miss,
                        detail: None,
                    };
                    let bytes = match result {
                        Ok(Some(bytes)) if !cancel.load(Relaxed) && wanted(index, pass) => bytes,
                        Ok(Some(_)) => {
                            completed(index, Err(crate::remote::ReadCancelled.into()), download);
                            continue;
                        }
                        other => {
                            completed(index, other.map(|_| None), download);
                            continue;
                        }
                    };
                    if pass {
                        let result = remote_cache::remember_downloaded_pass(
                            ctx,
                            index,
                            &bytes,
                            &mut download,
                        );
                        completed(
                            index,
                            result.map(|pass| Some(Downloaded::Pass(pass))),
                            download,
                        );
                        continue;
                    }
                    let record =
                        match remote_cache::decode_record(ctx, index, &bytes, &mut download) {
                            Ok(record) => record,
                            Err(error) => {
                                completed(index, Err(error), download);
                                continue;
                            }
                        };
                    let position = queue
                        .iter()
                        .position(|queued| queued.priority > job.priority)
                        .unwrap_or(queue.len());
                    for hash in record.blobs.iter().rev() {
                        queue.insert(
                            position,
                            Job {
                                priority: job.priority,
                                index,
                                kind: JobKind::Blob(hash.clone()),
                            },
                        );
                    }
                    let remaining = record.blobs.len();
                    pending[index] = Some(Pending {
                        record,
                        download,
                        remaining,
                    });
                    if remaining == 0 {
                        finish_artifact(ctx, index, &mut pending, cancel_units, Ok(()), &completed);
                    }
                }
                JobKind::Blob(_) => {
                    if let Some(action) = &mut pending[index] {
                        action.remaining -= 1;
                        if result.is_err() || action.remaining == 0 {
                            finish_artifact(
                                ctx,
                                index,
                                &mut pending,
                                cancel_units,
                                result.map(|_| ()),
                                &completed,
                            );
                        }
                    }
                }
            }
        }
        // The build may finish with blob jobs still queued. Every attempted
        // record gets one terminal report even when none of its blobs started.
        for index in 0..pending.len() {
            finish_artifact(
                ctx,
                index,
                &mut pending,
                cancel_units,
                Err(crate::remote::ReadCancelled.into()),
                &completed,
            );
        }
        drop(workers);
    });
}

fn finish_artifact(
    ctx: &Ctx,
    index: usize,
    pending: &mut [Option<Pending>],
    cancel_units: &[AtomicBool],
    result: Result<()>,
    completed: &impl Fn(usize, Result<Option<Downloaded>>, RemoteDownload),
) {
    let Some(mut action) = pending[index].take() else {
        return;
    };
    let result = result.and_then(|()| {
        if cancel_units[index].load(Relaxed) {
            return Err(crate::remote::ReadCancelled.into());
        }
        remote_cache::verify_downloaded(ctx, &action.record)
    });
    if result.is_ok() {
        action.download.downloaded_ns = Some(ctx.report.elapsed_ns());
    } else {
        cancel_units[index].store(true, Relaxed);
    }
    completed(
        index,
        result.map(|()| Some(Downloaded::Artifact(action.record))),
        action.download,
    );
}
