use colored::{Color, Colorize};
use comfy_table::{
    modifiers::UTF8_ROUND_CORNERS,
    presets::{ASCII_FULL, UTF8_FULL},
    Attribute, Cell, CellAlignment, ContentArrangement, Table,
};
use serde::Serialize;
use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::thread;
use std::time::Instant;

use crate::gitutil;
use crate::opts::{AnalyzeConfig, AnalyzeThresholds, Mode, Options};
use std::fs::{create_dir_all, File};

fn color_output_enabled(is_terminal: bool, no_color: bool, force_color: bool) -> bool {
    if no_color {
        return false;
    }
    is_terminal || force_color
}

fn stdout_supports_color() -> bool {
    use std::io::IsTerminal;

    color_output_enabled(
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var_os("FORCE_COLOR").is_some(),
    )
}

fn stderr_supports_color() -> bool {
    use std::io::IsTerminal;

    color_output_enabled(
        std::io::stderr().is_terminal(),
        std::env::var_os("NO_COLOR").is_some(),
        std::env::var_os("FORCE_COLOR").is_some(),
    )
}

fn styled_text(text: &str, color: Color, bold: bool, enabled: bool) -> String {
    if !enabled {
        return text.to_string();
    }

    let styled = text.color(color);
    if bold {
        styled.bold().to_string()
    } else {
        styled.to_string()
    }
}

