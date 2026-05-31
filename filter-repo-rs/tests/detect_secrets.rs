mod common;
use common::fake_secrets;
use common::*;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Output {
    let start = Instant::now();
    loop {
        if child
            .try_wait()
            .expect("poll detect-secrets child process")
            .is_some()
        {
            return child
                .wait_with_output()
                .expect("collect detect-secrets child output");
        }

        if start.elapsed() >= timeout {
            let _ = child.kill();
            let output = child
                .wait_with_output()
                .expect("collect timed-out detect-secrets output");
            panic!(
                "detect-secrets timed out after {:?}\nstdout:\n{}\nstderr:\n{}",
                timeout,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }

        thread::sleep(Duration::from_millis(50));
    }
}

fn import_many_plain_text_blobs(repo: &Path, count: usize) {
    let branch = current_branch(repo);
    let (code, parent, stderr) = run_git(repo, &["rev-parse", "HEAD"]);
    assert_eq!(code, 0, "git rev-parse HEAD failed: {stderr}");
    let parent = parent.trim();
    let mut stream = Vec::new();
    for idx in 0..count {
        let contents = format!("plain historical text object {idx:05}\n");
        writeln!(&mut stream, "blob").expect("write fast-import blob command");
        writeln!(&mut stream, "mark :{}", idx + 1).expect("write fast-import mark");
        writeln!(&mut stream, "data {}", contents.len()).expect("write fast-import data size");
        stream.extend_from_slice(contents.as_bytes());
    }

    let message = "add many plain text blobs\n";
    writeln!(&mut stream, "commit refs/heads/{branch}").expect("write fast-import commit");
    writeln!(
        &mut stream,
        "committer A U Thor <a.u.thor@example.com> 0 +0000"
    )
    .expect("write fast-import committer");
    writeln!(&mut stream, "data {}", message.len()).expect("write fast-import message size");
    stream.extend_from_slice(message.as_bytes());
    writeln!(&mut stream, "from {parent}").expect("write fast-import parent");
    for idx in 0..count {
        writeln!(&mut stream, "M 100644 :{} objects/{idx:05}.txt", idx + 1)
            .expect("write fast-import filemodify");
    }

    let mut child = std::process::Command::new("git")
        .current_dir(repo)
        .arg("fast-import")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn git fast-import");
    let write_result = child
        .stdin
        .take()
        .expect("open git fast-import stdin")
        .write_all(&stream);
    let output = child.wait_with_output().expect("run git fast-import");
    assert!(
        write_result.is_ok(),
        "write git fast-import stream failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        write_result.err(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "git fast-import failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn detect_secrets_dry_run_writes_draft_file() {
    let repo = init_repo();
    let aws_access_key_id = fake_secrets::aws_access_key_id();
    let super_secret = fake_secrets::super_secret_123();

    write_file(
        &repo,
        "app.env",
        &format!("AWS_ACCESS_KEY_ID={aws_access_key_id}\npassword={super_secret}\n"),
    );
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add secret-like values"]);

    let output = cli_command()
        .arg("--detect-secrets")
        .arg("--dry-run")
        .current_dir(&repo)
        .output()
        .expect("run detect-secrets mode");

    assert!(
        output.status.success(),
        "detect-secrets dry-run should succeed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("potential secret"),
        "expected detection summary in stdout: {}",
        stdout
    );

    let rules = repo.join("detected-secrets.txt");
    assert!(rules.exists(), "detected-secrets.txt should be generated");
    let content = std::fs::read_to_string(&rules).expect("read detected-secrets.txt");
    assert!(
        content.contains(&fake_secrets::removed_rule(&aws_access_key_id)),
        "draft should include aws access key rule: {}",
        content
    );
}

#[test]
fn detect_secrets_reports_zero_when_no_matches() {
    let repo = init_repo();

    let output = cli_command()
        .arg("--detect-secrets")
        .arg("--dry-run")
        .current_dir(&repo)
        .output()
        .expect("run detect-secrets mode on clean repo");

    assert!(
        output.status.success(),
        "detect-secrets on clean repo should succeed"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 potential secrets"),
        "expected zero summary in stdout: {}",
        stdout
    );
}

#[test]
fn detect_secrets_handles_many_reachable_objects_without_cat_file_pipe_deadlock() {
    let repo = init_repo();

    import_many_plain_text_blobs(&repo, 5000);

    let child = cli_command()
        .arg("--detect-secrets")
        .arg("--dry-run")
        .current_dir(&repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn detect-secrets mode");
    let output = wait_with_timeout(child, Duration::from_secs(15));

    assert!(
        output.status.success(),
        "detect-secrets should succeed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("0 potential secrets"),
        "expected zero summary in stdout: {}",
        stdout
    );
}

#[test]
fn detect_secrets_supports_custom_detect_pattern() {
    let repo = init_repo();
    let custom_secret = fake_secrets::custom_secret_2026();

    write_file(&repo, "custom.txt", &format!("internal={custom_secret}\n"));
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add custom secret token"]);

    let output = cli_command()
        .arg("--detect-secrets")
        .arg("--detect-pattern")
        .arg(r"ZZZ-CUSTOM-SECRET-[0-9]{4}")
        .arg("--dry-run")
        .current_dir(&repo)
        .output()
        .expect("run detect-secrets with custom pattern");

    assert!(
        output.status.success(),
        "detect-secrets custom pattern should succeed"
    );

    let rules = repo.join("detected-secrets.txt");
    assert!(
        rules.exists(),
        "detected-secrets.txt should be generated for custom pattern"
    );
    let content = std::fs::read_to_string(&rules).expect("read detected-secrets.txt");
    assert!(
        content.contains(&fake_secrets::removed_rule(&custom_secret)),
        "draft should include custom-pattern match: {}",
        content
    );
}

#[test]
fn detect_secrets_detects_openai_api_key() {
    let repo = init_repo();
    let openai_api_key = fake_secrets::openai_api_key();

    write_file(
        &repo,
        "config.py",
        &format!("OPENAI_API_KEY={openai_api_key}\n"),
    );
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add openai key"]);

    let output = cli_command()
        .arg("--detect-secrets")
        .arg("--dry-run")
        .current_dir(&repo)
        .output()
        .expect("run detect-secrets mode");

    assert!(output.status.success(), "detect-secrets should succeed");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("potential secret"),
        "expected detection summary in stdout: {}",
        stdout
    );

    let rules = repo.join("detected-secrets.txt");
    let content = std::fs::read_to_string(&rules).expect("read detected-secrets.txt");
    assert!(
        content.contains(&fake_secrets::removed_rule(&openai_api_key)),
        "draft should include openai api key rule: {}",
        content
    );
}

#[test]
fn detect_secrets_detects_additional_common_patterns() {
    let repo = init_repo();
    let aws_secret_access_key = fake_secrets::aws_secret_access_key();
    let google_api_key = fake_secrets::google_api_key();
    let gitlab_token = fake_secrets::gitlab_token();
    let npm_token = fake_secrets::npm_token();
    let slack_webhook = fake_secrets::slack_webhook_url();
    let stripe_secret = fake_secrets::stripe_live_secret();
    let tokens_env = format!(
        "AWS_SECRET_ACCESS_KEY={aws_secret_access_key}\n\
GOOGLE_API_KEY={google_api_key}\n\
GITLAB_TOKEN={gitlab_token}\n\
NPM_TOKEN={npm_token}\n\
SLACK_WEBHOOK={}\n\
STRIPE_SECRET={}\n",
        slack_webhook, stripe_secret
    );

    write_file(&repo, "tokens.env", &tokens_env);
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add additional tokens"]);

    let output = cli_command()
        .arg("--detect-secrets")
        .arg("--dry-run")
        .current_dir(&repo)
        .output()
        .expect("run detect-secrets mode");

    assert!(output.status.success(), "detect-secrets should succeed");

    let rules = repo.join("detected-secrets.txt");
    let content = std::fs::read_to_string(&rules).expect("read detected-secrets.txt");
    assert!(
        content.contains(&fake_secrets::removed_rule(&aws_secret_access_key)),
        "draft should include aws secret access key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&google_api_key)),
        "draft should include google api key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&gitlab_token)),
        "draft should include gitlab pat: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&npm_token)),
        "draft should include npm token: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&slack_webhook)),
        "draft should include slack webhook url: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&stripe_secret)),
        "draft should include stripe secret key: {}",
        content
    );
}

#[test]
fn detect_secrets_detects_llm_vendor_keys() {
    let repo = init_repo();
    let google_api_key = fake_secrets::google_api_key();
    let anthropic_api_key = fake_secrets::anthropic_api_key();
    let xai_api_key = fake_secrets::xai_api_key();
    let deepseek_api_key = fake_secrets::deepseek_api_key();
    let zai_api_key = fake_secrets::zai_api_key();
    let minimax_api_key = fake_secrets::minimax_api_key();
    let moonshot_api_key = fake_secrets::moonshot_api_key();
    let qwen_api_key = fake_secrets::qwen_api_key();

    write_file(
        &repo,
        "llm.env",
        &format!(
            "GEMINI_API_KEY={google_api_key}\n\
ANTHROPIC_API_KEY={anthropic_api_key}\n\
XAI_API_KEY={xai_api_key}\n\
DEEPSEEK_API_KEY={deepseek_api_key}\n\
GLM_API_KEY={zai_api_key}\n\
MINIMAX_API_KEY={minimax_api_key}\n\
KIMI_API_KEY={moonshot_api_key}\n\
QWEN_API_KEY={qwen_api_key}\n"
        ),
    );
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "-q", "-m", "add llm keys"]);

    let output = cli_command()
        .arg("--detect-secrets")
        .arg("--dry-run")
        .current_dir(&repo)
        .output()
        .expect("run detect-secrets mode");

    assert!(output.status.success(), "detect-secrets should succeed");

    let rules = repo.join("detected-secrets.txt");
    let content = std::fs::read_to_string(&rules).expect("read detected-secrets.txt");
    assert!(
        content.contains(&fake_secrets::removed_rule(&google_api_key)),
        "draft should include gemini/google api key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&anthropic_api_key)),
        "draft should include anthropic key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&xai_api_key)),
        "draft should include xai key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&deepseek_api_key)),
        "draft should include deepseek key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&zai_api_key)),
        "draft should include glm(z.ai) key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&minimax_api_key)),
        "draft should include minimax key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&moonshot_api_key)),
        "draft should include kimi key: {}",
        content
    );
    assert!(
        content.contains(&fake_secrets::removed_rule(&qwen_api_key)),
        "draft should include qwen key: {}",
        content
    );
}
