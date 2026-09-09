use crate::{Error, Result};
use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use std::path::Path;

/// Built-in artifacts excluded from filtered copies, expressed as gitignore
/// patterns. Bare names match a directory at any depth; the `**/` prefix keeps
/// the multi-segment `.yarn/*` artifacts matching at any depth too.
const DEFAULT_EXCLUDES: &[&str] = &[
    "node_modules",
    ".pnpm-store",
    "**/.yarn/cache",
    "**/.yarn/unplugged",
    "**/.yarn/install-state.gz",
    "**/.yarn/build-state.yml",
    "target",
    ".venv",
    "venv",
    ".tox",
    ".nox",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".turbo",
    ".vite",
    ".parcel-cache",
    ".cache",
    "dist",
    "build",
    "coverage",
];

#[derive(Clone, Debug)]
pub(crate) struct CopyFilter {
    matcher: Gitignore,
    no_git: bool,
}

impl Default for CopyFilter {
    fn default() -> Self {
        Self::new(&[], &[], false).expect("built-in exclude patterns are valid")
    }
}

impl CopyFilter {
    /// Builds a filter from the built-in defaults, treating every `exclude`
    /// entry as a gitignore pattern and every `include` entry as a gitignore
    /// negation. Later patterns win, so an `include` overrides both the
    /// built-in defaults and any earlier `exclude` for the same path. `no_git`
    /// additionally drops the copy root's `.git`; see [`Self::excludes`].
    /// Returns [`Error::InvalidFilter`] if a pattern cannot be parsed.
    pub(crate) fn new(exclude: &[String], include: &[String], no_git: bool) -> Result<Self> {
        let mut builder = GitignoreBuilder::new("");
        for pattern in DEFAULT_EXCLUDES {
            add_pattern(&mut builder, pattern)?;
        }
        let user_patterns = exclude
            .iter()
            .map(|pattern| (pattern, false))
            .chain(include.iter().map(|pattern| (pattern, true)));
        for (raw, negate) in user_patterns {
            if let Some(line) = user_pattern(raw, negate) {
                add_pattern(&mut builder, &line)?;
            }
        }
        let matcher = builder
            .build()
            .map_err(|error| Error::InvalidFilter(error.to_string()))?;
        Ok(Self { matcher, no_git })
    }

    /// Whether the entry at `path` (relative to the copy root) is left out of
    /// the copy. This is evaluated for every entry of a pruning walk: an
    /// excluded directory is never descended into, so each entry only has to
    /// match on its own and its ancestors are already known to be included.
    ///
    /// Git's own data is never subject to the patterns: anything at or under
    /// a `.git` entry is kept, with one exception — `no_git` drops the copy
    /// root's own `.git` (directory or worktree pointer file). Nested `.git`
    /// entries such as submodules or vendored repositories are ordinary
    /// content and are kept either way.
    pub(crate) fn excludes(&self, path: &Path, is_dir: bool) -> bool {
        if path
            .components()
            .any(|component| component.as_os_str() == ".git")
        {
            return self.no_git && path == Path::new(".git");
        }
        matches!(self.matcher.matched(path, is_dir), Match::Ignore(_))
    }

    /// [`Self::excludes`] for a walkdir entry rooted at `from`. Entries that
    /// are not under `from` are never excluded.
    pub(crate) fn excludes_entry(&self, from: &Path, entry: &walkdir::DirEntry) -> bool {
        entry
            .path()
            .strip_prefix(from)
            .map_or(false, |path| self.excludes(path, entry.file_type().is_dir()))
    }
}

/// Turns a user-supplied value into a gitignore line, or `None` for a blank
/// value. The flag that carried the value decides its direction (`negate`
/// adds the leading `!`), so a `#` or `!` the user wrote first is escaped and
/// read as part of the path rather than as a comment or a negation. Other
/// bytes reach the glob parser unchanged.
fn user_pattern(raw: &str, negate: bool) -> Option<String> {
    if raw.trim().is_empty() {
        return None;
    }
    let escaped = if raw.starts_with('#') || raw.starts_with('!') {
        format!("\\{raw}")
    } else {
        raw.to_owned()
    };
    Some(if negate {
        format!("!{escaped}")
    } else {
        escaped
    })
}