fn eprintln_color(color: Color, msg: &str) {
    eprintln!(
        "{}",
        styled_text(msg, color, false, stderr_supports_color())
    );
}

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum WarningLevel {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize)]
pub struct Warning {
    pub level: WarningLevel,
    pub message: String,
    pub recommendation: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Concern {
    Ok,
    Info,
    Warning,
    Critical,
}

impl Concern {
    fn label(self) -> &'static str {
        match self {
            Concern::Ok => "OK",
            Concern::Info => "Info",
            Concern::Warning => "Warning",
            Concern::Critical => "Critical",
        }
    }
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct ObjectStat {
    pub oid: String,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct FileStat {
    pub path: String,
    pub size: u64,
    pub versions: usize,
    pub largest_oid: String,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct DirectoryStat {
    pub path: String,
    pub entries: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct PathStat {
    pub path: String,
    pub length: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct CommitMessageStat {
    pub oid: String,
    pub length: usize,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct RepositoryMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workdir: Option<String>,
    pub loose_objects: u64,
    pub loose_size_bytes: u64,
    pub packed_objects: u64,
    pub packed_size_bytes: u64,
    pub total_objects: u64,
    pub total_size_bytes: u64,
    pub object_types: BTreeMap<String, u64>,
    pub object_type_sizes: BTreeMap<String, u64>,
    pub tree_total_size_bytes: u64,
    pub total_tree_entries: u64,
    pub checkout_files: u64,
    pub checkout_directories: u64,
    pub checkout_total_size_bytes: u64,
    pub checkout_max_path_depth: usize,
    pub checkout_symlinks: u64,
    pub checkout_submodules: u64,
    pub refs_total: usize,
    pub refs_heads: usize,
    pub refs_tags: usize,
    pub refs_remotes: usize,
    pub refs_other: usize,
    pub largest_commits: Vec<ObjectStat>,
    pub largest_blobs: Vec<ObjectStat>,
    pub largest_files: Vec<FileStat>,
    pub largest_trees: Vec<ObjectStat>,
    pub largest_tags: Vec<ObjectStat>,
    pub max_tree_entries: Option<DirectoryStat>,
    pub blobs_over_threshold: Vec<ObjectStat>,
    pub directory_hotspots: Option<DirectoryStat>,
    pub longest_path: Option<PathStat>,
    pub max_commit_parents: usize,
    pub oversized_commit_messages: Vec<CommitMessageStat>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AnalysisReport {
    pub metrics: RepositoryMetrics,
    pub warnings: Vec<Warning>,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct BlobSizeStats {
    unpacked_size: HashMap<String, u64>,
    packed_size: HashMap<String, u64>,
    processed_objects: usize,
}

#[derive(Debug, Default)]
struct ReachableInventory {
    oids: Vec<String>,
    object_types: HashMap<String, String>,
    object_sizes: HashMap<String, u64>,
    paths_by_oid: HashMap<String, Vec<String>>,
}

pub fn run(opts: &Options) -> io::Result<()> {
    debug_assert_eq!(opts.mode, Mode::Analyze);
    let report = generate_report(opts)?;
    if opts.analyze.json {
        let json = serde_json::to_string_pretty(&report).map_err(to_io_error)?;
        println!("{}", json);
    } else {
        print_human(&report, &opts.analyze);
    }

    // Write report files if requested
    if opts.write_report || opts.write_report_json {
        // Get the git directory
        let git_dir = match gitutil::git_dir(&opts.target) {
            Ok(dir) => dir,
            Err(_) => opts.target.join(".git"),
        };
        let debug_dir = git_dir.join("filter-repo");
        if !debug_dir.exists() {
            create_dir_all(&debug_dir)?;
        }

        // Write text report
        if opts.write_report {
            let report_path = debug_dir.join("report.txt");
            let mut f = File::create(&report_path)?;
            write_text_report(&mut f, &report, &opts.analyze)?;
            eprintln!("Analysis report written to {}", report_path.display());
        }

        // Write JSON report
        if opts.write_report_json {
            let json_path = debug_dir.join("report.json");
            let mut f = File::create(&json_path)?;
            let json = serde_json::to_string_pretty(&report).map_err(to_io_error)?;
            f.write_all(json.as_bytes())?;
            eprintln!("Analysis JSON report written to {}", json_path.display());
        }
    }

    Ok(())
}

/// Write a plain text analysis report to the given writer using the same row
/// builders as the terminal renderer.
fn write_text_report<W: Write>(
    f: &mut W,
    report: &AnalysisReport,
    cfg: &AnalyzeConfig,
) -> io::Result<()> {
    writeln!(f, "Repository analysis")?;
    writeln!(
        f,
        "Workdir: {}",
        report.metrics.workdir.as_deref().unwrap_or("N/A")
    )?;
    writeln!(f)?;

    write_plain_rows(f, "Repository summary", build_summary_rows(report, cfg))?;
    write_plain_rows(
        f,
        "Object types",
        build_object_type_rows(&report.metrics, cfg),
    )?;
    let checkout_rows = build_checkout_rows(&report.metrics, cfg);
    if !checkout_rows.is_empty() {
        write_plain_rows(f, "Checkout (HEAD)", checkout_rows)?;
    }

    if !report.metrics.largest_files.is_empty() {
        writeln!(f, "Top files by size")?;
        for file in &report.metrics.largest_files {
            writeln!(
                f,
                "- {} | {} | {} versions | {}",
                file.path,
                format_bytes(file.size),
                file.versions,
                &file.largest_oid[..file.largest_oid.len().min(8)]
            )?;
        }
        writeln!(f)?;
    }

    if !report.metrics.largest_blobs.is_empty() {
        writeln!(f, "Top blobs by size")?;
        for blob in &report.metrics.largest_blobs {
            writeln!(
                f,
                "- {} | {} | {}",
                format_bytes(blob.size),
                &blob.oid[..blob.oid.len().min(8)],
                blob.path.as_deref().unwrap_or("")
            )?;
        }
        writeln!(f)?;
    }

    if !report.metrics.largest_trees.is_empty() {
        writeln!(f, "Top trees by size")?;
        for tree in &report.metrics.largest_trees {
            writeln!(
                f,
                "- {} | {}",
                format_bytes(tree.size),
                &tree.oid[..tree.oid.len().min(8)]
            )?;
        }
        writeln!(f)?;
    }

    if !report.metrics.oversized_commit_messages.is_empty() {
        writeln!(f, "Oversized commit messages")?;
        for msg in &report.metrics.oversized_commit_messages {
            writeln!(
                f,
                "- {} bytes | {}",
                msg.length,
                &msg.oid[..msg.oid.len().min(8)]
            )?;
        }
        writeln!(f)?;
    }

    Ok(())
}

fn write_plain_rows<W: Write>(
    f: &mut W,
    title: &str,
    rows: Vec<Vec<Cow<'_, str>>>,
) -> io::Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    writeln!(f, "{}", title)?;
    for row in rows {
        let line = row
            .iter()
            .map(|value| value.as_ref())
            .collect::<Vec<_>>()
            .join(" | ");
        writeln!(f, "- {}", line)?;
    }
    writeln!(f)?;
    Ok(())
}

pub fn generate_report(opts: &Options) -> io::Result<AnalysisReport> {
    // Avoid Windows verbatim (\\?\) paths which can confuse external tools like Git when
    // passed via command-line flags. Use the provided path directly.
    let repo = opts.source.clone();
    let metrics = collect_metrics(&repo, &opts.analyze)?;
    let warnings = evaluate_warnings(&metrics, &opts.analyze.thresholds);
    Ok(AnalysisReport { metrics, warnings })
}

fn collect_metrics(repo: &Path, cfg: &AnalyzeConfig) -> io::Result<RepositoryMetrics> {
    let _start_time = Instant::now();
    let mut metrics = RepositoryMetrics {
        workdir: Some(repo.display().to_string()),
        ..Default::default()
    };

    eprintln_color(Color::Cyan, "[*] Starting repository analysis...");

    // First, get one reachable object universe and size it.
    eprintln_color(Color::Cyan, "[*] Gathering reachable object inventory...");
    let inventory = gather_reachable_inventory(repo)?;

    // Initialize metrics with blob sizes - pre-allocate reasonable capacities
    let estimated_blobs = inventory
        .object_types
        .values()
        .filter(|kind| kind.as_str() == "blob")
        .count();
    let mut stats = StatsCollection {
        blob_paths: HashMap::with_capacity(estimated_blobs),
    };

    for (oid, paths) in &inventory.paths_by_oid {
        if inventory
            .object_types
            .get(oid)
            .is_some_and(|kind| kind == "blob")
        {
            stats.blob_paths.insert(oid.clone(), paths.clone());
        }
    }

    // Optional physical object database stats. These are not used for headline totals.
    gather_footprint(repo, &mut metrics)?;
    gather_refs(repo, &mut metrics)?;

    // Update metrics from the reachable object universe.
    populate_reachable_object_metrics(&mut metrics, &inventory, cfg);

    // One cat-file --batch pass over reachable commits + trees yields max
    // parents, oversized commit messages, and tree-entry counts together.
    eprintln_color(Color::Cyan, "[*] Processing commit history...");
    let history = gather_history_metrics(repo, &inventory, cfg.thresholds.warn_commit_msg_bytes)?;
    metrics.max_commit_parents = history.max_parents;
    metrics.total_tree_entries = history.total_tree_entries;
    metrics.max_tree_entries = history.max_tree_entries;
    metrics.oversized_commit_messages = history.oversized_commit_messages;

    // Find largest blobs and prepare path mappings
    let mut largest_blobs: BinaryHeap<Reverse<(u64, String)>> = BinaryHeap::new();
    let mut threshold_hits: BinaryHeap<Reverse<(u64, String)>> = BinaryHeap::new();

    for oid in stats.blob_paths.keys() {
        let actual_size = inventory.object_sizes.get(oid).copied().unwrap_or(0);
        push_top(&mut largest_blobs, cfg.top, actual_size, oid);
        if actual_size >= cfg.thresholds.warn_blob_bytes {
            push_top(&mut threshold_hits, cfg.top, actual_size, oid);
        }
    }

    // Convert to ObjectStat with paths
    metrics.largest_blobs = heap_to_object_stats_with_paths(largest_blobs, &stats.blob_paths);
    metrics.blobs_over_threshold =
        heap_to_object_stats_with_paths(threshold_hits, &stats.blob_paths);

    // Group blobs by file path to find unique files
    metrics.largest_files =
        compute_largest_files(&stats.blob_paths, &inventory.object_sizes, cfg.top);

    // Keep a quick HEAD snapshot for context (simplified)
    eprintln_color(Color::Cyan, "[*] Analyzing working directory...");
    gather_head_checkout_metrics(repo, &mut metrics)?;

    eprintln_color(Color::Green, "[*] Analysis complete!");
    Ok(metrics)
}

struct StatsCollection {
    blob_paths: HashMap<String, Vec<String>>,
}

fn gather_footprint(repo: &Path, metrics: &mut RepositoryMetrics) -> io::Result<()> {
    let output = run_git_capture(repo, &["count-objects", "-v"])?;
    for line in output.lines() {
        let mut parts = line.splitn(2, ':');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        match key {
            "count" => metrics.loose_objects = value.parse::<u64>().unwrap_or(0),
            "size" => metrics.loose_size_bytes = value.parse::<u64>().unwrap_or(0) * 1024,
            "in-pack" => metrics.packed_objects = value.parse::<u64>().unwrap_or(0),
            "size-pack" => metrics.packed_size_bytes = value.parse::<u64>().unwrap_or(0) * 1024,
            _ => {}
        }
    }
    Ok(())
}

fn gather_reachable_inventory(repo: &Path) -> io::Result<ReachableInventory> {
    let (mut reader, mut child) =
        run_git_capture_stream(repo, &["rev-list", "--objects", "--all"])?;
    let mut inventory = ReachableInventory::default();
    let mut seen = HashSet::new();
    let mut line_buf = String::new();

    while reader.read_line(&mut line_buf)? > 0 {
        let line = line_buf.trim_end();
        if !line.is_empty() {
            let mut parts = line.splitn(2, ' ');
            if let Some(oid) = parts.next() {
                if !oid.is_empty() && seen.insert(oid.to_string()) {
                    inventory.oids.push(oid.to_string());
                }
                if let Some(path) = parts.next() {
                    if !path.is_empty() {
                        inventory
                            .paths_by_oid
                            .entry(oid.to_string())
                            .or_default()
                            .push(path.to_string());
                    }
                }
            }
        }
        line_buf.clear();
    }

    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "git rev-list --objects --all failed: {}",
            status
        )));
    }

    batch_check_reachable_objects(repo, &mut inventory)?;
    Ok(inventory)
}

fn spawn_oid_batch_writer(
    mut stdin: ChildStdin,
    oids: Vec<String>,
) -> thread::JoinHandle<io::Result<()>> {
    thread::spawn(move || {
        for oid in oids {
            stdin.write_all(oid.as_bytes())?;
            stdin.write_all(b"\n")?;
        }
        Ok(())
    })
}

fn batch_check_reachable_objects(
    repo: &Path,
    inventory: &mut ReachableInventory,
) -> io::Result<()> {
    if inventory.oids.is_empty() {
        return Ok(());
    }

    let mut child = Command::new("git")
        .current_dir(repo)
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("failed to open git cat-file stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture git cat-file stdout"))?;
    let writer = spawn_oid_batch_writer(stdin, inventory.oids.clone());
    let mut reader = BufReader::new(stdout);
    let mut line_buf = String::new();

    while reader.read_line(&mut line_buf)? > 0 {
        let line = line_buf.trim_end();
        let mut parts = line.split_whitespace();
        if let (Some(oid), Some(kind), Some(size)) = (parts.next(), parts.next(), parts.next()) {
            if let Ok(size) = size.parse::<u64>() {
                inventory
                    .object_types
                    .insert(oid.to_string(), kind.to_string());
                inventory.object_sizes.insert(oid.to_string(), size);
            }
        }
        line_buf.clear();
    }

    writer
        .join()
        .map_err(|_| io::Error::other("git cat-file oid writer thread panicked"))??;
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "git cat-file --batch-check failed: {}",
            status
        )));
    }
    Ok(())
}

