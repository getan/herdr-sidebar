//! The ✨ commit-message suggestion: ask the local `claude` CLI to summarize
//! the pending diff (like VS Code's sparkle button), falling back to a
//! filename-based heuristic when the CLI is missing, slow, or fails. Runs on a
//! background thread so the TUI stays responsive; the app polls the returned
//! channel from its refresh tick.

use std::io::Write;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

/// Cap the diff sent to the model — huge diffs only slow generation down, and
/// the file list already names everything that changed.
const MAX_DIFF_BYTES: usize = 16 * 1024;

/// How long to wait for `claude` before killing it and falling back.
const TIMEOUT: Duration = Duration::from_secs(60);

const PROMPT: &str = "Write a git commit message for the diff on stdin: one imperative \
                      subject line under 72 characters, no quotes, no trailing period. \
                      Reply with ONLY the message line.";

/// Spawn generation for `diff`/`files`; the result arrives on the channel.
/// Always yields exactly one message (the fallback is used on any failure).
pub fn spawn(diff: String, files: Vec<String>) -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        let message = generate(&diff, &files);
        let _ = tx.send(message);
    });
    rx
}

fn generate(diff: &str, files: &[String]) -> String {
    match ask_claude(diff) {
        Some(message) => message,
        None => fallback(files),
    }
}

fn suggest_program() -> String {
    std::env::var("HERDR_SIDEBAR_SUGGEST_PROGRAM")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "codex".to_string())
}

fn suggest_model(program: &str) -> String {
    if let Ok(m) = std::env::var("HERDR_SIDEBAR_SUGGEST_MODEL") {
        if !m.trim().is_empty() {
            return m;
        }
    }
    if program.contains("codex") {
        "gpt-5-mini".to_string()
    } else {
        "haiku".to_string()
    }
}

/// One subject line from the configured CLI, or `None` on any failure.
/// Default is `codex` (via `codex exec`), fallback to `claude` shim is
/// kept for backwards compat. Program/model are overridable via
/// `HERDR_SIDEBAR_SUGGEST_PROGRAM` / `HERDR_SIDEBAR_SUGGEST_MODEL`.
fn ask_claude(diff: &str) -> Option<String> {
    let mut input = String::with_capacity(diff.len().min(MAX_DIFF_BYTES));
    for c in diff.chars() {
        if input.len() + c.len_utf8() > MAX_DIFF_BYTES {
            input.push_str("\n[diff truncated]");
            break;
        }
        input.push(c);
    }

    let program = suggest_program();
    let model = suggest_model(&program);
    let is_codex = program.contains("codex");

    #[cfg(windows)]
    let claude_candidates = ["claude", "claude.cmd"];
    #[cfg(not(windows))]
    let claude_candidates: &[&str] = &["claude"];

    let candidates: Vec<String> = if is_codex {
        vec![program.clone()]
    } else if program != "claude" {
        vec![program.clone()]
    } else {
        claude_candidates.iter().map(|s| s.to_string()).collect()
    };

    for prog in candidates {
        let spawn_res = if prog.contains("codex") {
            let prompt = format!("{PROMPT}\n\nDiff:\n{input}");
            std::process::Command::new(&prog)
                .args(["exec", "-m", &model, "--color", "never", &prompt])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        } else {
            std::process::Command::new(&prog)
                .args(["-p", "--model", &model, "--strict-mcp-config", PROMPT])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
        };
        let Ok(mut child) = spawn_res else { continue };
        if !prog.contains("codex") {
            if let Some(stdin) = child.stdin.take() {
                let mut stdin = stdin;
                if stdin.write_all(input.as_bytes()).is_err() {
                    let _ = child.kill();
                    continue;
                }
            }
        }
        if let Some(msg) = wait_with_timeout(child) {
            return Some(msg);
        }
        if is_codex {
            continue;
        }
    }
    // codex failed → try claude as fallback
    if is_codex {
        for prog in claude_candidates {
            let Ok(mut child) = std::process::Command::new(prog)
                .args(["-p", "--model", "haiku", "--strict-mcp-config", PROMPT])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn() else { continue };
            if let Some(stdin) = child.stdin.take() {
                let mut stdin = stdin;
                if stdin.write_all(input.as_bytes()).is_err() {
                    let _ = child.kill();
                    continue;
                }
            }
            if let Some(msg) = wait_with_timeout(child) {
                return Some(msg);
            }
        }
    }
    None
}

/// Wait for the child up to [`TIMEOUT`]; kill it and give up on overrun.
/// Reading stdout AFTER exit is safe here because `-p` output is one short
/// line, far below any pipe buffer.
fn wait_with_timeout(mut child: std::process::Child) -> Option<String> {
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if start.elapsed() > TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return None,
        }
    }
    let mut out = String::new();
    use std::io::Read;
    child.stdout.take()?.read_to_string(&mut out).ok()?;
    clean_reply(&out)
}

/// The reply line, stripped of the quoting/fencing chat models sometimes add
/// despite instructions; `None` when nothing usable came back. Startup log
/// noise (MCP warnings and the like) can precede the reply on stdout, so this
/// takes the LAST usable line and drops warning-looking lines outright.
fn clean_reply(raw: &str) -> Option<String> {
    let line = raw.lines().map(str::trim).rfind(|l| {
        let lower = l.to_lowercase();
        !l.is_empty()
            && !l.starts_with("```")
            && !lower.contains("warn")
            && !lower.contains("error")
    })?;
    let line = line.trim_matches(['"', '\'', '`']).trim_end_matches('.').trim();
    (!line.is_empty()).then(|| line.to_string())
}

/// Filename-based fallback: good enough to save retyping, honest about scope.
fn fallback(files: &[String]) -> String {
    let name = |path: &String| {
        path.rsplit('/').next().unwrap_or(path).to_string()
    };
    match files {
        [] => "Update".to_string(),
        [only] => format!("Update {}", name(only)),
        [first, rest @ ..] => format!("Update {} and {} more", name(first), rest.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reply_cleanup_strips_quotes_fences_and_periods() {
        assert_eq!(clean_reply("Add sidebar merge\n"), Some("Add sidebar merge".into()));
        assert_eq!(clean_reply("\"Fix the thing.\""), Some("Fix the thing".into()));
        assert_eq!(
            clean_reply("```\nRefactor launch flow\n```"),
            Some("Refactor launch flow".into())
        );
        assert_eq!(clean_reply("   \n\n"), None);
        // Log noise before (or instead of) the reply must never win.
        assert_eq!(
            clean_reply("RendererWarning resource UID duplicate\nAdd auth docs\n"),
            Some("Add auth docs".into())
        );
        assert_eq!(clean_reply("[WARN] something\nERROR: nope\n"), None);
    }

    #[test]
    fn fallback_names_the_files() {
        assert_eq!(fallback(&[]), "Update");
        assert_eq!(fallback(&["src/app.rs".into()]), "Update app.rs");
        assert_eq!(
            fallback(&["src/app.rs".into(), "b".into(), "c".into()]),
            "Update app.rs and 2 more"
        );
    }
}
