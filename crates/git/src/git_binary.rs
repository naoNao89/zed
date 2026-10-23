use std::path::{Path, PathBuf};

use anyhow::{Result, bail};

/// A validated git binary choice: the resolved path plus where it came from
/// and why, for logging.
pub struct SelectedGitBinary {
    pub path: PathBuf,
    pub source: &'static str,
    pub reason: &'static str,
}

fn helper_exists_in_dir(dir: &Path) -> bool {
    #[cfg(windows)]
    const NAMES: [&str; 2] = ["git-remote-https.exe", "git-remote-https"];
    #[cfg(not(windows))]
    const NAMES: [&str; 1] = ["git-remote-https"];

    NAMES.iter().any(|name| {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            match std::fs::metadata(&candidate) {
                Ok(metadata) => metadata.permissions().mode() & 0o111 != 0,
                Err(_) => false,
            }
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

/// Returns true when the git binary at `path` ships a usable `remote-https`
/// helper, which remote operations (`fetch`/`pull`/`push` over HTTP(S))
/// require. Without it git fails with `git: 'remote-https' is not a git
/// command` (see issue #57862).
///
/// The check is fully local (no network): it respects a `GIT_EXEC_PATH`
/// override and otherwise asks git itself via `git --exec-path`, then looks
/// for the `git-remote-https` helper file in that directory.
///
/// Returns false on a missing binary, spawn failure, non-zero exit, or a
/// missing/non-executable helper. It MUST NOT interpret auth, network, or
/// any other non-zero exit of a launched remote git command: only the
/// conditions above may trigger a fallback to another binary.
    // Local `--exec-path` probe only (no repo, no network): bypasses
    // `GitBinary::build_command` security flags intentionally; needs a
    // blocking spawn in this sync selector.
    #[allow(clippy::disallowed_methods)]
pub fn has_remote_https_helper(path: &Path) -> bool {
    if let Ok(exec_path_override) = std::env::var("GIT_EXEC_PATH") {
        if !exec_path_override.trim().is_empty()
            && helper_exists_in_dir(Path::new(exec_path_override.trim()))
        {
            return true;
        }
    }
    let output = std::process::Command::new(path).arg("--exec-path").output();
    let output = match output {
        Ok(output) => output,
        Err(_) => return false,
    };
    if !output.status.success() {
        return false;
    }
    let dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if dir.is_empty() {
        return false;
    }
    helper_exists_in_dir(Path::new(&dir))
}

/// Selects the git binary to use, preferring `system` over `bundled`.
///
/// Each candidate is validated with [`has_remote_https_helper`]: a candidate
/// is usable only when its binary exists, spawns, and ships the
/// `remote-https` helper. Falls back from system to bundled ONLY on a missing
/// binary, spawn failure, or missing helper — never on auth/network failures
/// or any other non-zero exit of a launched git command.
pub fn select_git_binary(system: Option<PathBuf>, bundled: Option<PathBuf>) -> Result<PathBuf> {
    select_git_binary_with_info(system, bundled).map(|selected| selected.path)
}

/// [`select_git_binary`] plus the selection's provenance for logging.
/// `source` is `system` or `bundled`; `reason` is `validated` when the first
/// choice was taken and `fallback-missing-helper` when falling back because
/// the system binary was missing or lacked the helper.
pub fn select_git_binary_with_info(
    system: Option<PathBuf>,
    bundled: Option<PathBuf>,
) -> Result<SelectedGitBinary> {
    if let Some(system_path) = system.clone() {
        if has_remote_https_helper(&system_path) {
            return Ok(SelectedGitBinary {
                path: system_path,
                source: "system",
                reason: "validated",
            });
        }
    }
    if let Some(bundled_path) = bundled.clone() {
        if has_remote_https_helper(&bundled_path) {
            let reason = if system.is_some() {
                "fallback-missing-helper"
            } else {
                "validated"
            };
            return Ok(SelectedGitBinary {
                path: bundled_path,
                source: "bundled",
                reason,
            });
        }
    }
    bail!(
        "no usable git binary found (tried system={system:?}, bundled={bundled:?}): \
        each candidate is missing, failed to spawn, or lacks the `remote-https` helper \
        (git: 'remote-https' is not a git command). Install a git with HTTPS support \
        or bundle one."
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Writes a fake `git` executable that answers `--exec-path` with
    /// `exec_dir`, optionally containing a `git-remote-https` helper.
    fn fake_git(dir: &tempfile::TempDir, name: &str, with_helper: bool) -> PathBuf {
        let exec_dir = dir.path().join(format!("{name}-exec"));
        std::fs::create_dir_all(&exec_dir).unwrap();
        if with_helper {
            let helper = exec_dir.join("git-remote-https");
            std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                let mut permissions = std::fs::metadata(&helper).unwrap().permissions();
                permissions.set_mode(0o755);
                std::fs::set_permissions(&helper, permissions).unwrap();
            }
        }
        let git_path = dir.path().join(name);
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = \"--exec-path\" ]; then echo \"{}\"; exit 0; fi\nexit 1\n",
            exec_dir.display()
        );
        let mut file = std::fs::File::create(&git_path).unwrap();
        file.write_all(script.as_bytes()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mut permissions = std::fs::metadata(&git_path).unwrap().permissions();
            permissions.set_mode(0o755);
            std::fs::set_permissions(&git_path, permissions).unwrap();
        }
        git_path
    }

    #[test]
    fn test_select_prefers_system_when_valid() {
        let dir = tempfile::tempdir().unwrap();
        let system = fake_git(&dir, "system-git", true);
        let bundled = fake_git(&dir, "bundled-git", true);
        let selected = select_git_binary_with_info(Some(system.clone()), Some(bundled)).unwrap();
        assert_eq!(selected.path, system);
        assert_eq!(selected.source, "system");
        assert_eq!(selected.reason, "validated");
    }

    #[test]
    fn test_select_falls_back_to_bundled_when_system_lacks_helper() {
        let dir = tempfile::tempdir().unwrap();
        let system = fake_git(&dir, "system-git", false);
        let bundled = fake_git(&dir, "bundled-git", true);
        let selected = select_git_binary_with_info(Some(system), Some(bundled.clone())).unwrap();
        assert_eq!(selected.path, bundled);
        assert_eq!(selected.source, "bundled");
        assert_eq!(selected.reason, "fallback-missing-helper");
    }

    #[test]
    fn test_select_falls_back_to_bundled_when_system_missing() {
        let dir = tempfile::tempdir().unwrap();
        let bundled = fake_git(&dir, "bundled-git", true);
        let missing: PathBuf = dir.path().join("does-not-exist-git");
        let selected = select_git_binary_with_info(Some(missing), Some(bundled.clone())).unwrap();
        assert_eq!(selected.path, bundled);
        assert_eq!(selected.source, "bundled");
    }

    #[test]
    fn test_select_bails_when_neither_usable() {
        let dir = tempfile::tempdir().unwrap();
        let system = fake_git(&dir, "system-git", false);
        let err = select_git_binary(Some(system), None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no usable git binary"), "{message}");
        assert!(message.contains("remote-https"), "{message}");
    }

    #[test]
    fn test_select_bails_when_both_missing() {
        let err = select_git_binary(None, None).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("no usable git binary"), "{message}");
        assert!(message.contains("remote-https"), "{message}");
        assert!(message.contains("tried system=None"), "{message}");
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn test_select_does_not_fall_back_on_remote_command_failure() {
        // `fake_git` exits non-zero (like an auth/network failure) for every
        // command except `--exec-path`, which is the only probe selection uses.
        // A usable helper means the system binary stays selected; remote
        // failures must propagate from the caller, never trigger a fallback.
        let dir = tempfile::tempdir().unwrap();
        let system = fake_git(&dir, "system-git", true);
        let bundled = fake_git(&dir, "bundled-git", true);

        // Sanity: both candidates look usable to the selector ...
        assert!(has_remote_https_helper(&system));
        assert!(has_remote_https_helper(&bundled));

        // ... but every other invocation fails, simulating e.g. auth rejection.
        let status = std::process::Command::new(&system)
            .arg("ls-remote")
            .arg("https://example.com/repo.git")
            .status()
            .unwrap();
        assert!(!status.success());

        // Selection still pins system; no fallback to bundled.
        let selected = select_git_binary_with_info(Some(system.clone()), Some(bundled)).unwrap();
        assert_eq!(selected.path, system);
        assert_eq!(selected.source, "system");
        assert_eq!(selected.reason, "validated");
    }

    #[test]
    fn test_has_remote_https_helper_rejects_missing_binary() {
        assert!(!has_remote_https_helper(Path::new(
            "/definitely/not/a/real/git-binary-for-test"
        )));
    }
}