fn populate_reachable_object_metrics(
    metrics: &mut RepositoryMetrics,
    inventory: &ReachableInventory,
    cfg: &AnalyzeConfig,
) {
    // Note: `largest_blobs` and `blobs_over_threshold` are derived later in
    // `collect_metrics` from the blob-path map, so they are intentionally not
    // computed here to avoid a redundant pass.
    let mut largest_commits: BinaryHeap<Reverse<(u64, String)>> = BinaryHeap::new();
    let mut largest_trees: BinaryHeap<Reverse<(u64, String)>> = BinaryHeap::new();
    let mut largest_tags: BinaryHeap<Reverse<(u64, String)>> = BinaryHeap::new();

    metrics.total_objects = inventory.object_types.len() as u64;
    metrics.total_size_bytes = 0;
    metrics.object_types.clear();
    metrics.object_type_sizes.clear();
    metrics.tree_total_size_bytes = 0;

    for (oid, kind) in &inventory.object_types {
        let size = inventory.object_sizes.get(oid).copied().unwrap_or(0);
        *metrics.object_types.entry(kind.clone()).or_insert(0) += 1;
        *metrics.object_type_sizes.entry(kind.clone()).or_insert(0) += size;
        metrics.total_size_bytes = metrics.total_size_bytes.saturating_add(size);

        match kind.as_str() {
            "commit" => push_top(&mut largest_commits, cfg.top, size, oid),
            "tree" => {
                metrics.tree_total_size_bytes = metrics.tree_total_size_bytes.saturating_add(size);
                push_top(&mut largest_trees, cfg.top, size, oid);
            }
            "tag" => push_top(&mut largest_tags, cfg.top, size, oid),
            _ => {}
        }
    }

    metrics.largest_commits =
        heap_to_object_stats_with_paths(largest_commits, &inventory.paths_by_oid);
    metrics.largest_trees = heap_to_object_stats_with_paths(largest_trees, &inventory.paths_by_oid);
    metrics.largest_tags = heap_to_object_stats_with_paths(largest_tags, &inventory.paths_by_oid);
}

/// History/structure metrics derived from one `cat-file --batch` pass over the
/// reachable commit and tree objects. Folding commits and trees into a single
/// pass replaces three separate full-history git invocations (`rev-list
/// --parents --all`, `git log --all`, and a trees-only `cat-file --batch`).
#[derive(Debug, Default)]
struct HistoryMetrics {
    max_parents: usize,
    oversized_commit_messages: Vec<CommitMessageStat>,
    total_tree_entries: u64,
    max_tree_entries: Option<DirectoryStat>,
}

