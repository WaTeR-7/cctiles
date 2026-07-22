use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// git branch changes rarely enough (and a `diff --shortstat` on a large
/// repo is slow enough) that recomputing it on every redraw would risk the
/// same kind of UI lag #54 fixed for transcript parsing - so this polls on
/// its own background thread and only ever hands the UI thread a cheap
/// cached read.
const POLL_INTERVAL: Duration = Duration::from_secs(3);

pub struct GitStatusWatcher {
    status: Arc<Mutex<Option<String>>>,
}

impl GitStatusWatcher {
    pub fn start(dir: &str) -> Self {
        let status = Arc::new(Mutex::new(git_summary(dir)));
        let status_for_thread = Arc::clone(&status);
        let dir = dir.to_string();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(POLL_INTERVAL);
                let summary = git_summary(&dir);
                if let Ok(mut current) = status_for_thread.lock() {
                    *current = summary;
                }
            }
        });
        Self { status }
    }

    /// `None` when `dir` isn't (or is no longer) a git repository.
    pub fn summary(&self) -> Option<String> {
        self.status.lock().ok().and_then(|s| s.clone())
    }
}

fn git_summary(dir: &str) -> Option<String> {
    let branch_output = Command::new("git")
        .args(["-C", dir, "rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !branch_output.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&branch_output.stdout)
        .trim()
        .to_string();
    if branch.is_empty() {
        return None;
    }

    let diff_output = Command::new("git")
        .args(["-C", dir, "diff", "HEAD", "--shortstat"])
        .output()
        .ok();
    let (added, removed) = diff_output
        .filter(|output| output.status.success())
        .map(|output| parse_shortstat(&String::from_utf8_lossy(&output.stdout)))
        .unwrap_or((0, 0));

    Some(format!("{branch}  +{added}/-{removed}"))
}

/// Parses the counts out of `git diff --shortstat` output, e.g.
/// " 2 files changed, 10 insertions(+), 3 deletions(-)". Either count (or
/// both) may be absent when nothing was added/removed.
fn parse_shortstat(text: &str) -> (u32, u32) {
    let count_containing = |word: &str| {
        text.split(',')
            .find(|part| part.contains(word))
            .and_then(|part| part.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    (count_containing("insertion"), count_containing("deletion"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_both_counts() {
        let text = " 2 files changed, 10 insertions(+), 3 deletions(-)\n";
        assert_eq!(parse_shortstat(text), (10, 3));
    }

    #[test]
    fn parses_singular_counts() {
        let text = " 1 file changed, 1 insertion(+), 1 deletion(-)\n";
        assert_eq!(parse_shortstat(text), (1, 1));
    }

    #[test]
    fn parses_insertions_only() {
        let text = " 1 file changed, 5 insertions(+)\n";
        assert_eq!(parse_shortstat(text), (5, 0));
    }

    #[test]
    fn empty_diff_is_zero_zero() {
        assert_eq!(parse_shortstat(""), (0, 0));
    }

    #[test]
    fn git_summary_is_none_outside_a_repository() {
        let dir = std::env::temp_dir().join(format!("cctiles-not-a-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");
        assert_eq!(
            git_summary(dir.to_str().expect("path should be valid utf-8")),
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn git_summary_reports_the_current_branch_and_a_clean_diffstat() {
        let dir = std::env::temp_dir().join(format!("cctiles-git-repo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create scratch dir");
        let dir_str = dir.to_str().expect("path should be valid utf-8");
        let run = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .args([&["-C", dir_str], args].concat())
                    .env("GIT_AUTHOR_NAME", "test")
                    .env("GIT_AUTHOR_EMAIL", "test@example.com")
                    .env("GIT_COMMITTER_NAME", "test")
                    .env("GIT_COMMITTER_EMAIL", "test@example.com")
                    .output()
                    .expect("failed to run git")
                    .status
                    .success()
            );
        };
        run(&["init", "--initial-branch=main", "--quiet"]);
        std::fs::write(dir.join("file.txt"), "hello\n").expect("failed to write test file");
        run(&["add", "file.txt"]);
        run(&["commit", "--quiet", "-m", "initial commit"]);

        assert_eq!(git_summary(dir_str), Some("main  +0/-0".to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
