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
            .expect("poll filter-repo-rs child process")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("collect filter-repo-rs child output");
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out filter-repo-rs output");
            panic!(
                "filter-repo-rs timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
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
fn write_batch_all_objects_stderr_flood_git(repo: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;

    let bin_dir = repo.join("fake-git-bin");
    std::fs::create_dir_all(&bin_dir).expect("create fake git dir");
    let git_path = bin_dir.join("git");
    let script = r#"#!/bin/sh
saw_cat_file=0
saw_batch_all=0
for arg in "$@"; do
  if [ "$arg" = "cat-file" ]; then
    saw_cat_file=1
  fi
  if [ "$arg" = "--batch-all-objects" ]; then
    saw_batch_all=1
  fi
done
if [ "$saw_cat_file" = "1" ] && [ "$saw_batch_all" = "1" ]; then
  i=0
  while [ "$i" -lt 5000 ]; do
    printf 'batch-all stderr flood %05d 0123456789abcdef0123456789abcdef\n' "$i" >&2
    i=$((i + 1))
  done
  exit 1
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
fn max_blob_size_edge_cases() {
    let repo = init_repo();
    write_file(&repo, "empty.txt", "");
    write_file(&repo, "tiny.txt", "A");
    let threshold_content = vec![b'X'; 100];
    std::fs::write(repo.join("threshold.bin"), &threshold_content).unwrap();
    let over_content = vec![b'Y'; 101];
    std::fs::write(repo.join("over.bin"), &over_content).unwrap();
    let large_content = vec![b'Z'; 10000];
    std::fs::write(repo.join("large.bin"), &large_content).unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add edge case files"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(100);
    });
    let (_c2, tree, _e2) = run_git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("empty.txt"));
    assert!(tree.contains("tiny.txt"));
    assert!(tree.contains("threshold.bin"));
    assert!(!tree.contains("over.bin"));
    assert!(!tree.contains("large.bin"));
}

#[test]
fn max_blob_size_with_path_filtering() {
    let repo = init_repo();
    std::fs::create_dir_all(repo.join("keep")).unwrap();
    std::fs::create_dir_all(repo.join("drop")).unwrap();
    let large_content = vec![b'A'; 2000];
    std::fs::write(repo.join("keep/large.bin"), &large_content).unwrap();
    std::fs::write(repo.join("drop/large.bin"), &large_content).unwrap();
    std::fs::write(repo.join("keep/small.txt"), "small content").unwrap();
    std::fs::write(repo.join("drop/small.txt"), "small content").unwrap();
    run_git(&repo, &["add", "."]);
    run_git(
        &repo,
        &["commit", "-m", "add files in different directories"],
    );
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(1000);
        o.paths.push(b"keep/".to_vec());
    });
    let (_c2, tree, _e2) = run_git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("keep/small.txt"));
    assert!(!tree.contains("drop/"));
    assert!(!tree.contains("keep/large.bin"));
}

#[test]
fn max_blob_size_with_strip_blobs_by_sha() {
    let repo = init_repo();
    let content1 = "test content 1";
    let content2 = "test content 2";
    std::fs::write(repo.join("file1.txt"), content1).unwrap();
    std::fs::write(repo.join("file2.txt"), content2).unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add test files"]);
    let (_c1, sha1_output, _e1) = run_git(&repo, &["hash-object", "file1.txt"]);
    let (_c2, sha2_output, _e2) = run_git(&repo, &["hash-object", "file2.txt"]);
    let sha1 = sha1_output.trim();
    let sha2 = sha2_output.trim();
    let sha_list_content = format!("{}\n{}", sha1, sha2);
    std::fs::write(repo.join("sha_list.txt"), &sha_list_content).unwrap();
    run_git(&repo, &["add", "sha_list.txt"]);
    run_git(&repo, &["commit", "-m", "add sha list"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(1000);
        o.strip_blobs_with_ids = Some(repo.join("sha_list.txt"));
    });
    let (_c2, tree, _e2) = run_git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(!tree.contains("file1.txt"));
    assert!(!tree.contains("file2.txt"));
}

#[test]
fn max_blob_size_empty_repository() {
    let repo = init_repo();
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(1000);
    });
    let (_c2, tree, _e2) = run_git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("README.md"));
}

#[test]
fn max_blob_size_mixed_blob_types() {
    let repo = init_repo();
    write_file(&repo, "text.txt", &"a".repeat(1500));
    std::fs::write(repo.join("binary.bin"), vec![0u8; 1500]).unwrap();
    write_file(&repo, "utf8.txt", &"浣犲ソ".repeat(500));
    std::fs::write(repo.join("zeroes.bin"), vec![0u8; 500]).unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add mixed content types"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(1000);
    });
    let (_c2, tree, _e2) = run_git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert!(tree.contains("zeroes.bin"));
    assert!(!tree.contains("text.txt"));
    assert!(!tree.contains("binary.bin"));
    assert!(!tree.contains("utf8.txt"));
}

#[test]
fn max_blob_size_batch_optimization_verification() {
    let repo = init_repo();
    for i in 0..100 {
        let content = format!("file content {}", i);
        write_file(&repo, &format!("file{}.txt", i), &content);
    }
    write_file(&repo, "large1.bin", &"a".repeat(2000));
    write_file(&repo, "large2.bin", &"b".repeat(3000));
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add many files for batch test"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(1500);
    });
    let (_c2, tree, _e2) = run_git(&repo, &["ls-tree", "-r", "--name-only", "HEAD"]);
    for i in 0..100 {
        assert!(tree.contains(&format!("file{}.txt", i)));
    }
    assert!(!tree.contains("large1.bin"));
    assert!(!tree.contains("large2.bin"));
}