fn gather_history_metrics(
    repo: &Path,
    inventory: &ReachableInventory,
    threshold_bytes: usize,
) -> io::Result<HistoryMetrics> {
    let oids: Vec<String> = inventory
        .object_types
        .iter()
        .filter(|(_, kind)| *kind == "commit" || *kind == "tree")
        .map(|(oid, _)| oid.clone())
        .collect();
    let mut result = HistoryMetrics::default();
    if oids.is_empty() {
        return Ok(result);
    }

    let mut child = Command::new("git")
        .current_dir(repo)
        .args(["cat-file", "--batch"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| io::Error::other("failed to open git cat-file stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture git cat-file stdout"))?;
    let writer = spawn_oid_batch_writer(stdin, oids);
    let mut reader = BufReader::new(stdout);
    let mut header = Vec::with_capacity(96);
    let mut max_tree: Option<(String, u64)> = None;

    loop {
        header.clear();
        if reader.read_until(b'\n', &mut header)? == 0 {
            break;
        }
        if header.ends_with(b"\n") {
            header.pop();
        }
        if header.ends_with(b"\r") {
            header.pop();
        }
        let header_str = String::from_utf8_lossy(&header);
        let mut parts = header_str.split_whitespace();
        let Some(oid) = parts.next() else {
            continue;
        };
        let Some(kind) = parts.next() else {
            continue;
        };
        let Some(size) = parts.next().and_then(|s| s.parse::<usize>().ok()) else {
            continue;
        };
        let oid = oid.to_string();
        let kind = kind.to_string();

        let mut payload = vec![0u8; size];
        reader.read_exact(&mut payload)?;
        let mut trailing = [0u8; 1];
        let _ = reader.read_exact(&mut trailing);

        match kind.as_str() {
            "tree" => {
                // A tree entry is `<mode> <name>\0<raw-hash>`. The raw hash bytes
                // can themselves contain 0x00, so counting NUL bytes would
                // overcount; walk entries skipping the hash (width from oid hex).
                let hash_len = oid.len() / 2;
                let entries = count_tree_entries(&payload, hash_len);
                result.total_tree_entries = result.total_tree_entries.saturating_add(entries);
                if max_tree.as_ref().is_none_or(|(_, best)| entries > *best) {
                    max_tree = Some((oid, entries));
                }
            }
            "commit" => {
                let parents = count_commit_parents(&payload);
                if parents > result.max_parents {
                    result.max_parents = parents;
                }
                if threshold_bytes > 0 {
                    let length = commit_message_len(&payload);
                    if length >= threshold_bytes {
                        result
                            .oversized_commit_messages
                            .push(CommitMessageStat { oid, length });
                    }
                }
            }
            _ => {}
        }
    }

    writer
        .join()
        .map_err(|_| io::Error::other("git cat-file batch writer thread panicked"))??;
    let status = child.wait()?;
    if !status.success() {
        return Err(io::Error::other(format!(
            "git cat-file --batch failed: {}",
            status
        )));
    }

    result.max_tree_entries = max_tree.map(|(oid, entries)| DirectoryStat {
        path: oid,
        entries: entries as usize,
    });
    // The batch pass visits objects in git's enumeration order; sort for a
    // deterministic report (largest message first, then oid).
    result
        .oversized_commit_messages
        .sort_by(|a, b| b.length.cmp(&a.length).then_with(|| a.oid.cmp(&b.oid)));
    Ok(result)
}

/// Count `parent ` header lines in a raw commit object (the parent count, which
/// matches `git rev-list --parents`). Header lines precede the first blank line.
fn count_commit_parents(payload: &[u8]) -> usize {
    let mut parents = 0;
    for line in payload.split(|&b| b == b'\n') {
        if line.is_empty() {
            break; // blank line terminates the header
        }
        if line.starts_with(b"parent ") {
            parents += 1;
        }
    }
    parents
}

/// Length in bytes of a raw commit object's message, i.e. everything after the
/// first empty header line. Equivalent to the byte length of `%B`.
fn commit_message_len(payload: &[u8]) -> usize {
    match payload.windows(2).position(|w| w == b"\n\n") {
        Some(pos) => payload.len() - (pos + 2),
        None => 0,
    }
}

/// Count the entries in a raw git tree payload. Each entry is
/// `<mode> <name>\0<raw-hash>`; `hash_len` is the raw hash width in bytes
/// (20 for SHA-1, 32 for SHA-256).
fn count_tree_entries(payload: &[u8], hash_len: usize) -> u64 {
    if hash_len == 0 {
        return 0;
    }
    let mut pos = 0usize;
    let mut count = 0u64;
    while pos < payload.len() {
        match payload[pos..].iter().position(|&b| b == 0) {
            Some(rel) => {
                pos += rel + 1 + hash_len;
                count += 1;
            }
            None => break,
        }
    }
    count
}

fn gather_head_checkout_metrics(repo: &Path, metrics: &mut RepositoryMetrics) -> io::Result<()> {
    let output = Command::new("git")
        .current_dir(repo)
        .args(["ls-tree", "-r", "-z", "--long", "HEAD"])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }

    let mut directories: HashSet<String> = HashSet::new();
    let mut directory_entries: HashMap<String, usize> = HashMap::new();
    let mut longest_path: Option<PathStat> = None;

    for record in output.stdout.split(|&b| b == 0) {
        if record.is_empty() {
            continue;
        }
        let Some(tab_pos) = record.iter().position(|&b| b == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&record[..tab_pos]);
        let path = String::from_utf8_lossy(&record[tab_pos + 1..]).to_string();
        let mut parts = meta.split_whitespace();
        let mode = parts.next().unwrap_or("");
        let kind = parts.next().unwrap_or("");
        let _oid = parts.next();
        let size = parts.next().unwrap_or("");

        if mode == "160000" || kind == "commit" {
            metrics.checkout_submodules += 1;
            continue;
        }
        if mode == "120000" {
            metrics.checkout_symlinks += 1;
        }
        if kind == "blob" {
            metrics.checkout_files += 1;
            if let Ok(size) = size.parse::<u64>() {
                metrics.checkout_total_size_bytes =
                    metrics.checkout_total_size_bytes.saturating_add(size);
            }
        }

        let path_len = path.len();
        if longest_path
            .as_ref()
            .is_none_or(|current| path_len > current.length)
        {
            longest_path = Some(PathStat {
                path: path.clone(),
                length: path_len,
            });
        }

        let components: Vec<&str> = path.split('/').collect();
        metrics.checkout_max_path_depth = metrics.checkout_max_path_depth.max(components.len());
        if components.len() > 1 {
            let mut prefix = String::new();
            for component in &components[..components.len() - 1] {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                directories.insert(prefix.clone());
            }
            let parent = components[..components.len() - 1].join("/");
            *directory_entries.entry(parent).or_insert(0) += 1;
        }
    }

    metrics.checkout_directories = directories.len() as u64;
    metrics.directory_hotspots = directory_entries
        .into_iter()
        .max_by_key(|(_, entries)| *entries)
        .map(|(path, entries)| DirectoryStat { path, entries });
    metrics.longest_path = longest_path;
    Ok(())
}

#[cfg(test)]
fn collect_blob_sizes_from_reader<R: BufRead>(reader: &mut R) -> io::Result<BlobSizeStats> {
    let mut unpacked_size = HashMap::new();
    let mut packed_size = HashMap::new();
    let mut processed_objects = 0usize;
    let mut line_buf = String::new();

    while reader.read_line(&mut line_buf)? > 0 {
        let trimmed = line_buf.trim();
        if !trimmed.is_empty() {
            let mut parts_iter = trimmed.split_whitespace();
            if let (Some(sha), Some(objtype), Some(objsize_str), Some(objdisksize_str)) = (
                parts_iter.next(),
                parts_iter.next(),
                parts_iter.next(),
                parts_iter.next(),
            ) {
                if objtype == "blob" {
                    if let (Ok(objsize), Ok(objdisksize)) =
                        (objsize_str.parse::<u64>(), objdisksize_str.parse::<u64>())
                    {
                        unpacked_size.insert(sha.to_string(), objsize);
                        packed_size.insert(sha.to_string(), objdisksize);
                    }
                }
            }
            processed_objects += 1;
        }
        line_buf.clear();
    }

    Ok(BlobSizeStats {
        unpacked_size,
        packed_size,
        processed_objects,
    })
}

fn gather_refs(repo: &Path, metrics: &mut RepositoryMetrics) -> io::Result<()> {
    let refs = gitutil::get_all_refs(repo)?;
    for name in refs.keys() {
        let name = name.as_str();
        metrics.refs_total += 1;
        if name.starts_with("refs/heads/") {
            metrics.refs_heads += 1;
        } else if name.starts_with("refs/tags/") {
            metrics.refs_tags += 1;
        } else if name.starts_with("refs/remotes/") {
            metrics.refs_remotes += 1;
        } else {
            metrics.refs_other += 1;
        }
    }
    Ok(())
}

