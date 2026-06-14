use filter_repo_rs as fr;

mod common;
use common::*;
use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Output {
    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .expect("poll analyze child process")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("collect analyze child output");
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out analyze output");
            panic!(
                "analyze timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                timeout,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
fn find_git_on_path() -> String {
    std::env::var_os("PATH")
        .and_then(|path| {
            std::env::split_paths(&path)
                .map(|dir| dir.join("git"))
                .find(|candidate| candidate.is_file())
        })
        .expect("git should be on PATH")
        .to_string_lossy()
        .into_owned()
}

#[cfg(unix)]
fn prepend_path(command: &mut std::process::Command, dir: &Path) {
    let current_path = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![dir.to_path_buf()];
    paths.extend(std::env::split_paths(&current_path));
    command.env("PATH", std::env::join_paths(paths).expect("join PATH"));
}

#[cfg(unix)]
fn write_rev_list_stdout_flood_git(repo: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = repo.join("fake-git-bin");
    std::fs::create_dir_all(&bin_dir).expect("create fake git dir");
    let git_path = bin_dir.join("git");
    let script = r#"#!/bin/sh
saw_rev_list=0
saw_objects=0
saw_all=0
for arg in "$@"; do
  if [ "$arg" = "rev-list" ]; then
    saw_rev_list=1
  fi
  if [ "$arg" = "--objects" ]; then
    saw_objects=1
  fi
  if [ "$arg" = "--all" ]; then
    saw_all=1
  fi
done
if [ "$saw_rev_list" = "1" ] && [ "$saw_objects" = "1" ] && [ "$saw_all" = "1" ]; then
  "$FRRS_REAL_GIT" "$@"
  status=$?
  if [ "$status" -ne 0 ]; then
    exit "$status"
  fi
  i=0
  while [ "$i" -lt 5000 ]; do
    printf '000000000000000000000000000000000000%04d extra/%05d.txt\n' "$i" "$i"
    i=$((i + 1))
  done
  exit 0
fi
exec "$FRRS_REAL_GIT" "$@"
"#;
    std::fs::write(&git_path, script).expect("write fake git");
    let mut perms = std::fs::metadata(&git_path)
        .expect("fake git metadata")
        .permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&git_path, perms).expect("make fake git executable");
    bin_dir
}

#[test]
fn analyze_mode_produces_human_report() {
    let repo = init_repo();
    let opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true, // Use --force to bypass sanity checks for unit tests
        ..Default::default()
    };
    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");
    assert!(
        report.metrics.refs_total >= 1,
        "expected refs to be counted"
    );
    assert!(
        !report.warnings.is_empty(),
        "expected at least one informational warning"
    );
    fr::analysis::run(&opts).expect("analyze mode should render without error");
}

#[test]
fn analyze_mode_emits_json() {
    let repo = init_repo();
    let mut opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true, // Use --force to bypass sanity checks for unit tests
        ..Default::default()
    };
    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");
    let json = serde_json::to_string(&report).expect("serialize report");
    let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert!(
        v.get("metrics").is_some(),
        "metrics missing in json: {}",
        json
    );
    assert!(
        v.get("warnings").is_some(),
        "warnings missing in json: {}",
        json
    );
    opts.analyze.json = true;
    fr::analysis::run(&opts).expect("json analyze run should succeed");
}

