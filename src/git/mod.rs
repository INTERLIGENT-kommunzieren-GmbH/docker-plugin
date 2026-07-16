use anyhow::{Context, Result, anyhow};
use git2::{
    BranchType, CertificateCheckStatus, Cred, FetchOptions, PushOptions, RemoteCallbacks,
    Repository,
};
use std::path::Path;

mod known_hosts;

pub struct GitService {
    repo: Repository,
}

impl GitService {
    pub fn from_repo(repo: Repository) -> Self {
        Self { repo }
    }

    pub fn repo(&self) -> &Repository {
        &self.repo
    }
    pub fn auth_callbacks() -> RemoteCallbacks<'static> {
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(|_url, username_from_url, _allowed_types| {
            let user: &str = username_from_url.unwrap_or("git");
            Cred::ssh_key_from_agent(user)
        });
        // libgit2 reads ~/.ssh/known_hosts itself, but git2's Rust binding discards its
        // verdict, so without a certificate_check callback any not-yet-trusted host is
        // rejected with GIT_ECERTIFICATE (-17). We reproduce OpenSSH's behaviour: accept
        // known/matching hosts, refuse changed keys, and prompt the user to trust unknown
        // hosts on first use (persisting the accepted key back to known_hosts).
        callbacks.certificate_check(Self::verify_host_key);
        callbacks
    }

    /// Verify a presented SSH host key against `~/.ssh/known_hosts`, prompting the
    /// user (trust-on-first-use) for hosts that are not yet known.
    fn verify_host_key(
        cert: &git2::cert::Cert,
        host: &str,
    ) -> Result<CertificateCheckStatus, git2::Error> {
        let Some(hostkey) = cert.as_hostkey() else {
            // Not an SSH host key (e.g. an X.509 cert) — defer to libgit2's own check.
            return Ok(CertificateCheckStatus::CertificatePassthrough);
        };
        let (Some(raw), Some(key_type)) = (hostkey.hostkey(), hostkey.hostkey_type()) else {
            return Ok(CertificateCheckStatus::CertificatePassthrough);
        };
        let type_name = key_type.name();

        match known_hosts::check(host, type_name, raw) {
            known_hosts::HostKeyStatus::Match => Ok(CertificateCheckStatus::CertificateOk),
            known_hosts::HostKeyStatus::Changed => {
                crate::ui::critical(format!(
                    "WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED for '{host}'! \
                     The {} host key does not match the one pinned in ~/.ssh/known_hosts. \
                     Refusing to connect.",
                    key_type.short_name()
                ));
                Err(git2::Error::from_str("remote host key verification failed"))
            }
            known_hosts::HostKeyStatus::Unknown => {
                let fingerprint = hostkey
                    .hash_sha256()
                    .map(|h| known_hosts::fingerprint_sha256(h))
                    .unwrap_or_else(|| "unavailable".to_string());
                crate::ui::warning(format!(
                    "The authenticity of host '{host}' can't be established."
                ));
                crate::ui::info(format!(
                    "{} key fingerprint is {fingerprint}.",
                    key_type.short_name()
                ));

                let accepted = inquire::Confirm::new(&format!(
                    "Add '{host}' to ~/.ssh/known_hosts and continue connecting?"
                ))
                .with_default(false)
                .prompt()
                .unwrap_or(false);

                if !accepted {
                    return Err(git2::Error::from_str("host key verification declined"));
                }
                match known_hosts::add(host, type_name, raw) {
                    Ok(path) => crate::ui::success(format!("Added '{host}' to {}", path.display())),
                    Err(e) => crate::ui::warning(format!("Could not update known_hosts: {e}")),
                }
                Ok(CertificateCheckStatus::CertificateOk)
            }
        }
    }
    pub fn open(path: &Path) -> Result<Self> {
        let repo = Repository::open(path)
            .context(format!("Failed to open git repository at {:?}", path))?;
        Ok(Self { repo })
    }

    pub fn get_current_branch(&self) -> Result<String> {
        let head = self.repo.head().context("Failed to get HEAD")?;
        let branch = head
            .shorthand()
            .map_err(|_| anyhow!("HEAD is not a branch"))?;
        Ok(branch.to_string())
    }

    pub fn list_release_branches(&self) -> Result<Vec<String>> {
        let mut branches = Vec::new();
        let local_branches = self.repo.branches(Some(BranchType::Local))?;

        for branch in local_branches {
            let (branch, _) = branch?;
            if let Some(name) = branch.name()?
                && is_release_branch_name(name)
            {
                branches.push(name.to_string());
            }
        }

        // Also check remote branches
        let remote_branches = self.repo.branches(Some(BranchType::Remote))?;
        for branch in remote_branches {
            let (branch, _) = branch?;
            if let Some(name) = branch.name()? {
                // Strip remote prefix (e.g., origin/)
                if let Some(short_name) = name.split('/').next_back()
                    && is_release_branch_name(short_name)
                    && !branches.contains(&short_name.to_string())
                {
                    branches.push(short_name.to_string());
                }
            }
        }

        branches.sort_by(|a, b| compare_versions(a, b));
        Ok(branches)
    }

    #[allow(dead_code)]
    pub fn create_branch(&self, name: &str, target: &str) -> Result<()> {
        let obj = self.repo.revparse_single(target)?;
        let commit = obj
            .as_commit()
            .ok_or_else(|| anyhow!("Target is not a commit"))?;
        self.repo.branch(name, commit, false)?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn checkout_branch(&self, name: &str) -> Result<()> {
        let obj = self.repo.revparse_single(name)?;
        self.repo.checkout_tree(&obj, None)?;
        self.repo.set_head(&format!("refs/heads/{}", name))?;
        Ok(())
    }

    pub fn get_commits_between_range(&self, range: &str) -> Result<Vec<(String, String)>> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_range(range)?;
        self.collect_commits(revwalk)
    }

    pub fn get_all_commits_from(&self, revision: &str) -> Result<Vec<(String, String)>> {
        let mut revwalk = self.repo.revwalk()?;
        let obj = self.repo.revparse_single(revision)?;
        revwalk.push(obj.id())?;
        self.collect_commits(revwalk)
    }

    pub fn list_tags(&self) -> Result<Vec<String>> {
        let mut tags = Vec::new();
        self.repo.tag_foreach(|_id, name| {
            if let Ok(name_str) = std::str::from_utf8(name) {
                // name is refs/tags/v1.0.0
                let short_name = name_str.strip_prefix("refs/tags/").unwrap_or(name_str);
                tags.push(short_name.to_string());
            }
            true
        })?;
        tags.sort_by(|a, b| compare_versions(a, b));
        Ok(tags)
    }

    pub fn get_merge_base(&self, one: &str, two: &str) -> Result<String> {
        let obj1 = self.repo.revparse_single(one)?;
        let obj2 = self.repo.revparse_single(two)?;
        let base = self.repo.merge_base(obj1.id(), obj2.id())?;
        Ok(base.to_string())
    }

    pub fn get_commits_between(&self, from: &str, to: &str) -> Result<Vec<(String, String)>> {
        let mut revwalk = self.repo.revwalk()?;
        revwalk.push_range(&format!("{}..{}", from, to))?;
        self.collect_commits(revwalk)
    }

    fn collect_commits(&self, mut revwalk: git2::Revwalk) -> Result<Vec<(String, String)>> {
        revwalk.set_sorting(git2::Sort::REVERSE)?;

        let mut commits = Vec::new();
        for id in revwalk {
            let id = id?;
            let commit = self.repo.find_commit(id)?;
            let summary = commit.summary().ok().flatten().unwrap_or("").to_string();

            // Filter out "release:" commits
            if !summary.starts_with("release:") {
                commits.push((id.to_string(), summary));
            }
        }
        Ok(commits)
    }

    #[allow(dead_code)]
    pub fn cherry_pick(&self, commit_hash: &str) -> Result<()> {
        let obj = self.repo.revparse_single(commit_hash)?;
        let commit = obj.as_commit().ok_or_else(|| anyhow!("Not a commit"))?;

        // Using git2 cherry-pick is complex for handling index/workdir
        // For simplicity and matching bash behavior, we use system git if available,
        // or attempt git2 if we want to stay pure.
        // Given the requirement for interactive conflict resolution in merge.rs,
        // we'll likely use Command in merge.rs, but let's provide a basic git2 version here.
        self.repo.cherrypick(commit, None)?;

        let index = self.repo.index()?;
        if index.has_conflicts() {
            return Err(anyhow!(
                "Cherry-pick resulted in conflicts for commit {}",
                commit_hash
            ));
        }

        // If no conflicts, we need to commit the changes
        // This is simplified; real implementation would need more logic
        Ok(())
    }

    #[allow(dead_code)]
    pub fn create_tag(&self, name: &str, message: &str) -> Result<()> {
        let head = self.repo.head()?.peel_to_commit()?;
        let sig = self.repo.signature()?;
        self.repo
            .tag(name, head.as_object(), &sig, message, false)?;
        Ok(())
    }

    pub fn get_primary_branch(&self) -> Result<String> {
        if self.repo.find_branch("main", BranchType::Local).is_ok() {
            Ok("main".to_string())
        } else if self.repo.find_branch("master", BranchType::Local).is_ok() {
            Ok("master".to_string())
        } else {
            // Check remote
            if self
                .repo
                .find_branch("origin/main", BranchType::Remote)
                .is_ok()
            {
                Ok("main".to_string())
            } else if self
                .repo
                .find_branch("origin/master", BranchType::Remote)
                .is_ok()
            {
                Ok("master".to_string())
            } else {
                Err(anyhow!("Could not determine primary branch (main/master)"))
            }
        }
    }

    pub fn fetch_all(&self) -> anyhow::Result<()> {
        let mut remote = self.repo.find_remote("origin")?;

        let callbacks = Self::auth_callbacks();

        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);

        remote.fetch(
            &["+refs/heads/*:refs/remotes/origin/*"],
            Some(&mut fetch_options),
            None,
        )?;

        Ok(())
    }

    pub fn fetch_tags(&self) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let callbacks = Self::auth_callbacks();
        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);
        remote.fetch(&["refs/tags/*:refs/tags/*"], Some(&mut fetch_options), None)?;
        Ok(())
    }

    pub fn add_file(&self, path: &Path) -> Result<()> {
        let mut index = self.repo.index()?;
        index.add_path(path)?;
        index.write()?;
        Ok(())
    }

    pub fn commit(&self, message: &str) -> Result<()> {
        let mut index = self.repo.index()?;
        let tree_id = index.write_tree()?;
        let tree = self.repo.find_tree(tree_id)?;
        let sig = self.repo.signature()?;
        let parent = self.repo.head()?.peel_to_commit()?;
        self.repo
            .commit(Some("HEAD"), &sig, &sig, message, &tree, &[&parent])?;
        Ok(())
    }

    pub fn create_tag_on_head(&self, name: &str, message: &str) -> Result<()> {
        let head = self.repo.head()?.peel_to_commit()?;
        let sig = self.repo.signature()?;
        self.repo
            .tag(name, head.as_object(), &sig, message, false)?;
        Ok(())
    }

    pub fn update_branch(&self, branch_name: &str) -> Result<()> {
        // If branch doesn't exist locally, create it from origin
        if self
            .repo
            .find_branch(branch_name, BranchType::Local)
            .is_err()
        {
            if let Ok(remote_branch) = self
                .repo
                .find_branch(&format!("origin/{}", branch_name), BranchType::Remote)
            {
                let target_commit = remote_branch.get().peel_to_commit()?;
                self.repo.branch(branch_name, &target_commit, false)?;
            } else {
                return Err(anyhow!(
                    "Branch {} not found locally or on origin",
                    branch_name
                ));
            }
        }

        // Fast-forward local branch to match origin
        let mut local_branch = self.repo.find_branch(branch_name, BranchType::Local)?;
        let remote_branch = self
            .repo
            .find_branch(&format!("origin/{}", branch_name), BranchType::Remote)?;
        let remote_target = remote_branch.get().peel_to_commit()?;

        let ref_ = local_branch.get_mut();
        ref_.set_target(remote_target.id(), "Fast-forwarding to origin")?;

        Ok(())
    }

    pub fn list_vendor_modules(project_dir: &Path) -> Result<Vec<String>> {
        let vendor_dir = project_dir.join("htdocs/vendor");
        if !vendor_dir.exists() {
            return Ok(Vec::new());
        }

        let mut modules = Vec::new();
        // Vendor modules are usually in htdocs/vendor/vendor-name/module-name
        // We look for .git directories in subdirectories
        for entry in std::fs::read_dir(&vendor_dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                for sub_entry in std::fs::read_dir(&path)? {
                    let sub_entry = sub_entry?;
                    let sub_path = sub_entry.path();
                    if sub_path.is_dir()
                        && sub_path.join(".git").exists()
                        && let (Some(vendor), Some(module)) =
                            (path.file_name(), sub_path.file_name())
                    {
                        modules.push(format!(
                            "{}/{}",
                            vendor.to_string_lossy(),
                            module.to_string_lossy()
                        ));
                    }
                }
            }
        }
        modules.sort();
        Ok(modules)
    }

    pub fn get_changelog(&self, release: &str) -> String {
        let obj = match self.repo.revparse_single(release) {
            Ok(o) => o,
            Err(_) => return format!("No changelog available for release {}", release),
        };
        let commit = match obj.peel_to_commit() {
            Ok(c) => c,
            Err(_) => return format!("No changelog available for release {}", release),
        };
        let tree = match commit.tree() {
            Ok(t) => t,
            Err(_) => return format!("No changelog available for release {}", release),
        };

        for filename in &["CHANGELOG.md", "changelog.md", "CHANGELOG"] {
            if let Ok(entry) = tree.get_path(Path::new(filename))
                && let Ok(blob) = self.repo.find_blob(entry.id())
            {
                let content = String::from_utf8_lossy(blob.content());
                return content.lines().take(20).collect::<Vec<_>>().join("\n");
            }
        }

        format!("No changelog available for release {}", release)
    }

    pub fn create_worktree(&self, name: &str, path: &Path, branch: Option<&str>) -> Result<()> {
        let mut opts = git2::WorktreeAddOptions::new();

        let branch_obj = if let Some(b) = branch {
            if name == b {
                // We want to track an existing branch or create one with this name
                if let Ok(local_branch) = self.repo.find_branch(b, BranchType::Local) {
                    Some(local_branch)
                } else {
                    self.repo
                        .find_branch(&format!("origin/{}", b), BranchType::Remote)
                        .ok()
                }
            } else {
                // We want to create a new branch 'name' based on 'b'
                let obj = self.repo.revparse_single(b)?;
                let commit = obj
                    .as_commit()
                    .ok_or_else(|| anyhow!("Target {} is not a commit", b))?;

                // Create the branch in main repo first
                let new_branch = self.repo.branch(name, commit, false)?;
                Some(new_branch)
            }
        } else {
            None
        };

        if let Some(ref b) = branch_obj {
            opts.reference(Some(b.get()));
        }

        let _wt = self.repo.worktree(name, path, Some(&opts))?;

        if let Some(b) = branch {
            let target_branch = if name == b { b } else { name };
            let wt_repo = Repository::open(path)?;

            // Check if branch exists locally
            if let Ok(local_branch) = wt_repo.find_branch(target_branch, BranchType::Local) {
                let commit = local_branch.get().peel_to_commit()?;
                wt_repo.set_head(local_branch.get().name().unwrap())?;
                wt_repo.checkout_tree(commit.as_object(), None)?;
            }
        }
        Ok(())
    }

    pub fn push_branch(&self, branch_name: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let refspec = format!("refs/heads/{}:refs/heads/{}", branch_name, branch_name);
        let callbacks = Self::auth_callbacks();
        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);
        remote.push(&[&refspec], Some(&mut push_options))?;
        Ok(())
    }

    pub fn push_tag(&self, tag_name: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let refspec = format!("refs/tags/{}:refs/tags/{}", tag_name, tag_name);
        let callbacks = Self::auth_callbacks();
        let mut push_options = PushOptions::new();
        push_options.remote_callbacks(callbacks);
        remote.push(&[&refspec], Some(&mut push_options))?;
        Ok(())
    }

    pub fn pull(&self, branch_name: &str) -> Result<()> {
        let mut remote = self.repo.find_remote("origin")?;
        let callbacks = Self::auth_callbacks();
        let mut fetch_options = FetchOptions::new();
        fetch_options.remote_callbacks(callbacks);
        remote.fetch(&[branch_name], Some(&mut fetch_options), None)?;

        let fetch_head = self.repo.find_reference("FETCH_HEAD")?;
        let fetch_commit = fetch_head.peel_to_commit()?;

        let mut checkout_builder = git2::build::CheckoutBuilder::new();
        checkout_builder.force();

        self.repo
            .checkout_tree(fetch_commit.as_object(), Some(&mut checkout_builder))?;
        self.repo.set_head(&format!("refs/heads/{}", branch_name))?;

        Ok(())
    }

    pub fn has_branch(&self, name: &str) -> Result<bool> {
        Ok(self.repo.find_branch(name, BranchType::Local).is_ok())
    }

    pub fn delete_branch(&self, name: &str) -> Result<()> {
        let mut branch = self.repo.find_branch(name, BranchType::Local)?;
        branch.delete()?;
        Ok(())
    }
}

pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let a_clean = a.strip_prefix('v').unwrap_or(a);
    let b_clean = b.strip_prefix('v').unwrap_or(b);

    let a_parts: Vec<&str> = a_clean.split('.').collect();
    let b_parts: Vec<&str> = b_clean.split('.').collect();

    for i in 0..std::cmp::min(a_parts.len(), b_parts.len()) {
        let a_num = a_parts[i].parse::<u32>();
        let b_num = b_parts[i].parse::<u32>();

        match (a_num, b_num) {
            (Ok(an), Ok(bn)) => {
                if an != bn {
                    return an.cmp(&bn);
                }
            }
            _ => {
                if a_parts[i] != b_parts[i] {
                    return a_parts[i].cmp(b_parts[i]);
                }
            }
        }
    }

    a_parts.len().cmp(&b_parts.len())
}

pub fn is_release_branch_name(name: &str) -> bool {
    // Matches x.y.x or v.x.y.x pattern
    let clean_name = name.strip_prefix('v').unwrap_or(name);
    let parts: Vec<&str> = clean_name.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    parts[0].chars().all(|c| c.is_ascii_digit())
        && parts[1].chars().all(|c| c.is_ascii_digit())
        && parts[2] == "x"
}

pub fn get_docker_user_id() -> String {
    #[cfg(unix)]
    {
        format!("{}:{}", unsafe { libc::getuid() }, unsafe {
            libc::getgid()
        })
    }
    #[cfg(not(unix))]
    {
        "1000:1000".to_string() // Fallback for non-unix
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CleanupMode {
    Always,
    Keep,
}

pub struct WorktreeCleanup<'a> {
    git_path: &'a Path,
    worktree_path: std::path::PathBuf,
    active: bool,
}

impl<'a> WorktreeCleanup<'a> {
    pub fn new(git_path: &'a Path, worktree_path: std::path::PathBuf, mode: CleanupMode) -> Self {
        Self {
            git_path,
            worktree_path,
            active: matches!(mode, CleanupMode::Always),
        }
    }

    pub fn cancel(&mut self) {
        self.active = false;
    }
}

impl<'a> Drop for WorktreeCleanup<'a> {
    fn drop(&mut self) {
        if self.active && self.worktree_path.exists() {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(self.git_path)
                .arg("worktree")
                .arg("remove")
                .arg("--force")
                .arg(&self.worktree_path)
                .status();
        }
    }
}