fn evaluate_warnings(metrics: &RepositoryMetrics, thresholds: &AnalyzeThresholds) -> Vec<Warning> {
    let mut warnings = Vec::new();
    if metrics.total_size_bytes >= thresholds.crit_total_bytes {
        warnings.push(Warning {
      level: WarningLevel::Critical,
      message: format!(
        "Repository is {:.2} GiB (threshold {:.2} GiB).", to_gib(metrics.total_size_bytes), to_gib(thresholds.crit_total_bytes)
      ),
      recommendation: Some("Avoid storing generated files or large media in Git; consider Git-LFS or external storage.".to_string()),
    });
    } else if metrics.total_size_bytes >= thresholds.warn_total_bytes {
        warnings.push(Warning {
            level: WarningLevel::Warning,
            message: format!(
                "Repository is {:.2} GiB (warning threshold {:.2} GiB).",
                to_gib(metrics.total_size_bytes),
                to_gib(thresholds.warn_total_bytes)
            ),
            recommendation: Some(
                "Prune large assets or split the project to keep Git operations fast.".to_string(),
            ),
        });
    }
    if metrics.refs_total >= thresholds.warn_ref_count {
        warnings.push(Warning {
            level: WarningLevel::Warning,
            message: format!(
                "Repository has {} refs (warning threshold {}).",
                metrics.refs_total, thresholds.warn_ref_count
            ),
            recommendation: Some(
                "Delete stale branches/tags or move rarely-needed refs to a separate remote."
                    .to_string(),
            ),
        });
    }
    if metrics.total_objects as usize >= thresholds.warn_object_count {
        warnings.push(Warning {
      level: WarningLevel::Warning,
      message: format!(
        "Repository contains {} Git objects (warning threshold {}).",
        metrics.total_objects,
        thresholds.warn_object_count
      ),
      recommendation: Some("Consider sharding the project or aggregating many tiny files to reduce object churn.".to_string()),
    });
    }
    if let Some(dir) = &metrics.directory_hotspots {
        if dir.entries >= thresholds.warn_tree_entries {
            warnings.push(Warning {
        level: WarningLevel::Warning,
        message: format!(
          "Directory '{}' has {} entries (threshold {}).", dir.path, dir.entries, thresholds.warn_tree_entries
        ),
        recommendation: Some("Shard large directories into smaller subdirectories to keep tree traversals fast.".to_string()),
      });
        }
    }
    if let Some(path) = &metrics.longest_path {
        if path.length >= thresholds.warn_path_length {
            warnings.push(Warning {
        level: WarningLevel::Warning,
        message: format!(
          "Path '{}' is {} characters long (threshold {}).", path.path, path.length, thresholds.warn_path_length
        ),
        recommendation: Some("Shorten deeply nested names to improve compatibility with tooling and filesystems.".to_string()),
      });
        }
    }
    for blob in &metrics.blobs_over_threshold {
        warnings.push(Warning {
            level: WarningLevel::Warning,
            message: format!(
                "Blob {} is {:.2} MiB (threshold {:.2} MiB).",
                blob.oid,
                to_mib(blob.size),
                to_mib(thresholds.warn_blob_bytes)
            ),
            recommendation: Some(
                "Track large files with Git-LFS or store them outside the repository.".to_string(),
            ),
        });
    }
    if metrics.max_commit_parents > thresholds.warn_max_parents {
        warnings.push(Warning {
            level: WarningLevel::Info,
            message: format!(
        "Commit with {} parents detected (threshold {}). Octopus merges can complicate history.",
        metrics.max_commit_parents,
        thresholds.warn_max_parents
      ),
            recommendation: Some(
                "Consider rebasing large merge trains or splitting history to simplify traversal."
                    .to_string(),
            ),
        });
    }
    for msg in &metrics.oversized_commit_messages {
        warnings.push(Warning {
            level: WarningLevel::Info,
            message: format!(
                "Commit {} has a {} byte message (threshold {}).",
                msg.oid, msg.length, thresholds.warn_commit_msg_bytes
            ),
            recommendation: Some(
                "Store large logs or dumps outside Git; keep commit messages concise.".to_string(),
            ),
        });
    }
    if warnings.is_empty() {
        warnings.push(Warning {
            level: WarningLevel::Info,
            message: "No size-related issues detected above configured thresholds.".to_string(),
            recommendation: None,
        });
    }
    warnings
}

fn print_human(report: &AnalysisReport, cfg: &AnalyzeConfig) {
    println!("{}", banner("Repository analysis"));
    if let Some(path) = &report.metrics.workdir {
        println!("{}", path);
    }

    print_section("Repository summary");
    let rows = build_summary_rows(report, cfg);
    print_table(
        &[
            ("Name", CellAlignment::Left),
            ("Value", CellAlignment::Right),
            ("Concern", CellAlignment::Center),
        ],
        rows,
    );

    print_section("Object types");
    print_table(
        &[
            ("Type", CellAlignment::Left),
            ("Count", CellAlignment::Right),
            ("Total", CellAlignment::Right),
            ("Max", CellAlignment::Right),
            ("Concern", CellAlignment::Center),
        ],
        build_object_type_rows(&report.metrics, cfg),
    );

    let checkout_rows = build_checkout_rows(&report.metrics, cfg);
    if !checkout_rows.is_empty() {
        print_section("Checkout (HEAD)");
        print_table(
            &[
                ("Metric", CellAlignment::Left),
                ("Value", CellAlignment::Left),
                ("Details", CellAlignment::Left),
                ("Concern", CellAlignment::Center),
            ],
            checkout_rows,
        );
    }

    // Show largest files (unique files, grouped by path) instead of individual blob versions
    if !report.metrics.largest_files.is_empty() {
        println!(
            "Top {} files by size:",
            format_count(report.metrics.largest_files.len() as u64)
        );
        let rows: Vec<Vec<Cow<'_, str>>> = report
            .metrics
            .largest_files
            .iter()
            .enumerate()
            .map(|(idx, file)| {
                let truncated_oid = format!("{:.8}", file.largest_oid);
                vec![
                    Cow::Owned(format!("{}", idx + 1)),
                    Cow::Owned(format_bytes(file.size)),
                    Cow::Owned(file.path.clone()),
                    Cow::Owned(format!("{} ver", file.versions)),
                    Cow::Owned(truncated_oid),
                ]
            })
            .collect();
        print_table(
            &[
                ("#", CellAlignment::Right),
                ("Size", CellAlignment::Right),
                ("Path", CellAlignment::Left),
                ("Vers", CellAlignment::Center),
                ("OID", CellAlignment::Center),
            ],
            rows,
        );
    }
    if !report.metrics.largest_blobs.is_empty() {
        println!(
            "Top {} blobs by size:",
            format_count(report.metrics.largest_blobs.len() as u64)
        );
        let rows: Vec<Vec<Cow<'_, str>>> = report
            .metrics
            .largest_blobs
            .iter()
            .enumerate()
            .map(|(idx, blob)| {
                vec![
                    Cow::Owned(format!("{}", idx + 1)),
                    Cow::Owned(format_bytes(blob.size)),
                    Cow::Owned(blob.path.clone().unwrap_or_default()),
                    Cow::Owned(format!("{:.8}", blob.oid)),
                    Cow::Borrowed(
                        concern_for_warn_u64(blob.size, cfg.thresholds.warn_blob_bytes).label(),
                    ),
                ]
            })
            .collect();
        print_table(
            &[
                ("#", CellAlignment::Right),
                ("Size", CellAlignment::Right),
                ("Path", CellAlignment::Left),
                ("OID", CellAlignment::Center),
                ("Concern", CellAlignment::Center),
            ],
            rows,
        );
    }
    if !report.metrics.largest_trees.is_empty() {
        println!(
            "Top {} trees by size:",
            format_count(report.metrics.largest_trees.len() as u64)
        );
        let rows: Vec<Vec<Cow<'_, str>>> = report
            .metrics
            .largest_trees
            .iter()
            .enumerate()
            .map(|(idx, tree)| {
                let truncated_oid = format!("{:.8}", tree.oid);
                vec![
                    Cow::Owned(format!("{}", idx + 1)),
                    Cow::Owned(format_bytes(tree.size)),
                    Cow::Owned(truncated_oid),
                ]
            })
            .collect();
        print_table(
            &[
                ("#", CellAlignment::Right),
                ("Size", CellAlignment::Right),
                ("OID", CellAlignment::Center),
                ("Concern", CellAlignment::Center),
            ],
            rows.into_iter()
                .map(|mut row: Vec<Cow<'_, str>>| {
                    row.push(Cow::Borrowed("Info"));
                    row
                })
                .collect(),
        );
    }

    // History oddities are summarized above; keep oversized messages as a list
    if !report.metrics.oversized_commit_messages.is_empty() {
        println!("Oversized commit messages:");
        let rows = report
            .metrics
            .oversized_commit_messages
            .iter()
            .enumerate()
            .map(|(idx, msg)| {
                let truncated_oid = format!("{:.8}", msg.oid);
                vec![
                    Cow::Owned(format!("{}", idx + 1)),
                    Cow::Owned(format_count(msg.length as u64)),
                    Cow::Owned(truncated_oid),
                    Cow::Borrowed(
                        concern_for_warn_usize(msg.length, cfg.thresholds.warn_commit_msg_bytes)
                            .label(),
                    ),
                ]
            })
            .collect();
        print_table(
            &[
                ("#", CellAlignment::Right),
                ("Bytes", CellAlignment::Right),
                ("OID", CellAlignment::Center),
                ("Concern", CellAlignment::Center),
            ],
            rows,
        );
    }
}

