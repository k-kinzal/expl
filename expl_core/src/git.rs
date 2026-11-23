//! Git infrastructure and discovery tests for the exploration application.

pub use expl_runtime::git::{BranchBlocker, Git, GitError};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn git_uses_selected_checkout_and_index() {
        use crate::Project;
        use std::fs;

        // Only the child inherits the overrides, so other tests run normally.
        const CHILD_ROOT: &str = "EXPL_TEST_GIT_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let root = PathBuf::from(root);
            let project = Project::discover(&root.join(".expl")).expect("discover project");
            assert_eq!(project.root(), root);

            let worktree = project.worktrees_dir().join("node-test");
            assert_eq!(
                Project::for_repository(&worktree)
                    .expect("discover linked worktree")
                    .root(),
                worktree
            );
            let git = Git::in_worktree(worktree);
            assert!(git.has_branch("selected").expect("read shared refs"));
            git.run(&["add", "--", "change"]).expect("stage change");
            assert_eq!(git.status_paths().expect("worktree status"), ["change"]);
            assert_eq!(
                git.run(&["status", "--porcelain"]).expect("staged status"),
                "A  change"
            );
            git.run(&["branch", "created-by-child"])
                .expect("write selected ref");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let dir_path = dir.path().canonicalize().expect("canonical tempdir");
        let root = dir_path.join("selected");
        let other = dir_path.join("other");
        for (repo, branch) in [(&root, "selected"), (&other, "other")] {
            fs::create_dir(repo).expect("create repository directory");
            let git = Git::new(repo);
            git.run(&["init", "-q", "-b", branch]).expect("init");
            fs::write(repo.join(branch), branch).expect("write tracked file");
            git.run(&["add", "--", branch]).expect("stage tracked file");
            git.run(&[
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "initial",
            ])
            .expect("commit");
        }
        let project = Project::at(&root);
        fs::create_dir_all(project.worktrees_dir()).expect("create worktrees directory");
        fs::write(project.database_path(), "").expect("mark project initialized");
        let worktree = project.worktrees_dir().join("node-test");
        let git = Git::new(&root);
        git.run(&[
            "worktree",
            "add",
            "-qb",
            "node-test",
            worktree.to_str().expect("worktree path"),
        ])
        .expect("add worktree");
        fs::write(worktree.join("change"), "new content").expect("write change");

        let other_git = Git::new(&other);
        let other_refs = other_git.run(&["show-ref", "--head"]).expect("other refs");
        let other_dir = other.join(".git");
        let other_index = fs::read(other_dir.join("index")).expect("other index");
        let output = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "git::tests::git_uses_selected_checkout_and_index",
            ])
            .env(CHILD_ROOT, &root)
            .env("GIT_DIR", &other_dir)
            .env("GIT_WORK_TREE", &other)
            .env("GIT_COMMON_DIR", &other_dir)
            .env("GIT_INDEX_FILE", other_dir.join("index"))
            .env("GIT_OBJECT_DIRECTORY", other_dir.join("objects"))
            .env("GIT_CEILING_DIRECTORIES", &root)
            .output()
            .expect("run child test");
        assert!(
            output.status.success(),
            "child test failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        assert!(git.has_branch("created-by-child").expect("selected refs"));
        assert_eq!(
            Git::new(worktree)
                .run(&["show", ":change"])
                .expect("staged content"),
            "new content"
        );
        assert_eq!(
            other_git.run(&["show-ref", "--head"]).expect("other refs"),
            other_refs
        );
        assert_eq!(
            fs::read(other_dir.join("index")).expect("other index"),
            other_index
        );
        assert_eq!(fs::read(other.join("other")).expect("other file"), b"other");
        assert_eq!(fs::read_dir(other).expect("other checkout").count(), 2);
    }
}
