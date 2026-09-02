//! Per-run build performance reports.
//!
//! This module owns the versioned local diagnostic JSON and long-term run
//! summary for Corgi invocations. `Recorder` permits scheduler and test worker
//! threads to add facts as they become known, while the caller retains
//! ownership of classifying the invocation's final outcome.

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

const SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub run: Run,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub stages: BTreeMap<String, StageTiming>,
    pub cache: CacheSummary,
    pub counters: Counters,
    pub units: Vec<Unit>,
    pub test_harnesses: Vec<TestHarness>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
}

impl Report {
    pub fn new(run: Run) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            run,
            stages: BTreeMap::new(),
            cache: CacheSummary::default(),
            counters: Counters::default(),
            units: Vec::new(),
            test_harnesses: Vec::new(),
            execution: None,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Run {
    pub id: String,
    pub started_at_unix_ns: u128,
    pub duration_ns: u64,
    pub workspace: Workspace,
    pub command: Command,
    pub tool: Tool,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Serialize)]
pub struct Workspace {
    pub root: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct Command {
    pub name: String,
    pub workspace: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub packages: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root_set: Option<String>,
    pub profile: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub features: Vec<String>,
    pub incremental: bool,
    pub force_tests: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_filter: Option<String>,
    pub exec_args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    pub corgi_version: String,
    pub corgi_build_id: String,
    pub rustc_version: String,
    pub host: String,
    pub logical_cpus: usize,
    pub toolchain: ToolchainInput,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub declared_environment: Vec<EnvironmentInput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub host_rustflags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub target_rustflags: Vec<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stage: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    #[default]
    Success,
    Failed,
    Interrupted,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct StageTiming {
    pub start_ns: u64,
    pub end_ns: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct CacheSummary {
    pub plan: PlanCache,
    pub artifacts: ArtifactCache,
    pub test_results: TestResultCache,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct PlanCache {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<PlanCacheResult>,
    pub lookup_ns: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanCacheResult {
    Hit,
    Miss,
    Stale,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ArtifactCache {
    pub workspace: CacheCounts,
    pub dependencies: CacheCounts,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct CacheCounts {
    pub hits: u64,
    pub misses: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct TestResultCache {
    pub hits: u64,
    pub misses: u64,
    pub bypassed: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Counters {
    pub source_hash_ns: u64,
    pub hinted_directories: u64,
    pub files_statted: u64,
    pub files_rehashed: u64,
    pub immutable_source_hash_hits: u64,
    pub export_check_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Unit {
    pub id: String,
    pub logical_id: String,
    pub package: Package,
    pub action: UnitAction,
    pub target: Target,
    pub profile: Value,
    pub features: Vec<String>,
    pub dependencies: Vec<UnitDependency>,
    pub outcome: UnitOutcome,
    pub cache: UnitCache,
    pub key: UnitKey,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timings: Option<UnitTimings>,
    pub outputs: Vec<Output>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Package {
    pub id: String,
    pub name: String,
    pub version: String,
    pub scope: String,
    pub root: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitAction {
    pub kind: String,
    pub host: bool,
    pub is_root: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Target {
    pub name: String,
    pub kinds: Vec<String>,
    pub crate_types: Vec<String>,
    pub edition: String,
    pub source: String,
    pub platform: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitDependency {
    pub unit: String,
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitOutcome {
    pub status: UnitStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitStatus {
    Success,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitCache {
    pub result: UnitCacheResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub probe: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitCacheResult {
    Hit,
    Miss,
    NotChecked,
}

#[derive(Clone, Debug, Serialize)]
pub struct UnitKey {
    pub hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inputs: Option<ActionKeyInputs>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum ActionKeyInputs {
    Compile(Box<CompileKeyInputs>),
    BuildScriptRun(Box<BuildScriptRunKeyInputs>),
}

#[derive(Clone, Debug, Serialize)]
pub struct CompileKeyInputs {
    pub source_hash: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub declared_environment: Vec<EnvironmentInput>,
    pub effective_environment_hash: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub link_dependencies: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub build_script: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lints: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clippy: Option<String>,
    pub cap_lints: bool,
    pub uses_toolchain: bool,
    pub compiler_identity: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct BuildScriptRunKeyInputs {
    pub source_hash: String,
    pub script: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub declared_environment: Vec<EnvironmentInput>,
    pub effective_environment_hash: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolInput>,
    pub uses_toolchain: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct EnvironmentInput {
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolchainInput {
    pub cc: String,
    pub ld: String,
    pub sdk: String,
    pub xcode: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolInput {
    pub name: String,
    pub version: String,
    pub identity: String,
    pub environment_name: String,
    pub environment_value: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct UnitTimings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata_ns: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_ns: Option<u64>,
    pub key_ns: u64,
    pub cache_ns: u64,
    pub compiler_ns: u64,
    pub validate_ns: u64,
    pub ingest_ns: u64,
    pub ingest_bytes: u64,
    pub finish_ns: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Output {
    pub name: String,
    pub hash: String,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct TestHarness {
    pub unit: String,
    pub name: String,
    pub cache: HarnessCache,
    pub discovery_ns: u64,
    pub duration_ns: u64,
    pub summary: TestSummary,
    pub tests: Vec<Test>,
}

#[derive(Clone, Debug, Serialize)]
pub struct HarnessCache {
    pub result: UnitCacheResult,
    pub pass_key: String,
    pub bypassed: bool,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct TestSummary {
    pub passed: u64,
    pub failed: u64,
    pub killed: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct Test {
    pub name: String,
    pub outcome: TestStatus,
    pub duration_ns: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestStatus {
    Passed,
    Failed,
    Killed,
}

#[derive(Clone, Debug, Serialize)]
pub struct Execution {
    pub unit: String,
    pub program: String,
    pub args: Vec<String>,
    pub start_ns: u64,
    pub end_ns: u64,
    pub outcome: ExecutionOutcome,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionOutcome {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
}

pub struct Recorder {
    started: Instant,
    report: Mutex<Report>,
}

impl Recorder {
    pub fn new_at(run: Run, started: Instant) -> Self {
        Self {
            started,
            report: Mutex::new(Report::new(run)),
        }
    }

    pub fn elapsed_ns(&self) -> u64 {
        self.started
            .elapsed()
            .as_nanos()
            .try_into()
            .unwrap_or(u64::MAX)
    }

    pub fn update<F>(&self, update: F)
    where
        F: FnOnce(&mut Report),
    {
        update(&mut self.report.lock().unwrap());
    }

    pub fn finish_to_path(&self, path: &Path) -> Result<Report> {
        let duration_ns = self.elapsed_ns();
        let mut report = self.report.lock().unwrap().clone();
        report.run.duration_ns = duration_ns;
        let parent = path
            .parent()
            .context("timing report path has no parent directory")?;
        fs::create_dir_all(parent)
            .with_context(|| format!("creating timing report directory {}", parent.display()))?;
        let bytes = serde_json::to_vec(&report).context("serializing timing report")?;
        let report_temporary = path.with_extension("json.partial");
        fs::write(&report_temporary, bytes)
            .with_context(|| format!("writing timing report {}", report_temporary.display()))?;
        fs::rename(&report_temporary, path)
            .with_context(|| format!("publishing timing report {}", path.display()))?;
        Ok(report)
    }
}

const RUNS_CSV_HEADER: &str = "schema_version,run_id,started_at_unix_ns,workspace_root,command,profile,target,corgi_version,rustc_version,outcome,failure_stage,total_ns,build_ns,execute_ns,test_ns,plan_cache_result,workspace_hits,workspace_misses,dependency_hits,dependency_misses,test_result_hits,test_result_misses";

pub fn append_run(store_root: &Path, report: &Report) -> Result<()> {
    let stage_duration = |name: &str| {
        report
            .stages
            .get(name)
            .map(|stage| stage.end_ns.saturating_sub(stage.start_ns))
            .unwrap_or_default()
    };
    let build_ns = ["setup", "plan", "prepare", "build", "export"]
        .into_iter()
        .map(stage_duration)
        .sum::<u64>();
    let outcome = match report.run.outcome.status {
        RunStatus::Success => "success",
        RunStatus::Failed => "failed",
        RunStatus::Interrupted => "interrupted",
    };
    let plan_cache_result = match report.cache.plan.result {
        Some(PlanCacheResult::Hit) => "hit",
        Some(PlanCacheResult::Miss) => "miss",
        Some(PlanCacheResult::Stale) => "stale",
        None => "",
    };
    let values = [
        SCHEMA_VERSION.to_string(),
        report.run.id.clone(),
        report.run.started_at_unix_ns.to_string(),
        report.run.workspace.root.clone(),
        report.run.command.name.clone(),
        report.run.command.profile.clone(),
        report.run.command.target.clone().unwrap_or_default(),
        report.run.tool.corgi_version.clone(),
        report
            .run
            .tool
            .rustc_version
            .lines()
            .next()
            .unwrap_or_default()
            .to_string(),
        outcome.to_string(),
        report.run.outcome.stage.clone().unwrap_or_default(),
        report.run.duration_ns.to_string(),
        build_ns.to_string(),
        stage_duration("execute").to_string(),
        stage_duration("test").to_string(),
        plan_cache_result.to_string(),
        report.cache.artifacts.workspace.hits.to_string(),
        report.cache.artifacts.workspace.misses.to_string(),
        report.cache.artifacts.dependencies.hits.to_string(),
        report.cache.artifacts.dependencies.misses.to_string(),
        report.cache.test_results.hits.to_string(),
        report.cache.test_results.misses.to_string(),
    ];
    let row = values
        .iter()
        .map(|value| csv_cell(value))
        .collect::<Vec<_>>()
        .join(",");
    let path = store_root.join("metrics").join("runs.csv");
    let parent = path
        .parent()
        .context("run metrics path has no parent directory")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("creating run metrics directory {}", parent.display()))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening run metrics {}", path.display()))?;
    file.lock()
        .with_context(|| format!("locking run metrics {}", path.display()))?;
    if file.metadata()?.len() == 0 {
        writeln!(file, "{RUNS_CSV_HEADER}")?;
    }
    writeln!(file, "{row}")?;
    file.flush()?;
    Ok(())
}

fn csv_cell(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn recorder_accepts_concurrent_units_and_publishes_atomically() {
        let recorder = Arc::new(Recorder::new_at(run(), Instant::now()));
        recorder.update(|report| report.units.push(unit()));
        std::thread::scope(|scope| {
            for id in 0..8 {
                let recorder = Arc::clone(&recorder);
                scope.spawn(move || recorder.update(|report| report.counters.files_statted += id));
            }
        });
        let directory =
            std::env::temp_dir().join(format!("corgi-report-test-{}", std::process::id()));
        let path = directory.join("report.json");
        let report = recorder.finish_to_path(&path).unwrap();
        assert!(path.exists());
        assert_eq!(report.counters.files_statted, 28);
        let persisted: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(persisted["schema_version"], 3);
        assert_eq!(persisted["units"][0]["id"], "demo:compile:1234");
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn concurrent_csv_appends_write_one_header_and_complete_rows() {
        let directory = std::env::temp_dir().join(format!("corgi-csv-test-{}", std::process::id()));
        fs::remove_dir_all(&directory).ok();
        let reports: Vec<Report> = (0..16)
            .map(|index| {
                let mut report = Report::new(run());
                report.run.id = format!("run-{index}");
                report
            })
            .collect();
        let mut reports = reports;
        reports[0].run.workspace.root = "workspace,\"quoted\"".into();
        std::thread::scope(|scope| {
            for report in &reports {
                scope.spawn(|| append_run(&directory, report).unwrap());
            }
        });

        let contents = fs::read_to_string(directory.join("metrics/runs.csv")).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), reports.len() + 1);
        assert_eq!(lines[0], RUNS_CSV_HEADER);
        assert!(contents.contains("\"workspace,\"\"quoted\"\"\""));
        for index in 0..reports.len() {
            let run_id = format!("run-{index}");
            assert_eq!(
                lines
                    .iter()
                    .filter(|line| line.split(',').nth(1) == Some(run_id.as_str()))
                    .count(),
                1
            );
        }
        fs::remove_dir_all(directory).unwrap();
    }

    fn run() -> Run {
        Run {
            id: "run-1".into(),
            started_at_unix_ns: 1_781_463_731_000_000_000,
            duration_ns: 0,
            workspace: Workspace {
                root: "/demo".into(),
            },
            command: Command {
                name: "build".into(),
                workspace: true,
                packages: vec![],
                root_set: None,
                profile: "dev".into(),
                target: None,
                features: vec![],
                incremental: true,
                force_tests: false,
                test_filter: None,
                exec_args: vec![],
            },
            tool: Tool {
                corgi_version: "0.1".into(),
                corgi_build_id: "build".into(),
                rustc_version: "rustc".into(),
                host: "host".into(),
                logical_cpus: 1,
                toolchain: ToolchainInput {
                    cc: "clang".into(),
                    ld: "ld".into(),
                    sdk: "15.5".into(),
                    xcode: "16.4".into(),
                },
                declared_environment: vec![],
                host_rustflags: vec![],
                target_rustflags: vec![],
            },
            outcome: Outcome::default(),
        }
    }

    fn unit() -> Unit {
        Unit {
            id: "demo:compile:1234".into(),
            logical_id: "demo:compile:5678".into(),
            package: Package {
                id: "demo 0.1.0".into(),
                name: "demo".into(),
                version: "0.1.0".into(),
                scope: "workspace".into(),
                root: "/demo".into(),
            },
            action: UnitAction {
                kind: "compile".into(),
                host: false,
                is_root: true,
            },
            target: Target {
                name: "demo".into(),
                kinds: vec!["lib".into()],
                crate_types: vec!["lib".into()],
                edition: "2024".into(),
                source: "src/lib.rs".into(),
                platform: "aarch64-apple-darwin".into(),
            },
            profile: json!({"name": "dev"}),
            features: vec!["feature".into()],
            dependencies: vec![UnitDependency {
                unit: "serde:compile:abcd".into(),
                role: "extern".into(),
                name: Some("serde".into()),
            }],
            outcome: UnitOutcome {
                status: UnitStatus::Success,
                message: None,
            },
            cache: UnitCache {
                result: UnitCacheResult::Hit,
                probe: None,
            },
            key: UnitKey {
                hash: "action-key".into(),
                inputs: Some(ActionKeyInputs::Compile(Box::new(CompileKeyInputs {
                    source_hash: "source".into(),
                    declared_environment: vec![],
                    effective_environment_hash: "environment".into(),
                    link_dependencies: vec![],
                    build_script: None,
                    lints: vec![],
                    clippy: None,
                    cap_lints: false,
                    uses_toolchain: true,
                    compiler_identity: "compiler".into(),
                }))),
            },
            timings: Some(UnitTimings::default()),
            outputs: vec![Output {
                name: "libdemo.rlib".into(),
                hash: "output".into(),
                bytes: 42,
            }],
        }
    }
}