#[test]
fn analyze_mode_limits_top_entries_and_populates_paths() {
    let repo = init_repo();
    // create blobs of various sizes so the top list can be truncated
    for i in 0..5 {
        let size = (i + 1) * 1024;
        let contents = "x".repeat(size);
        write_file(&repo, &format!("data/blob{}.bin", i), &contents);
    }
    // create multiple duplicate blobs with distinct contents to ensure truncation
    for (idx, paths) in [
        ("A", vec!["dups/a1.txt", "dups/a2.txt", "dups/a3.txt"]),
        ("B", vec!["dups/b1.txt", "dups/b2.txt"]),
        ("C", vec!["dups/c1.txt", "dups/c2.txt"]),
    ] {
        let payload = format!("duplicate payload {}", idx);
        for path in paths {
            write_file(&repo, path, &payload);
        }
    }
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "populate blobs"]).0, 0);

    let mut opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true, // Use --force to bypass sanity checks for unit tests
        ..Default::default()
    };
    opts.analyze.top = 2;
    opts.analyze.thresholds.warn_blob_bytes = 1500;
    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");

    assert!(
        report.metrics.largest_blobs.len() <= opts.analyze.top,
        "largest blobs exceeded top limit"
    );
    assert!(
        report.metrics.blobs_over_threshold.len() <= opts.analyze.top,
        "threshold hits exceeded top limit"
    );
    assert!(
        report
            .metrics
            .largest_blobs
            .iter()
            .all(|b| b.path.is_some()),
        "expected sample paths for top blobs"
    );
    assert!(
        report
            .metrics
            .blobs_over_threshold
            .iter()
            .all(|b| b.path.is_some()),
        "expected sample paths for threshold hits"
    );
}

#[test]
fn analyze_mode_warns_on_commit_thresholds() {
    let repo = init_repo();
    // oversized commit message that should exceed the configured threshold
    write_file(&repo, "logs.txt", &"L".repeat(64));
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", &"M".repeat(64)]).0, 0);
    let (_, long_oid, _) = run_git(&repo, &["rev-parse", "HEAD"]);
    let long_oid = long_oid.trim().to_string();
    // determine the name of the default branch (e.g. master or main)
    // prefer symbolic-ref, but fall back to rev-parse if needed
    let (_, base_branch, _) = run_git(&repo, &["symbolic-ref", "--short", "HEAD"]);
    let mut base_branch = base_branch.trim().to_string();
    if base_branch.is_empty() || base_branch == "HEAD" {
        let (_, alt, _) = run_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"]);
        base_branch = alt.trim().to_string();
    }

    // create a feature branch and diverging history to produce a merge commit
    assert_eq!(run_git(&repo, &["checkout", "-b", "feature"]).0, 0);
    write_file(&repo, "feature.txt", "feature work");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "feature commit"]).0, 0);

    // return to the original default branch, regardless of its name
    assert_eq!(run_git(&repo, &["checkout", &base_branch]).0, 0);
    write_file(&repo, "master.txt", "master work");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "master commit"]).0, 0);

    let merge_msg = "Merge branch 'feature' with an explanation that exceeds the warn threshold";
    assert_eq!(run_git(&repo, &["merge", "feature", "-m", merge_msg]).0, 0);

    let mut opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true, // Use --force to bypass sanity checks for unit tests
        ..Default::default()
    };
    opts.analyze.thresholds.warn_commit_msg_bytes = 32;
    opts.analyze.thresholds.warn_max_parents = 1;

    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");

    assert!(
        report.metrics.max_commit_parents > 1,
        "expected merge commit to exceed parent threshold"
    );
    assert!(
        report
            .metrics
            .oversized_commit_messages
            .iter()
            .any(|m| m.oid.trim() == long_oid),
        "expected long commit message to be recorded"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.message.contains(&long_oid)),
        "expected warning mentioning oversized commit message"
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|w| w.message.contains("parents")),
        "expected warning about excessive commit parents"
    );
}

/// Raw stdout bytes from a git command (so byte lengths and embedded NULs are
/// preserved exactly, unlike the lossy String helper).
fn git_raw_stdout(repo: &Path, args: &[&str]) -> Vec<u8> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

/// Reproduce the legacy oversized-commit-message measurement exactly:
/// `git log --all --pretty=%H%x00%B%x00`, split on NUL, trim the oid, and keep
/// messages whose byte length is >= threshold. Returns a sorted (oid8, len) set.
fn expected_oversized(repo: &Path, threshold: usize) -> Vec<(String, usize)> {
    let raw = git_raw_stdout(repo, &["log", "--all", "--pretty=%H%x00%B%x00"]);
    let mut parts = raw.split(|&b| b == 0);
    let mut out = Vec::new();
    loop {
        let Some(oid_bytes) = parts.next() else { break };
        let oid = String::from_utf8_lossy(oid_bytes).trim().to_string();
        if oid.is_empty() {
            break;
        }
        let Some(msg_bytes) = parts.next() else { break };
        if msg_bytes.len() >= threshold {
            out.push((oid[..oid.len().min(8)].to_string(), msg_bytes.len()));
        }
    }
    out.sort();
    out
}

