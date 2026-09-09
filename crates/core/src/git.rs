use crate::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum Source {
    PlainDirectory,
    /// A checkout whose `.git` is a self-contained directory.
    Repository,
    /// A `git worktree add` checkout: `.git` is a `gitdir:` pointer file and
    /// `common_dir` is the repository directory shared with the main checkout.
    LinkedWorktree { common_dir: PathBuf },
}

impl Source {
    pub(crate) fn is_linked_worktree(&self) -> bool {
        matches!(self, Self::LinkedWorktree { .. })
    }

    /// The `info/exclude` file that hides `.rift` for the checkout at `path`,
    /// or `None` when Git is not involved. A linked worktree has no exclude
    /// file of its own — Git reads the shared repository's — so one entry
    /// there covers the main checkout and every worktree.
    pub(crate) fn exclude_file(&self, path: &Path) -> Option<PathBuf> {
        match self {
            Self::PlainDirectory => None,
            Self::Repository => Some(repository_exclude_file(path)),
            Self::LinkedWorktree { common_dir } => Some(common_dir.join("info").join("exclude")),
        }
    }
}

/// The `info/exclude` file of a checkout whose `.git` is a directory.
pub(crate) fn repository_exclude_file(path: &Path) -> PathBuf {
    path.join(".git").join("info").join("exclude")
}

pub(crate) fn check_source(path: &Path) -> Result<Source> {
    let git = path.join(".git");
    let metadata = match fs::metadata(&git) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Source::PlainDirectory);
        }
        Err(error) => return Err(error.into()),
    };
    let (source, git_dir) = if metadata.is_dir() {
        (Source::Repository, git)
    } else {
        let git_dir = worktree_git_dir(path)?;
        let common_dir = common_git_dir(&git_dir)?;
        (Source::LinkedWorktree { common_dir }, git_dir)
    };

    for state in [
        "MERGE_HEAD",
        "CHERRY_PICK_HEAD",
        "REVERT_HEAD",
        "BISECT_LOG",
        "rebase-merge",
        "rebase-apply",
        "index.lock",
        "HEAD.lock",
    ] {
        if git_dir.join(state).exists() {
            return Err(Error::UnsafeGit(format!("Git state in progress: {state}")));
        }
    }
    Ok(source)
}

/// Resolves the Git directory named by a `.git` pointer file
/// (`gitdir: <path>`). The target must exist; a stale pointer left behind by
/// a moved or deleted repository is refused rather than acted on.
fn worktree_git_dir(path: &Path) -> Result<PathBuf> {
    let contents = fs::read_to_string(path.join(".git"))?;
    let target = contents
        .lines()
        .find_map(|line| line.trim().strip_prefix("gitdir:"))
        .map(str::trim)
        .filter(|target| !target.is_empty())
        .ok_or_else(|| Error::UnsafeGit("malformed .git file: expected a `gitdir:` line".into()))?;
    let git_dir = path.join(target);
    if !git_dir.is_dir() {
        return Err(Error::UnsafeGit(format!(
            "Git directory named by .git does not exist: {}",
            git_dir.display()
        )));
    }
    Ok(git_dir)
}

/// Resolves the repository directory a linked worktree shares with its main
/// checkout, named relative to the worktree's Git directory in `commondir`.
/// A `gitdir:` pointer without one — a submodule checkout, for example — is
/// not a linked worktree and is refused.
fn common_git_dir(git_dir: &Path) -> Result<PathBuf> {
    match fs::read_to_string(git_dir.join("commondir")) {
        Ok(relative) => Ok(git_dir.join(relative.trim())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Error::UnsafeGit(
            ".git points to a Git directory that is not a linked worktree; submodule checkouts are not supported"
                .into(),
        )),
        Err(error) => Err(error.into()),
    }
}

/// Appends `/.rift` to the given `info/exclude` file so the marker stays out
/// of `git status`. Idempotent.
pub(crate) fn hide_marker(exclude: &Path) -> Result<()> {
    if let Some(info) = exclude.parent() {
        fs::create_dir_all(info)?;
    }
    let existing = match fs::read_to_string(exclude) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };
    if existing.lines().any(|line| line.trim() == "/.rift") {
        return Ok(());
    }
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    fs::write(exclude, format!("{existing}{separator}/.rift\n"))?;
    Ok(())
}

