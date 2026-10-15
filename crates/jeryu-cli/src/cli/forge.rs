//! `jeryu forge` taxonomy: repositories, pull requests, and issues.
//!
//! Pull requests are GitHub-shaped: a pull request is opened from a `--head`
//! branch into a `--base` branch and is addressed by a per-repo `#number`.

use clap::Subcommand;

/// Forge command group.
#[derive(Debug, Subcommand)]
pub enum ForgeCommands {
    /// Repository operations.
    #[command(subcommand)]
    Repo(RepoCommands),

    /// Pull request operations.
    #[command(subcommand)]
    Pr(PrCommands),

    /// Issue operations.
    #[command(subcommand)]
    Issue(IssueCommands),
}

/// `jeryu auth` operations.
#[derive(Debug, Subcommand)]
pub enum AuthCommands {
    /// Check the token with one read-only whoami call. Exits non-zero when
    /// the forge refuses it, so no write is attempted with a dead token.
    Doctor,
}

/// `jeryu forge repo` operations.
#[derive(Debug, Subcommand)]
pub enum RepoCommands {
    /// Create a repository under the acting owner.
    Create {
        /// Repository name.
        name: String,
        /// Create the repository as private.
        #[arg(long, default_value_t = false)]
        private: bool,
        /// Default branch for the new repository.
        #[arg(long, default_value = "main")]
        default_branch: String,
    },
    /// List repositories for the acting owner.
    List,
}

/// `jeryu forge pr` operations.
#[derive(Debug, Subcommand)]
pub enum PrCommands {
    /// Open a pull request from a head branch into a base branch.
    Open {
        /// Repository name (under the acting owner).
        #[arg(long)]
        repo: String,
        /// Source branch carrying the changes.
        #[arg(long)]
        head: String,
        /// Target branch to merge into.
        #[arg(long, default_value = "main")]
        base: String,
        /// Pull request title.
        #[arg(long)]
        title: String,
        /// Open the pull request as a draft.
        #[arg(long, default_value_t = false)]
        draft: bool,
    },
    /// List pull requests for a repository.
    List {
        /// Repository name (under the acting owner).
        #[arg(long)]
        repo: String,
    },
    /// Show the status of a pull request by number.
    Status {
        /// Repository name (under the acting owner).
        #[arg(long)]
        repo: String,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
    },
    /// Merge a pull request, subject to the risk gate.
    Merge {
        /// Repository name (under the acting owner).
        #[arg(long)]
        repo: String,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
        /// Compatibility value: only trusted is supported; the server enforces merge policy.
        #[arg(long, default_value = "trusted")]
        trust_tier: String,
    },
    /// Show a pull request: head, base, state and mergeability.
    View {
        /// Repository, `name` under --owner or `owner/name`. Defaults to the
        /// git remote `origin` of the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
    },
    /// Show the check results of a pull request.
    Checks {
        /// Repository, `name` under --owner or `owner/name`. Defaults to the
        /// git remote `origin` of the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
    },
    /// Comment on a pull request.
    Comment {
        /// Repository, `name` under --owner or `owner/name`. Defaults to the
        /// git remote `origin` of the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
        /// Comment text (markdown).
        #[arg(long)]
        body: String,
    },
    /// Close a pull request, optionally naming the one that replaces it.
    Close {
        /// Repository, `name` under --owner or `owner/name`. Defaults to the
        /// git remote `origin` of the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
        /// Number of the pull request that replaces this one.
        #[arg(long)]
        superseded_by: Option<u64>,
    },
    /// Approve the current head of a pull request.
    Approve {
        /// Repository, `name` under --owner or `owner/name`. Defaults to the
        /// git remote `origin` of the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
        /// Head commit the approval is for. Defaults to the head the forge
        /// reports now.
        #[arg(long)]
        expected_head_sha: Option<String>,
    },
    /// Ask the gate to run again on the current head.
    Regate {
        /// Repository, `name` under --owner or `owner/name`. Defaults to the
        /// git remote `origin` of the current directory.
        #[arg(long)]
        repo: Option<String>,
        /// Pull request number (#N).
        #[arg(long)]
        pr: u64,
    },
}

/// `jeryu forge issue` operations.
#[derive(Debug, Subcommand)]
pub enum IssueCommands {
    /// Create an issue in a repository.
    Create {
        /// Repository name (under the acting owner).
        #[arg(long)]
        repo: String,
        /// Issue title.
        #[arg(long)]
        title: String,
        /// Optional issue body.
        #[arg(long)]
        body: Option<String>,
    },
    /// List issues in a repository.
    List {
        /// Repository name (under the acting owner).
        #[arg(long)]
        repo: String,
    },
}