fn run_git_capture(repo: &Path, args: &[&str]) -> io::Result<String> {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()?;
    if !out.status.success() {
        return Err(io::Error::other(format!("git {:?} failed", args)));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Stream-based git command runner for memory-efficient processing.
///
/// This function can replace run_git_capture when processing large outputs
/// to avoid loading the entire output into memory.
///
/// Returns a tuple of (BufReader, Child) so caller can wait on the child
/// to ensure the command succeeded.
fn run_git_capture_stream(
    repo: &Path,
    args: &[&str],
) -> io::Result<(BufReader<ChildStdout>, Child)> {
    let mut cmd = Command::new("git")
        .current_dir(repo)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let stdout = cmd
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("failed to capture git stdout"))?;
    Ok((BufReader::new(stdout), cmd))
}

#[cfg(test)]
fn flush_progress_writer<W: Write>(writer: &mut W) -> io::Result<bool> {
    match writer.flush() {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::BrokenPipe => Ok(false),
        Err(err) => Err(err),
    }
}

fn to_mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

fn to_gib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0 / 1024.0
}

fn to_io_error(err: serde_json::Error) -> io::Error {
    io::Error::other(err)
}

fn heap_to_object_stats_with_paths(
    heap: BinaryHeap<Reverse<(u64, String)>>,
    blob_paths: &HashMap<String, Vec<String>>,
) -> Vec<ObjectStat> {
    heap.into_sorted_vec()
        .into_iter()
        .map(|Reverse((size, oid))| {
            let path = blob_paths
                .get(&oid)
                .and_then(|paths| paths.first().cloned());
            ObjectStat { oid, size, path }
        })
        .collect()
}

fn compute_largest_files(
    blob_paths: &HashMap<String, Vec<String>>,
    object_sizes: &HashMap<String, u64>,
    top: usize,
) -> Vec<FileStat> {
    if top == 0 {
        return Vec::new();
    }

    let mut file_map: HashMap<String, (u64, String, usize)> = HashMap::new();

    for (oid, paths) in blob_paths {
        let size = object_sizes.get(oid).copied().unwrap_or(0);

        for path in paths {
            let entry = file_map.entry(path.clone()).or_insert((0, oid.clone(), 0));
            if size > entry.0 {
                entry.0 = size;
                entry.1 = oid.clone();
            }
            entry.2 += 1;
        }
    }

    let mut files: Vec<FileStat> = file_map
        .into_iter()
        .map(|(path, (size, largest_oid, versions))| FileStat {
            path,
            size,
            versions,
            largest_oid,
        })
        .collect();

    files.sort_by(|a, b| b.size.cmp(&a.size));
    files.truncate(top);
    files
}

fn push_top(heap: &mut BinaryHeap<Reverse<(u64, String)>>, limit: usize, size: u64, oid: &str) {
    if limit == 0 {
        return;
    }
    let entry = Reverse((size, oid.to_string()));
    if heap.len() < limit {
        heap.push(entry);
    } else if let Some(Reverse((min_size, _))) = heap.peek() {
        if size > *min_size {
            heap.pop();
            heap.push(entry);
        }
    }
}

fn banner(title: &str) -> String {
    let banner = format!(
        "{:=^width$}",
        format!(" {} ", title),
        width = render_width()
    );
    styled_text(&banner, Color::Cyan, true, stdout_supports_color())
}

fn print_section(title: &str) {
    println!();
    let section = format!(
        "{:-^width$}",
        format!(" {} ", title),
        width = render_width()
    );
    println!(
        "{}",
        styled_text(&section, Color::Cyan, true, stdout_supports_color())
    );
}

fn print_table(headers: &[(&str, CellAlignment)], rows: Vec<Vec<Cow<'_, str>>>) {
    if rows.is_empty() {
        return;
    }
    let mut table = Table::new();
    if stdout_prefers_utf8() {
        table.load_preset(UTF8_FULL);
        table.apply_modifier(UTF8_ROUND_CORNERS);
    } else {
        table.load_preset(ASCII_FULL);
    }
    table.set_content_arrangement(ContentArrangement::Dynamic);
    table.set_width(render_width() as u16);

    let header_cells = headers
        .iter()
        .map(|(title, align)| {
            Cell::new(*title)
                .add_attribute(Attribute::Bold)
                .set_alignment(*align)
        })
        .collect::<Vec<_>>();
    table.set_header(header_cells);

    for row in rows {
        let cells = headers
            .iter()
            .zip(row.into_iter())
            .map(|((_, align), value)| Cell::new(value.as_ref()).set_alignment(*align))
            .collect::<Vec<_>>();
        table.add_row(cells);
    }

    for line in table.to_string().lines() {
        println!("{}", line);
    }
}

fn render_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|width| *width >= 40)
        .unwrap_or(80)
        .min(120)
}

fn stdout_prefers_utf8() -> bool {
    use std::io::IsTerminal;

    if !io::stdout().is_terminal() {
        return false;
    }
    let locale = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_CTYPE"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default()
        .to_ascii_uppercase();
    locale.contains("UTF-8") || locale.contains("UTF8")
}

fn format_count<T: Into<u64>>(value: T) -> String {
    let digits: Vec<char> = value.into().to_string().chars().rev().collect();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.into_iter().enumerate() {
        if i > 0 && i % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out.chars().rev().collect()
}

/// Render a byte count with an adaptive binary unit so small objects (commits,
/// trees, tags) are not all flattened to `0.00 MiB`.
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let value = bytes as f64;
    if value >= GIB {
        format!("{:.2} GiB", value / GIB)
    } else if value >= MIB {
        format!("{:.2} MiB", value / MIB)
    } else if value >= KIB {
        format!("{:.2} KiB", value / KIB)
    } else {
        format!("{} B", bytes)
    }
}

