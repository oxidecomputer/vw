//! Putting the caller's Github credentials where a build will find them.
//!
//! Sources arrive from a developer's machine, but dependencies do not — the
//! instance fetches those itself, and needs credentials to do it. A `.netrc`
//! is how those get supplied because git, cargo and curl all already read one;
//! nothing on the instance has to be taught anything new.

use camino::{Utf8Path, Utf8PathBuf};
use vw_api_types_versions::latest::Credentials;

/// The hosts a build fetches from.
///
/// `api.github.com` as well as `github.com` because a dependency fetched
/// through the API — a release artifact, a tarball of a tag — goes to the
/// other host and would otherwise be an anonymous request against a private
/// repository.
const HOSTS: [&str; 2] = ["github.com", "api.github.com"];

#[derive(Debug, thiserror::Error)]
pub(crate) enum NetrcError {
    #[error("creating {0}")]
    CreateDir(Utf8PathBuf, #[source] std::io::Error),
    #[error("writing {0}")]
    Write(Utf8PathBuf, #[source] std::io::Error),
    #[error("reading the owner of {0}")]
    Owner(Utf8PathBuf, #[source] std::io::Error),
}

/// Write `credentials` to the netrc at `path`.
///
/// Replaces whatever was there. A token is reissued or rotated without any
/// ceremony on the instance's part, and a netrc that has fallen behind the
/// caller's real credentials is worth nothing.
pub(crate) fn write(
    path: &Utf8Path,
    credentials: &Credentials,
) -> Result<(), NetrcError> {
    let mut contents = String::new();
    for host in HOSTS {
        contents.push_str(&format!(
            "machine {host}\n  login {}\n  password {}\n",
            credentials.user, credentials.token,
        ));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| NetrcError::CreateDir(parent.to_owned(), e))?;
    }

    write_private(path, contents.as_bytes())?;
    take_ownership_from_parent(path)?;

    Ok(())
}

/// Write `contents` to `path` so that only its owner can ever read it.
///
/// The permissions are set as the file is created rather than after it is
/// written. Creating it first and fixing the mode afterwards would leave a
/// window — brief, but on a machine several people can reach — in which a live
/// credential sits in a world readable file.
#[cfg(unix)]
fn write_private(path: &Utf8Path, contents: &[u8]) -> Result<(), NetrcError> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    // Removed rather than truncated, so the mode below applies to a file this
    // call created. An existing netrc might have any permissions at all.
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(NetrcError::Write(path.to_owned(), e)),
    }

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| NetrcError::Write(path.to_owned(), e))?;

    file.write_all(contents)
        .map_err(|e| NetrcError::Write(path.to_owned(), e))
}

#[cfg(not(unix))]
fn write_private(path: &Utf8Path, contents: &[u8]) -> Result<(), NetrcError> {
    std::fs::write(path, contents)
        .map_err(|e| NetrcError::Write(path.to_owned(), e))
}

/// Give the netrc the same owner as the directory holding it.
///
/// The agent runs as root, but a build does not: on the vivado and artifact
/// instances it runs as `ubuntu`. A root owned netrc in ubuntu's home is
/// readable by nobody except root — which is to say, unreadable by exactly the
/// process it exists for. Taking ownership from the enclosing directory gets
/// this right on helios too, where both are root and nothing changes.
#[cfg(unix)]
fn take_ownership_from_parent(path: &Utf8Path) -> Result<(), NetrcError> {
    use std::os::unix::fs::MetadataExt;

    let Some(parent) = path.parent() else {
        return Ok(());
    };

    let owner = std::fs::metadata(parent)
        .map_err(|e| NetrcError::Owner(parent.to_owned(), e))?;
    let (uid, gid) = (owner.uid(), owner.gid());

    let current = std::fs::metadata(path)
        .map_err(|e| NetrcError::Owner(path.to_owned(), e))?;
    if current.uid() == uid && current.gid() == gid {
        return Ok(());
    }

    // Only root may hand a file to somebody else. An agent running as an
    // ordinary user in a directory it does not own cannot fix this, and
    // failing the whole request over it would be worse than writing a netrc
    // that the intended reader may still be able to use.
    std::os::unix::fs::chown(path, Some(uid), Some(gid))
        .map_err(|e| NetrcError::Owner(path.to_owned(), e))
}

#[cfg(not(unix))]
fn take_ownership_from_parent(_path: &Utf8Path) -> Result<(), NetrcError> {
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    fn credentials() -> Credentials {
        Credentials {
            user: "picard".to_owned(),
            token: "ghp_darmokandjalad".to_owned(),
        }
    }

    fn scratch() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::TempDir::new().expect("scratch");
        let root = camino::Utf8Path::from_path(dir.path())
            .expect("utf8")
            .to_owned();
        (dir, root)
    }

    #[test]
    fn a_netrc_names_both_github_hosts() {
        let (_dir, root) = scratch();
        let path = root.join(".netrc");

        write(&path, &credentials()).expect("write netrc");

        let contents = std::fs::read_to_string(&path).expect("read netrc");
        assert_eq!(
            contents,
            "machine github.com\n  login picard\n  password \
             ghp_darmokandjalad\nmachine api.github.com\n  login picard\n  \
             password ghp_darmokandjalad\n",
        );
    }

    #[cfg(unix)]
    #[test]
    fn nobody_but_the_owner_can_read_a_netrc() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, root) = scratch();
        let path = root.join(".netrc");

        write(&path, &credentials()).expect("write netrc");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[cfg(unix)]
    #[test]
    fn a_permissive_netrc_already_there_does_not_stay_permissive() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, root) = scratch();
        let path = root.join(".netrc");
        std::fs::write(&path, "old").expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644))
            .expect("chmod");

        write(&path, &credentials()).expect("write netrc");

        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "mode was {:o}", mode & 0o777);
    }

    #[test]
    fn a_rotated_token_replaces_the_old_one() {
        let (_dir, root) = scratch();
        let path = root.join(".netrc");
        write(&path, &credentials()).expect("first");

        write(
            &path,
            &Credentials {
                user: "picard".to_owned(),
                token: "ghp_temba".to_owned(),
            },
        )
        .expect("second");

        let contents = std::fs::read_to_string(&path).expect("read netrc");
        assert!(contents.contains("ghp_temba"));
        assert!(
            !contents.contains("ghp_darmokandjalad"),
            "the old token is still there: {contents}",
        );
    }

    #[test]
    fn a_home_directory_that_is_not_there_yet_is_created() {
        let (_dir, root) = scratch();
        let path = root.join("home/ubuntu/.netrc");

        write(&path, &credentials()).expect("write netrc");

        assert!(path.is_file());
    }

    #[test]
    fn a_token_is_not_in_the_debug_output() {
        // Everything else here is careful about the file; this is about the
        // other way a credential escapes, which is a log line.
        let printed = format!("{:?}", credentials());

        assert!(!printed.contains("ghp_darmokandjalad"), "{printed}");
        assert!(printed.contains("picard"), "{printed}");
    }
}
