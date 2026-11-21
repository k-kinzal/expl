//! `expl init`.

use std::io::Write;
use std::path::Path;

use expl_core::git::Git;
use expl_core::{Declaration, Graph, Project};

use crate::cli::InitArgs;
use crate::error::CliError;
use crate::output::{println, short_commit, status};

pub(crate) fn init(start: &Path, args: &InitArgs) -> Result<(), CliError> {
    let project = Project::for_repository(start)?;
    if project.is_initialized() {
        return Err(CliError::Usage(format!(
            "expl is already initialised at {}",
            project.root().display()
        )));
    }
    let git = Git::new(project.root());
    let base = git.resolve_commit(&args.base).map_err(|_| {
        CliError::Usage(format!(
            "`{}` does not name a commit (expl needs at least one commit to start from)",
            args.base
        ))
    })?;

    let state = project.state_dir();
    for dir in [project.logs_dir(), project.processes_dir()] {
        std::fs::create_dir_all(&dir)
            .map_err(|e| CliError::io(format!("create {}", state.display()), e))?;
    }
    exclude_state_dir(&git)?;

    let declaration_path = project.declaration_path();
    let trunk = if declaration_path.exists() {
        let declaration = Declaration::load(&declaration_path)?;
        status(&format!("using existing {}", Project::DECLARATION_FILE));
        match (&args.trunk, declaration.trunk) {
            (None, Some(declared)) => Some(declared),
            _ => trunk_branch(&git, args)?,
        }
    } else {
        let trunk = trunk_branch(&git, args)?;
        std::fs::write(&declaration_path, Declaration::template(trunk.as_deref()))
            .map_err(|e| CliError::io(format!("write {}", declaration_path.display()), e))?;
        status(&format!("wrote {}", Project::DECLARATION_FILE));
        trunk
    };

    let mut graph = Graph::open(&project.database_path())?;
    let origin = graph.create_origin("Origin", trunk.as_deref(), &base)?;
    status(&format!(
        "initialised {} with origin {origin} at {}{}",
        project.root().display(),
        short_commit(Some(&base)),
        trunk.as_deref().map_or_else(
            || " (no trunk branch to follow)".to_owned(),
            |t| format!(", following {t}")
        )
    ));
    status("next: expl goal add \"<what you want>\" && expl up");
    println(&origin.0.to_string())
}

/// The branch to follow: `--trunk`, else the branch checked out (when
/// starting from it), else `--base` when it names one.
fn trunk_branch(git: &Git, args: &InitArgs) -> Result<Option<String>, CliError> {
    if let Some(trunk) = &args.trunk {
        if !git.has_branch(trunk)? {
            return Err(CliError::Usage(format!("`{trunk}` is not a local branch")));
        }
        return Ok(Some(trunk.clone()));
    }
    if args.base == "HEAD" {
        return Ok(git.current_branch()?);
    }
    Ok(git.has_branch(&args.base)?.then(|| args.base.clone()))
}

/// Add `/.expl/` to the repository's `info/exclude` so expl's state never
/// shows up as untracked, without touching tracked files.
fn exclude_state_dir(git: &Git) -> Result<(), CliError> {
    let common = git.run(&["rev-parse", "--git-common-dir"])?;
    let common = if Path::new(&common).is_absolute() {
        std::path::PathBuf::from(common)
    } else {
        git.dir().join(common)
    };
    let exclude = common.join("info").join("exclude");
    let entry = format!("/{}/", Project::STATE_DIR);
    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|line| line.trim() == entry) {
        return Ok(());
    }
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| CliError::io(format!("create {}", parent.display()), e))?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&exclude)
        .map_err(|e| CliError::io(format!("open {}", exclude.display()), e))?;
    let separator = if existing.is_empty() || existing.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    writeln!(file, "{separator}{entry}")
        .map_err(|e| CliError::io(format!("write {}", exclude.display()), e))
}