fn add_pattern(builder: &mut GitignoreBuilder, pattern: &str) -> Result<()> {
    builder
        .add_line(None, pattern)
        .map_err(|error| Error::InvalidFilter(error.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build(exclude: &[&str], include: &[&str], no_git: bool) -> CopyFilter {
        let owned = |patterns: &[&str]| patterns.iter().map(|p| (*p).to_owned()).collect::<Vec<_>>();
        CopyFilter::new(&owned(exclude), &owned(include), no_git).unwrap()
    }

    #[test]
    fn excludes_artifacts_at_any_depth() {
        let filter = CopyFilter::default();

        assert!(filter.excludes(Path::new("packages/app/node_modules"), true));
        assert!(filter.excludes(Path::new("packages/app/.yarn/cache"), true));
        assert!(filter.excludes(Path::new("packages/app/.yarn/install-state.gz"), false));
        assert!(!filter.excludes(Path::new("packages/app/package-lock.json"), false));
    }

    #[test]
    fn extra_excludes_extend_the_defaults() {
        let filter = build(&["fixtures"], &[], false);

        assert!(filter.excludes(Path::new("packages/app/fixtures"), true));
        assert!(filter.excludes(Path::new("packages/app/node_modules"), true));
    }

    #[test]
    fn included_names_reinclude_a_default() {
        let filter = build(&[], &["dist"], false);

        assert!(!filter.excludes(Path::new("packages/app/dist"), true));
        assert!(filter.excludes(Path::new("packages/app/node_modules"), true));
    }

    #[test]
    fn include_wins_over_exclude_for_the_same_name() {
        let filter = build(&["dist"], &["dist"], false);

        assert!(!filter.excludes(Path::new("packages/app/dist"), true));
    }

    #[test]
    fn multi_segment_patterns_anchor_like_gitignore() {
        let filter = build(&["foo/bar"], &[], false);

        // A middle slash anchors to the root, so only the top-level match hits.
        assert!(filter.excludes(Path::new("foo/bar"), true));
        assert!(!filter.excludes(Path::new("packages/app/foo/bar"), true));
    }

    #[test]
    fn double_star_matches_nested_segments() {
        let filter = build(&["**/foo/bar"], &[], false);

        assert!(filter.excludes(Path::new("packages/app/foo/bar"), true));
    }

    #[test]
    fn blank_names_are_ignored() {
        let filter = build(&["   "], &[""], false);

        assert!(filter.excludes(Path::new("node_modules"), true));
    }

    #[test]
    fn leading_bang_or_hash_is_part_of_the_path() {
        // `!` must not flip an exclude into a re-include, and `#` must not turn
        // the value into a gitignore comment that is silently dropped.
        let filter = build(&["!node_modules", "#tmp"], &[], false);

        assert!(filter.excludes(Path::new("node_modules"), true));
        assert!(filter.excludes(Path::new("!node_modules"), true));
        assert!(filter.excludes(Path::new("#tmp"), true));
    }

    #[test]
    fn git_data_is_never_filtered() {
        // `build` is a built-in default and `main` a user exclude, yet neither
        // may touch a ref file inside the copied `.git` directory.
        let filter = build(&["main", ".*"], &[], false);

        assert!(!filter.excludes(Path::new(".git"), true));
        assert!(!filter.excludes(Path::new(".git/refs/heads/build"), false));
        assert!(!filter.excludes(Path::new(".git/refs/heads/main"), false));
        assert!(filter.excludes(Path::new("main"), true));
        assert!(filter.excludes(Path::new(".env"), false));
    }

    #[test]
    fn no_git_drops_only_the_root_git_entry() {
        // `--include .git` cannot undo it; nested `.git` entries are content.
        let filter = build(&[], &[".git"], true);

        assert!(filter.excludes(Path::new(".git"), true));
        assert!(filter.excludes(Path::new(".git"), false));
        assert!(!filter.excludes(Path::new("vendor/dep/.git"), true));
        assert!(!filter.excludes(Path::new(".gitignore"), false));
    }

    #[test]
    fn invalid_patterns_are_rejected() {
        assert!(matches!(
            CopyFilter::new(&["\\".to_owned()], &[], false),
            Err(Error::InvalidFilter(_))
        ));
    }
}