fn expected_max_parents(repo: &Path) -> usize {
    let raw = git_raw_stdout(repo, &["rev-list", "--parents", "--all"]);
    String::from_utf8_lossy(&raw)
        .lines()
        .map(|line| line.split_whitespace().count().saturating_sub(1))
        .max()
        .unwrap_or(0)
}

#[test]
fn analyze_history_metrics_match_legacy_git_measurements() {
    // p2-analyze-batch-merge parity gate: the single cat-file --batch pass that
    // derives max parents and oversized commit messages must match the legacy
    // `git rev-list --parents --all` / `git log --all --pretty=%B` results
    // byte-for-byte across tricky commit shapes.
    let repo = init_repo();
    let base_branch = {
        let (_, b, _) = run_git(&repo, &["symbolic-ref", "--short", "HEAD"]);
        let b = b.trim().to_string();
        if b.is_empty() || b == "HEAD" {
            "master".to_string()
        } else {
            b
        }
    };

    // multi-paragraph + trailing newline
    write_file(&repo, "a.txt", "a\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(
        run_git(
            &repo,
            &[
                "commit",
                "-m",
                "subject line",
                "-m",
                "second paragraph body"
            ]
        )
        .0,
        0
    );

    // non-ASCII message
    write_file(&repo, "b.txt", "b\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(
        run_git(
            &repo,
            &[
                "commit",
                "-m",
                "日本語のコミットメッセージ with mixed ascii"
            ]
        )
        .0,
        0
    );

    // diverging branch + merge commit (2 parents)
    assert_eq!(run_git(&repo, &["checkout", "-b", "feature"]).0, 0);
    write_file(&repo, "c.txt", "c\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "feature work"]).0, 0);
    assert_eq!(run_git(&repo, &["checkout", &base_branch]).0, 0);
    write_file(&repo, "d.txt", "d\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "mainline work"]).0, 0);
    assert_eq!(
        run_git(
            &repo,
            &[
                "merge",
                "feature",
                "--no-ff",
                "-m",
                "Merge feature with a deliberately long explanation paragraph",
            ],
        )
        .0,
        0
    );

    // annotated tag => a tag object in the reachable set (must be ignored by the
    // commit/tree pass, not misparsed as a commit).
    assert_eq!(
        run_git(&repo, &["tag", "-a", "v1", "-m", "annotated tag message"]).0,
        0
    );

    for threshold in [1usize, 20, 41] {
        let mut opts = fr::Options {
            source: repo.clone(),
            target: repo.clone(),
            mode: fr::Mode::Analyze,
            force: true,
            ..Default::default()
        };
        opts.analyze.thresholds.warn_commit_msg_bytes = threshold;
        let report = fr::analysis::generate_report(&opts).expect("generate analysis report");

        assert_eq!(
            report.metrics.max_commit_parents,
            expected_max_parents(&repo),
            "max_commit_parents must match git rev-list --parents"
        );

        let mut got: Vec<(String, usize)> = report
            .metrics
            .oversized_commit_messages
            .iter()
            .map(|m| (m.oid[..m.oid.len().min(8)].to_string(), m.length))
            .collect();
        got.sort();
        assert_eq!(
            got,
            expected_oversized(&repo, threshold),
            "oversized commit messages must match legacy %B measurement at threshold {threshold}"
        );
    }
}

#[test]
fn analyze_subflags_imply_read_only_analyze_mode() {
    // `--analyze-top` is an analyze-only option. Used without an explicit
    // `--analyze`, it must still run a read-only analysis (and therefore never
    // fall through to the write/rewrite path that is gated by already-ran),
    // since narrowing the top-N display lists is meaningless for a rewrite.
    let repo = init_repo();
    write_file(&repo, "src/a.txt", "a\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(
        run_git(&repo, &["commit", "-m", "seed analyze subflag"]).0,
        0
    );

    let output = cli_command()
        .arg("--analyze-top")
        .arg("3")
        .arg("--source")
        .arg(repo.to_string_lossy().as_ref())
        .arg("--target")
        .arg(repo.to_string_lossy().as_ref())
        .output()
        .expect("run filter-repo-rs --analyze-top");

    assert!(
        output.status.success(),
        "--analyze-top alone should succeed as analysis: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Repository summary"),
        "expected a read-only analysis report, got: {}",
        stdout
    );
}

#[test]
fn analyze_json_stdout_is_valid_json_without_progress_prefix() {
    let repo = init_repo();
    write_file(&repo, "src/a.txt", "a\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "seed analyze json"]).0, 0);

    let output = cli_command()
        .arg("--analyze")
        .arg("--analyze-json")
        .arg("--source")
        .arg(repo.to_string_lossy().as_ref())
        .arg("--target")
        .arg(repo.to_string_lossy().as_ref())
        .output()
        .expect("run filter-repo-rs analyze json");

    assert!(
        output.status.success(),
        "analyze json should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout should contain only valid json");
    assert!(
        parsed.get("metrics").is_some(),
        "metrics missing from json: {}",
        stdout
    );
    assert!(
        parsed.get("warnings").is_some(),
        "warnings missing from json: {}",
        stdout
    );
}

#[cfg(unix)]
#[test]
fn analyze_drains_rev_list_stdout_after_blob_paths_are_known() {
    let repo = init_repo();
    write_file(&repo, "src/a.txt", "a\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "seed analyze drain"]).0, 0);

    let fake_git_dir = write_rev_list_stdout_flood_git(&repo);
    let mut command = cli_command();
    command
        .arg("--analyze")
        .arg("--analyze-json")
        .arg("--source")
        .arg(repo.to_string_lossy().as_ref())
        .arg("--target")
        .arg(repo.to_string_lossy().as_ref())
        .env("FRRS_REAL_GIT", find_git_on_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepend_path(&mut command, &fake_git_dir);

    let child = command.spawn().expect("spawn analyze mode");
    let output = wait_with_timeout(child, Duration::from_secs(10));

    assert!(
        output.status.success(),
        "analyze should drain remaining rev-list stdout\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str::<serde_json::Value>(&stdout).expect("stdout should remain valid json");
}

#[test]
fn analyze_report_does_not_include_placeholder_blob_ids() {
    let repo = init_repo();
    write_file(&repo, "src/a.txt", "a\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(
        run_git(&repo, &["commit", "-m", "seed analyze placeholder"]).0,
        0
    );

    let opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true,
        ..Default::default()
    };

    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");
    assert!(
        report
            .metrics
            .largest_blobs
            .iter()
            .all(|b| !b.oid.starts_with("placeholder_")),
        "largest_blobs should not contain placeholder oids: {:?}",
        report.metrics.largest_blobs
    );
}

#[test]
fn analyze_report_excludes_unreachable_blobs_from_top_lists() {
    let repo = init_repo();
    write_file(&repo, "src/reachable.txt", "reachable\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "add reachable blob"]).0, 0);

    write_file(&repo, "tmp-unreachable.bin", &"U".repeat(128 * 1024));
    let (status, stdout, stderr) = run_git(&repo, &["hash-object", "-w", "tmp-unreachable.bin"]);
    assert_eq!(status, 0, "hash-object should succeed: {}", stderr);
    let unreachable_oid = stdout.trim().to_string();
    assert!(
        !unreachable_oid.is_empty(),
        "expected hash-object to return an oid"
    );

    let (_, rev_list, _) = run_git(&repo, &["rev-list", "--objects", "--all"]);
    assert!(
        !rev_list
            .lines()
            .any(|line| line.starts_with(unreachable_oid.as_str())),
        "unreachable blob should not appear in reachable object listing"
    );

    let mut opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true,
        ..Default::default()
    };
    opts.analyze.top = 1;
    opts.analyze.thresholds.warn_blob_bytes = 1;

    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");
    let reachable_object_count = rev_list.lines().count() as u64;
    assert_eq!(
        report.metrics.total_objects, reachable_object_count,
        "headline total_objects should use the reachable object universe"
    );
    assert!(
        report
            .metrics
            .largest_blobs
            .iter()
            .all(|blob| blob.oid != unreachable_oid),
        "largest_blobs should not include unreachable oid {}: {:?}",
        unreachable_oid,
        report.metrics.largest_blobs
    );
    assert!(
        report
            .metrics
            .blobs_over_threshold
            .iter()
            .all(|blob| blob.oid != unreachable_oid),
        "blobs_over_threshold should not include unreachable oid {}: {:?}",
        unreachable_oid,
        report.metrics.blobs_over_threshold
    );
}

#[test]
fn analyze_report_populates_reachable_tree_tag_and_checkout_metrics() {
    let repo = init_repo();
    write_file(&repo, "src/deep/module/file.txt", "reachable payload\n");
    write_file(
        &repo,
        "src/deep/module/another.txt",
        "another reachable payload\n",
    );
    write_file(&repo, "assets/images/logo.txt", "logo payload\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(
        run_git(&repo, &["commit", "-m", "seed reachable metrics"]).0,
        0
    );
    assert_eq!(
        run_git(&repo, &["tag", "-a", "v1", "-m", "annotated tag"]).0,
        0
    );

    let opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true,
        ..Default::default()
    };

    let report = fr::analysis::generate_report(&opts).expect("generate analysis report");

    assert!(
        report
            .metrics
            .object_types
            .get("tree")
            .copied()
            .unwrap_or(0)
            > 0,
        "tree count should reflect reachable trees: {:?}",
        report.metrics.object_types
    );
    assert!(
        report.metrics.object_types.get("tag").copied().unwrap_or(0) > 0,
        "tag count should reflect reachable annotated tags: {:?}",
        report.metrics.object_types
    );
    assert!(
        report.metrics.tree_total_size_bytes > 0,
        "tree_total_size_bytes should be populated"
    );
    assert!(
        !report.metrics.largest_trees.is_empty(),
        "largest_trees should be populated"
    );
    assert!(
        report.metrics.directory_hotspots.is_some(),
        "directory_hotspots should be populated from HEAD"
    );
    assert!(
        report.metrics.longest_path.is_some(),
        "longest_path should be populated from HEAD"
    );
}

#[test]
fn analyze_mode_with_write_report_creates_report_file() {
    let repo = init_repo();
    write_file(&repo, "src/a.txt", "hello world\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "add file"]).0, 0);

    let mut opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true,
        ..Default::default()
    };
    opts.write_report = true;

    fr::analysis::run(&opts).expect("analyze mode with write_report should succeed");

    let report_path = repo.join(".git").join("filter-repo").join("report.txt");
    assert!(report_path.exists(), "report.txt should be created");

    let content = std::fs::read_to_string(&report_path).expect("read report.txt");
    assert!(
        content.contains("Repository analysis"),
        "report should contain header"
    );
    assert!(
        content.contains("Reachable objects"),
        "report should contain reachable metrics"
    );
    assert!(
        content.contains("Object types"),
        "report should contain per-type metrics"
    );
    assert!(
        content.contains("Highest concern"),
        "report should include concern levels"
    );
}

#[test]
fn analyze_mode_with_write_report_json_creates_json_report_file() {
    let repo = init_repo();
    write_file(&repo, "src/b.txt", "hello again\n");
    assert_eq!(run_git(&repo, &["add", "."]).0, 0);
    assert_eq!(run_git(&repo, &["commit", "-m", "add another file"]).0, 0);

    let opts = fr::Options {
        source: repo.clone(),
        target: repo.clone(),
        mode: fr::Mode::Analyze,
        force: true,
        write_report_json: true,
        ..Default::default()
    };

    fr::analysis::run(&opts).expect("analyze mode with write_report_json should succeed");

    let json_path = repo.join(".git").join("filter-repo").join("report.json");
    assert!(json_path.exists(), "report.json should be created");

    let content = std::fs::read_to_string(&json_path).expect("read report.json");
    let parsed: serde_json::Value = serde_json::from_str(&content).expect("valid json");
    assert!(
        parsed.get("metrics").is_some(),
        "json should contain metrics"
    );
    assert!(
        parsed.get("warnings").is_some(),
        "json should contain warnings"
    );
}