fn build_summary_rows<'a>(
    report: &'a AnalysisReport,
    cfg: &AnalyzeConfig,
) -> Vec<Vec<Cow<'a, str>>> {
    let metrics = &report.metrics;
    let mut rows: Vec<Vec<Cow<'_, str>>> = Vec::new();
    let thresholds = &cfg.thresholds;

    let highest = worst_human_concern(&report.warnings);
    rows.push(vec![
        Cow::Borrowed("Highest concern"),
        Cow::Borrowed(highest.label()),
        Cow::Borrowed(highest.label()),
    ]);
    rows.push(vec![
        Cow::Borrowed("Reachable objects"),
        Cow::Owned(format_count(metrics.total_objects)),
        Cow::Borrowed(
            concern_for_warn_usize(metrics.total_objects as usize, thresholds.warn_object_count)
                .label(),
        ),
    ]);
    rows.push(vec![
        Cow::Borrowed("Reachable object size"),
        Cow::Owned(format_bytes(metrics.total_size_bytes)),
        Cow::Borrowed(
            concern_for_warn_crit_u64(
                metrics.total_size_bytes,
                thresholds.warn_total_bytes,
                thresholds.crit_total_bytes,
            )
            .label(),
        ),
    ]);
    rows.push(vec![
        Cow::Borrowed("Physical loose objects"),
        Cow::Owned(format!(
            "{} ({})",
            format_count(metrics.loose_objects),
            format_bytes(metrics.loose_size_bytes)
        )),
        Cow::Borrowed("Info"),
    ]);
    rows.push(vec![
        Cow::Borrowed("Physical packed objects"),
        Cow::Owned(format!(
            "{} ({})",
            format_count(metrics.packed_objects),
            format_bytes(metrics.packed_size_bytes)
        )),
        Cow::Borrowed("Info"),
    ]);
    rows.push(vec![
        Cow::Borrowed("Refs"),
        Cow::Owned(format_count(metrics.refs_total as u64)),
        Cow::Borrowed(
            concern_for_warn_usize(metrics.refs_total, thresholds.warn_ref_count).label(),
        ),
    ]);
    rows.push(vec![
        Cow::Borrowed("Max commit parents"),
        Cow::Owned(format_count(metrics.max_commit_parents as u64)),
        Cow::Borrowed(
            concern_for_warn_usize(metrics.max_commit_parents, thresholds.warn_max_parents).label(),
        ),
    ]);
    rows.push(vec![
        Cow::Borrowed("Total tree entries"),
        Cow::Owned(format_count(metrics.total_tree_entries)),
        Cow::Borrowed(
            metrics
                .max_tree_entries
                .as_ref()
                .map(|tree| concern_for_warn_usize(tree.entries, thresholds.warn_tree_entries))
                .unwrap_or(Concern::Ok)
                .label(),
        ),
    ]);

    rows
}

fn build_object_type_rows(
    metrics: &RepositoryMetrics,
    cfg: &AnalyzeConfig,
) -> Vec<Vec<Cow<'static, str>>> {
    ["commit", "tree", "blob", "tag"]
        .into_iter()
        .filter_map(|kind| {
            let count = metrics.object_types.get(kind).copied().unwrap_or(0);
            if count == 0 {
                return None;
            }
            let total = metrics.object_type_sizes.get(kind).copied().unwrap_or(0);
            let max = max_object_size(metrics, kind);
            let concern = match kind {
                "blob" => concern_for_warn_u64(max, cfg.thresholds.warn_blob_bytes),
                _ => Concern::Info,
            };
            Some(vec![
                Cow::Owned(kind.to_string()),
                Cow::Owned(format_count(count)),
                Cow::Owned(format_bytes(total)),
                Cow::Owned(format_bytes(max)),
                Cow::Borrowed(concern.label()),
            ])
        })
        .collect()
}

fn build_checkout_rows<'a>(
    metrics: &'a RepositoryMetrics,
    cfg: &AnalyzeConfig,
) -> Vec<Vec<Cow<'a, str>>> {
    let mut rows = vec![
        vec![
            Cow::Borrowed("Files"),
            Cow::Owned(format_count(metrics.checkout_files)),
            Cow::Owned(format_bytes(metrics.checkout_total_size_bytes)),
            Cow::Borrowed("Info"),
        ],
        vec![
            Cow::Borrowed("Directories"),
            Cow::Owned(format_count(metrics.checkout_directories)),
            Cow::Borrowed(""),
            Cow::Borrowed("Info"),
        ],
        vec![
            Cow::Borrowed("Max path depth"),
            Cow::Owned(format_count(metrics.checkout_max_path_depth as u64)),
            Cow::Borrowed("components"),
            Cow::Borrowed("Info"),
        ],
    ];
    if metrics.checkout_symlinks > 0 {
        rows.push(vec![
            Cow::Borrowed("Symlinks"),
            Cow::Owned(format_count(metrics.checkout_symlinks)),
            Cow::Borrowed(""),
            Cow::Borrowed("Info"),
        ]);
    }
    if metrics.checkout_submodules > 0 {
        rows.push(vec![
            Cow::Borrowed("Submodules"),
            Cow::Owned(format_count(metrics.checkout_submodules)),
            Cow::Borrowed(""),
            Cow::Borrowed("Info"),
        ]);
    }
    if let Some(dir) = &metrics.directory_hotspots {
        rows.push(vec![
            Cow::Borrowed("Busiest directory"),
            Cow::Borrowed(dir.path.as_str()),
            Cow::Owned(format!("{} entries", format_count(dir.entries as u64))),
            Cow::Borrowed(
                concern_for_warn_usize(dir.entries, cfg.thresholds.warn_tree_entries).label(),
            ),
        ]);
    }
    if let Some(path) = &metrics.longest_path {
        rows.push(vec![
            Cow::Borrowed("Max path length"),
            Cow::Borrowed(path.path.as_str()),
            Cow::Owned(format!("{} chars", format_count(path.length as u64))),
            Cow::Borrowed(
                concern_for_warn_usize(path.length, cfg.thresholds.warn_path_length).label(),
            ),
        ]);
    }
    rows
}

fn max_object_size(metrics: &RepositoryMetrics, kind: &str) -> u64 {
    let objects = match kind {
        "commit" => &metrics.largest_commits,
        "tree" => &metrics.largest_trees,
        "blob" => &metrics.largest_blobs,
        "tag" => &metrics.largest_tags,
        _ => return 0,
    };
    objects.iter().map(|object| object.size).max().unwrap_or(0)
}

fn worst_human_concern(warnings: &[Warning]) -> Concern {
    warnings
        .iter()
        .filter(|warning| !warning.message.starts_with("No size-related issues"))
        .map(|warning| match warning.level {
            WarningLevel::Info => Concern::Info,
            WarningLevel::Warning => Concern::Warning,
            WarningLevel::Critical => Concern::Critical,
        })
        .max()
        .unwrap_or(Concern::Ok)
}