#[cfg(unix)]
#[test]
fn max_blob_size_prefetch_drains_cat_file_stderr_before_fallback() {
    let repo = init_repo();
    write_file(&repo, "small.txt", "small content\n");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add small file"]);

    let fake_git_dir = write_batch_all_objects_stderr_flood_git(&repo);
    let mut command = cli_command();
    command
        .arg("--max-blob-size")
        .arg("1024")
        .arg("--dry-run")
        .arg("--force")
        .current_dir(&repo)
        .env("FRRS_REAL_GIT", find_git_on_path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepend_path(&mut command, &fake_git_dir);

    let child = command.spawn().expect("spawn filter-repo-rs");
    let output = wait_with_timeout(child, Duration::from_secs(10));

    assert!(
        output.status.success(),
        "prefetch failure should fall back without hanging\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn max_blob_size_fallback_behavior() {
    let repo = tempfile::TempDir::new().unwrap();
    let repo_path = repo.path();
    let (c, _o, e) = run_git(repo_path, &["init"]);
    assert_eq!(c, 0, "git init failed: {}", e);
    run_git(repo_path, &["config", "user.name", "A U Thor"]);
    run_git(repo_path, &["config", "user.email", "a.u.thor@example.com"]);
    write_file(repo_path, "test.txt", "hello");
    run_git(repo_path, &["add", "."]);
    run_git(repo_path, &["commit", "-m", "add test file"]);
    run_tool_expect_success(repo_path, |o| {
        o.max_blob_size = Some(1000);
    });
    let (_c2, tree, _e2) = run_git(
        repo_path,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(tree.contains("test.txt"));
}

#[test]
fn max_blob_size_no_git_objects() {
    let repo = tempfile::TempDir::new().unwrap();
    let repo_path = repo.path();
    let (c, _o, e) = run_git(repo_path, &["init"]);
    assert_eq!(c, 0, "git init failed: {}", e);
    run_git(repo_path, &["config", "user.name", "test"]);
    run_git(repo_path, &["config", "user.email", "test@example.com"]);
    run_git(
        repo_path,
        &["commit", "--allow-empty", "-q", "-m", "empty commit 1"],
    );
    run_git(
        repo_path,
        &["commit", "--allow-empty", "-q", "-m", "empty commit 2"],
    );
    run_tool_expect_success(repo_path, |o| {
        o.max_blob_size = Some(1000);
    });
    let (_c2, tree, _e2) = run_git(
        repo_path,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(tree.is_empty());
}

#[test]
fn max_blob_size_corrupted_git_output() {
    let repo = init_repo();
    write_file(&repo, "test.txt", "test content");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add test file"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(5);
    });
    let (_c2, tree, _e2) = run_git(
        &repo,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(!tree.contains("test.txt"));
}

#[test]
fn max_blob_size_extreme_threshold_values() {
    let repo = init_repo();
    write_file(&repo, "tiny.txt", "x");
    write_file(&repo, "small.txt", &"x".repeat(100));
    write_file(&repo, "medium.txt", &"x".repeat(10000));
    write_file(&repo, "large.txt", &"x".repeat(100000));
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add various sized files"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(1);
    });
    let (_c2, tree1, _e2) = run_git(
        &repo,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(tree1.contains("tiny.txt"));
    assert!(!tree1.contains("small.txt"));
    assert!(!tree1.contains("medium.txt"));
    assert!(!tree1.contains("large.txt"));
    let repo2 = init_repo();
    write_file(&repo2, "tiny.txt", "x");
    write_file(&repo2, "small.txt", &"x".repeat(100));
    write_file(&repo2, "medium.txt", &"x".repeat(10000));
    write_file(&repo2, "large.txt", &"x".repeat(100000));
    run_git(&repo2, &["add", "."]);
    run_git(&repo2, &["commit", "-m", "add various sized files"]);
    run_tool_expect_success(&repo2, |o| {
        o.max_blob_size = Some(1000000);
    });
    let (_c2, tree2, _e2) = run_git(
        &repo2,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(tree2.contains("tiny.txt"));
    assert!(tree2.contains("small.txt"));
    assert!(tree2.contains("medium.txt"));
    assert!(tree2.contains("large.txt"));
}

#[test]
fn max_blob_size_precise_threshold_handling() {
    let repo = init_repo();
    std::fs::write(repo.join("exactly_100_bytes.txt"), b"a".repeat(100)).unwrap();
    std::fs::write(repo.join("exactly_101_bytes.txt"), b"b".repeat(101)).unwrap();
    std::fs::write(repo.join("just_under_100.txt"), b"c".repeat(99)).unwrap();
    std::fs::write(repo.join("just_over_100.txt"), b"d".repeat(101)).unwrap();
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-m", "add boundary test files"]);
    run_tool_expect_success(&repo, |o| {
        o.max_blob_size = Some(100);
    });
    let (_c2, tree, _e2) = run_git(
        &repo,
        &[
            "-c",
            "core.quotepath=false",
            "ls-tree",
            "-r",
            "--name-only",
            "HEAD",
        ],
    );
    assert!(tree.contains("exactly_100_bytes.txt"));
    assert!(tree.contains("just_under_100.txt"));
    assert!(!tree.contains("exactly_101_bytes.txt"));
    assert!(!tree.contains("just_over_100.txt"));
}