pub(crate) fn detach_destination(path: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    // Avoid process startup when libgit2 understands the repository;
    // the Git CLI remains the authority for layouts it cannot resolve.
    if let Some(commit) = resolve_head_commit(path) {
        fs::write(path.join(".git").join("HEAD"), format!("{commit}\n"))?;
        return Ok(());
    }

    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--verify", "HEAD^{commit}"])
        .output()?;
    if !output.status.success() {
        return Ok(());
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    fs::write(path.join(".git").join("HEAD"), format!("{commit}\n"))?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn resolve_head_commit(path: &Path) -> Option<git2::Oid> {
    let repository = git2::Repository::open(path).ok()?;
    repository
        .head()
        .ok()?
        .peel_to_commit()
        .ok()
        .map(|commit| commit.id())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{linked_worktree, run};
    use tempfile::TempDir;

    #[test]
    fn stale_worktree_pointer_is_rejected() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".git"), "gitdir: elsewhere").unwrap();

        assert!(matches!(
            check_source(temp.path()),
            Err(Error::UnsafeGit(_))
        ));
        assert!(!temp.path().join("elsewhere").exists());
    }

    #[test]
    fn malformed_worktree_marker_is_rejected() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".git"), "not a gitdir line").unwrap();

        assert!(matches!(
            check_source(temp.path()),
            Err(Error::UnsafeGit(_))
        ));
    }

    #[test]
    fn pointer_without_commondir_is_not_a_linked_worktree() {
        // A submodule checkout points at `<super>/.git/modules/<name>`, a real
        // Git directory that has no `commondir`.
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join("modules_lib")).unwrap();
        fs::write(temp.path().join(".git"), "gitdir: modules_lib").unwrap();

        assert!(matches!(
            check_source(temp.path()),
            Err(Error::UnsafeGit(message)) if message.contains("submodule")
        ));
    }

    #[test]
    fn check_source_distinguishes_plain_and_git_directories() {
        let plain = TempDir::new().unwrap();
        assert_eq!(check_source(plain.path()).unwrap(), Source::PlainDirectory);

        let git = TempDir::new().unwrap();
        fs::create_dir(git.path().join(".git")).unwrap();
        assert_eq!(check_source(git.path()).unwrap(), Source::Repository);
    }

    #[test]
    fn real_linked_worktree_resolves_the_shared_repository() {
        let temp = TempDir::new().unwrap();
        let (main, linked) = linked_worktree(&temp);

        assert_eq!(check_source(&main).unwrap(), Source::Repository);
        let Source::LinkedWorktree { common_dir } = check_source(&linked).unwrap() else {
            panic!("expected a linked worktree");
        };
        assert_eq!(fs::canonicalize(common_dir).unwrap(), main.join(".git"));

        // In-progress state lives in the worktree's own Git directory.
        fs::write(worktree_git_dir(&linked).unwrap().join("MERGE_HEAD"), "x").unwrap();
        assert!(matches!(
            check_source(&linked),
            Err(Error::UnsafeGit(_))
        ));
    }

    #[test]
    fn hide_marker_creates_and_appends_exclude_cleanly() {
        let temp = TempDir::new().unwrap();
        fs::create_dir(temp.path().join(".git")).unwrap();
        let exclude = repository_exclude_file(temp.path());

        hide_marker(&exclude).unwrap();
        assert_eq!(fs::read_to_string(&exclude).unwrap(), "/.rift\n");
        fs::write(&exclude, "existing").unwrap();
        hide_marker(&exclude).unwrap();
        assert_eq!(fs::read_to_string(&exclude).unwrap(), "existing\n/.rift\n");
        hide_marker(&exclude).unwrap();
        assert_eq!(fs::read_to_string(&exclude).unwrap(), "existing\n/.rift\n");
    }

    #[test]
    fn hide_marker_on_linked_worktree_writes_the_shared_exclude() {
        let temp = TempDir::new().unwrap();
        let (main, linked) = linked_worktree(&temp);
        let exclude = check_source(&linked)
            .unwrap()
            .exclude_file(&linked)
            .unwrap();

        hide_marker(&exclude).unwrap();

        // `git init` seeds the shared exclude with a comment template, so the
        // marker line is appended rather than being the whole file.
        assert!(
            fs::read_to_string(main.join(".git/info/exclude"))
                .unwrap()
                .lines()
                .any(|line| line == "/.rift")
        );
        // The worktree's `.git` stays a pointer file; nothing was created under it.
        assert!(linked.join(".git").is_file());
    }

    #[test]
    fn detach_does_nothing_for_a_repository_without_a_commit() {
        let temp = TempDir::new().unwrap();
        run(temp.path(), &["init", "-q"]);
        let head = fs::read_to_string(temp.path().join(".git/HEAD")).unwrap();

        detach_destination(temp.path()).unwrap();

        assert_eq!(
            fs::read_to_string(temp.path().join(".git/HEAD")).unwrap(),
            head
        );
    }
}
