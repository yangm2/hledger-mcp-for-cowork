//! Minimal git plumbing for the write path, via the **`git2`** crate (libgit2) — no `git`
//! subprocess (CLAUDE.md *Stack*). The crate keeps `#![forbid(unsafe_code)]`: the FFI lives in
//! `libgit2-sys`.
//!
//! The model: **one validated write = one commit = one epoch.** A commit's oid is the epoch id
//! M3's CAS checks against ([`GitRepo::head_oid`]). Commits use a fixed **synthetic** identity
//! (no PII — this is a public-repo project).

use std::path::Path;

use git2::{ErrorCode, IndexAddOption, Repository, Signature};

use crate::epoch::CommitOid;

/// Synthetic commit identity — never a real person (public repo, no PII).
const AUTHOR_NAME: &str = "hledger-mcp";
const AUTHOR_EMAIL: &str = "hledger-mcp@localhost";

/// Errors from the git layer.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    /// Any underlying libgit2 failure.
    #[error("git error: {0}")]
    Git(#[from] git2::Error),
}

/// A handle to the ledger's git repository (the directory containing the journal).
pub struct GitRepo {
    repo: Repository,
}

impl GitRepo {
    /// Open the repository at `dir` **without** initializing — `None` if `dir` is not a repo.
    /// For read-only status checks that must not create a repo as a side effect.
    pub fn open(dir: &Path) -> Result<Option<Self>, GitError> {
        match Repository::open(dir) {
            Ok(repo) => Ok(Some(Self { repo })),
            Err(err) if err.code() == ErrorCode::NotFound => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    /// Open the repository at `dir`, initializing one if none exists (the bootstrap path).
    pub fn open_or_init(dir: &Path) -> Result<Self, GitError> {
        let repo = match Repository::open(dir) {
            Ok(repo) => repo,
            Err(err) if err.code() == ErrorCode::NotFound => Repository::init(dir)?,
            Err(err) => return Err(err.into()),
        };
        Ok(Self { repo })
    }

    /// The current `HEAD` commit oid, or `None` when `HEAD` is unborn (a freshly
    /// `init`-ed repo with no commits yet). This **is the epoch**.
    pub fn head_oid(&self) -> Result<Option<CommitOid>, GitError> {
        match self.repo.head() {
            Ok(head) => Ok(head.target().map(|oid| CommitOid::new(oid.to_string()))),
            // Unborn branch (no commits yet) / no HEAD ref.
            Err(err)
                if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound =>
            {
                Ok(None)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Whether the working tree (or index) differs from `HEAD` — i.e. there are uncommitted
    /// changes. On an unborn HEAD, any tracked/untracked content counts as dirty.
    pub fn is_dirty(&self) -> Result<bool, GitError> {
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true);
        let statuses = self.repo.statuses(Some(&mut opts))?;
        Ok(!statuses.is_empty())
    }

    /// Whether **one specific path** (relative to the workdir) differs from `HEAD` — new,
    /// modified, or staged. Unlike [`is_dirty`](Self::is_dirty) this ignores every other file,
    /// so an unrelated untracked file (e.g. an abandoned candidate temp) does not register.
    pub fn is_path_dirty(&self, relpath: &Path) -> Result<bool, GitError> {
        match self.repo.status_file(relpath) {
            // `CURRENT` (clean) is the empty status set.
            Ok(status) => Ok(!status.is_empty()),
            // The path isn't tracked and isn't in the worktree → nothing to commit.
            Err(err) if err.code() == ErrorCode::NotFound => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Stage `relpath` (relative to the repo workdir) and commit it onto `HEAD`, returning the
    /// new commit oid. Handles the unborn-HEAD (first commit) case.
    pub fn commit_path(&self, relpath: &Path, message: &str) -> Result<CommitOid, GitError> {
        let mut index = self.repo.index()?;
        index.add_all([relpath].iter(), IndexAddOption::DEFAULT, None)?;
        index.write()?;
        let tree_oid = index.write_tree()?;
        let tree = self.repo.find_tree(tree_oid)?;
        let sig = Signature::now(AUTHOR_NAME, AUTHOR_EMAIL)?;

        let parents = match self.repo.head() {
            Ok(head) => {
                let parent = head.peel_to_commit()?;
                vec![parent]
            }
            Err(err)
                if err.code() == ErrorCode::UnbornBranch || err.code() == ErrorCode::NotFound =>
            {
                vec![]
            }
            Err(err) => return Err(err.into()),
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)?;
        Ok(CommitOid::new(oid.to_string()))
    }

    /// Restore the working tree to `HEAD` (force checkout), discarding uncommitted changes —
    /// the crash-reconciliation "else restore" branch. No-op semantics on an unborn HEAD.
    pub fn restore_to_head(&self) -> Result<(), GitError> {
        if self.head_oid()?.is_none() {
            return Ok(());
        }
        let mut checkout = git2::build::CheckoutBuilder::new();
        checkout.force();
        self.repo.checkout_head(Some(&mut checkout))?;
        Ok(())
    }

    /// The repository's working-directory path (where the journal lives).
    pub fn workdir(&self) -> Option<&Path> {
        self.repo.workdir()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_repo() -> (tempfile::TempDir, GitRepo) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = GitRepo::open_or_init(dir.path()).expect("init");
        (dir, repo)
    }

    #[test]
    fn fresh_repo_has_unborn_head_and_is_dirty_after_write() {
        let (dir, repo) = temp_repo();
        assert_eq!(repo.head_oid().unwrap(), None, "unborn HEAD");
        assert!(!repo.is_dirty().unwrap(), "empty repo is clean");
        std::fs::write(dir.path().join("main.journal"), "; hi\n").unwrap();
        assert!(repo.is_dirty().unwrap(), "untracked file makes it dirty");
    }

    #[test]
    fn commit_advances_head_and_cleans_tree() {
        let (dir, repo) = temp_repo();
        std::fs::write(dir.path().join("main.journal"), "; one\n").unwrap();
        let oid1 = repo
            .commit_path(Path::new("main.journal"), "first")
            .expect("first commit");
        assert_eq!(repo.head_oid().unwrap().as_deref(), Some(oid1.as_str()));
        assert!(!repo.is_dirty().unwrap(), "clean after commit");

        std::fs::write(dir.path().join("main.journal"), "; two\n").unwrap();
        let oid2 = repo
            .commit_path(Path::new("main.journal"), "second")
            .expect("second commit");
        assert_ne!(
            oid1.as_str(),
            oid2.as_str(),
            "each write is a distinct epoch"
        );
        assert!(!repo.is_dirty().unwrap());
    }

    #[test]
    fn restore_to_head_discards_uncommitted_changes() {
        let (dir, repo) = temp_repo();
        let journal = dir.path().join("main.journal");
        std::fs::write(&journal, "; committed\n").unwrap();
        repo.commit_path(Path::new("main.journal"), "c").unwrap();
        std::fs::write(&journal, "; uncommitted garbage\n").unwrap();
        assert!(repo.is_dirty().unwrap());
        repo.restore_to_head().unwrap();
        assert_eq!(std::fs::read_to_string(&journal).unwrap(), "; committed\n");
        assert!(!repo.is_dirty().unwrap());
    }

    #[test]
    fn restore_on_unborn_head_is_noop() {
        let (_dir, repo) = temp_repo();
        repo.restore_to_head().expect("no-op on unborn HEAD");
    }

    #[test]
    fn is_path_dirty_tracks_one_file_and_ignores_others() {
        let (dir, repo) = temp_repo();
        let journal = Path::new("main.journal");
        std::fs::write(dir.path().join(journal), "; one\n").unwrap();
        // Untracked, present in worktree → dirty.
        assert!(repo.is_path_dirty(journal).unwrap(), "new file is dirty");
        repo.commit_path(journal, "c").unwrap();
        assert!(!repo.is_path_dirty(journal).unwrap(), "clean after commit");

        // An unrelated untracked file must NOT make the journal look dirty.
        std::fs::write(
            dir.path().join(".hledger-mcp-candidate-x.journal"),
            "junk\n",
        )
        .unwrap();
        assert!(
            !repo.is_path_dirty(journal).unwrap(),
            "stray file does not dirty the journal path"
        );
        assert!(repo.is_dirty().unwrap(), "but repo-wide status sees it");

        // Editing the journal itself does register.
        std::fs::write(dir.path().join(journal), "; two\n").unwrap();
        assert!(repo.is_path_dirty(journal).unwrap(), "edit is dirty");
    }
}