fn concern_for_warn_usize(value: usize, warn: usize) -> Concern {
    if warn > 0 && value >= warn {
        Concern::Warning
    } else {
        Concern::Ok
    }
}

fn concern_for_warn_u64(value: u64, warn: u64) -> Concern {
    if warn > 0 && value >= warn {
        Concern::Warning
    } else {
        Concern::Ok
    }
}

fn concern_for_warn_crit_u64(value: u64, warn: u64, critical: u64) -> Concern {
    if critical > 0 && value >= critical {
        Concern::Critical
    } else {
        concern_for_warn_u64(value, warn)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        collect_blob_sizes_from_reader, color_output_enabled, commit_message_len,
        count_commit_parents, count_tree_entries, flush_progress_writer, format_bytes,
    };
    use std::io::{Cursor, ErrorKind, Write};

    fn sha1_tree_entry(mode: &str, name: &str, hash: [u8; 20]) -> Vec<u8> {
        let mut entry = Vec::new();
        entry.extend_from_slice(mode.as_bytes());
        entry.push(b' ');
        entry.extend_from_slice(name.as_bytes());
        entry.push(0);
        entry.extend_from_slice(&hash);
        entry
    }

    #[test]
    fn count_tree_entries_ignores_nul_bytes_inside_hashes() {
        // Two entries whose raw SHA-1 values both contain 0x00 bytes; a naive
        // NUL-byte count would overcount, the structured walk must return 2.
        let mut payload = Vec::new();
        payload.extend(sha1_tree_entry("100644", "a.txt", [0u8; 20]));
        let mut hash = [0xABu8; 20];
        hash[3] = 0x00;
        hash[9] = 0x00;
        payload.extend(sha1_tree_entry("100644", "b.txt", hash));

        assert_eq!(count_tree_entries(&payload, 20), 2);
    }

    #[test]
    fn count_tree_entries_handles_empty_payload_and_zero_hash_len() {
        assert_eq!(count_tree_entries(&[], 20), 0);
        assert_eq!(count_tree_entries(&[1, 2, 3], 0), 0);
    }

    #[test]
    fn format_bytes_scales_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.00 KiB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.00 MiB");
    }

    struct ErrorWriter {
        kind: ErrorKind,
    }

    impl Write for ErrorWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::new(self.kind, "forced flush error"))
        }
    }

    #[test]
    fn collect_blob_sizes_from_reader_tracks_only_blob_entries() {
        let input = "\
aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa blob 10 8
bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb tree 7 5
cccccccccccccccccccccccccccccccccccccccc blob 42 21
";
        let mut reader = Cursor::new(input.as_bytes());

        let stats = collect_blob_sizes_from_reader(&mut reader).expect("parse batch output");

        assert_eq!(
            stats.processed_objects, 3,
            "expected all non-empty lines to be processed"
        );
        assert_eq!(stats.unpacked_size.len(), 2, "expected only blob entries");
        assert_eq!(stats.packed_size.len(), 2, "expected only blob entries");
        assert_eq!(
            stats
                .unpacked_size
                .get("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            Some(&10)
        );
        assert_eq!(
            stats
                .packed_size
                .get("cccccccccccccccccccccccccccccccccccccccc"),
            Some(&21)
        );
    }

    #[test]
    fn collect_blob_sizes_from_reader_skips_malformed_or_invalid_sizes() {
        let input = "\
invalid line
dddddddddddddddddddddddddddddddddddddddd blob NaN 1
eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee blob 11 not-a-number
ffffffffffffffffffffffffffffffffffffffff blob 12 6
";
        let mut reader = Cursor::new(input.as_bytes());

        let stats = collect_blob_sizes_from_reader(&mut reader).expect("parse batch output");

        assert_eq!(stats.processed_objects, 4);
        assert_eq!(stats.unpacked_size.len(), 1);
        assert_eq!(stats.packed_size.len(), 1);
        assert_eq!(
            stats
                .unpacked_size
                .get("ffffffffffffffffffffffffffffffffffffffff"),
            Some(&12)
        );
        assert_eq!(
            stats
                .packed_size
                .get("ffffffffffffffffffffffffffffffffffffffff"),
            Some(&6)
        );
    }

    #[test]
    fn flush_progress_writer_treats_broken_pipe_as_non_fatal() {
        let mut writer = ErrorWriter {
            kind: ErrorKind::BrokenPipe,
        };
        let result = flush_progress_writer(&mut writer);
        assert!(result.is_ok(), "BrokenPipe should not propagate as error");
        assert!(
            !result.expect("BrokenPipe should map to non-fatal false"),
            "BrokenPipe should return false to indicate no further progress output"
        );
    }

    #[test]
    fn flush_progress_writer_propagates_other_flush_errors() {
        let mut writer = ErrorWriter {
            kind: ErrorKind::PermissionDenied,
        };
        let result = flush_progress_writer(&mut writer);
        assert!(
            result.is_err(),
            "non-BrokenPipe flush errors should propagate"
        );
    }

    #[test]
    fn color_output_enabled_respects_no_color_and_force_color() {
        assert!(color_output_enabled(true, false, false));
        assert!(!color_output_enabled(false, false, false));
        assert!(color_output_enabled(false, false, true));
        assert!(!color_output_enabled(true, true, false));
        assert!(!color_output_enabled(false, true, true));
    }

    fn commit_object(parents: usize, message: &str) -> Vec<u8> {
        let mut obj = Vec::new();
        obj.extend_from_slice(b"tree 1111111111111111111111111111111111111111\n");
        for _ in 0..parents {
            obj.extend_from_slice(b"parent 2222222222222222222222222222222222222222\n");
        }
        obj.extend_from_slice(b"author A <a@example.com> 0 +0000\n");
        obj.extend_from_slice(b"committer C <c@example.com> 0 +0000\n");
        obj.push(b'\n');
        obj.extend_from_slice(message.as_bytes());
        obj
    }

    #[test]
    fn count_commit_parents_counts_only_header_parent_lines() {
        assert_eq!(count_commit_parents(&commit_object(0, "msg\n")), 0);
        assert_eq!(count_commit_parents(&commit_object(1, "msg\n")), 1);
        // A "parent " occurrence inside the message must not be counted.
        assert_eq!(
            count_commit_parents(&commit_object(2, "parent of all\nbody\n")),
            2
        );
    }

    #[test]
    fn commit_message_len_matches_bytes_after_header_separator() {
        // Single-line message keeps its trailing newline (matching %B).
        assert_eq!(commit_message_len(&commit_object(0, "hello\n")), 6);
        // Multi-paragraph message: the internal blank line is part of the body.
        let msg = "subject\n\nbody\n";
        assert_eq!(commit_message_len(&commit_object(1, msg)), msg.len());
        // Non-ASCII bytes are counted by byte length, not chars.
        let utf8 = "日本語\n";
        assert_eq!(commit_message_len(&commit_object(0, utf8)), utf8.len());
        // Empty message => zero length.
        assert_eq!(commit_message_len(&commit_object(0, "")), 0);
    }
}
