//! SSH keys for reaching the instances in an environment.
//!
//! An environment gets its own keypair when it is created. The private half
//! never leaves this service except through the key endpoint, which only the
//! environment's owner can reach; the public half is registered with the Oxide
//! silo and attached to every instance the environment is made of, so the
//! instances come up reachable rather than needing a key added by hand
//! afterwards.

use ssh_key::{rand_core::OsRng, Algorithm, LineEnding, PrivateKey};
use vw_api_types_versions::latest::SshKeyPair;

/// What the key says it is for, so it is recognizable in `ssh-add -l` output
/// and in the Oxide silo's key list.
fn comment(user: &str, environment: &str) -> String {
    format!("vw {user}/{environment}")
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum KeyError {
    #[error("generating an ssh key failed")]
    Generate(#[source] ssh_key::Error),
    #[error("encoding an ssh key failed")]
    Encode(#[source] ssh_key::Error),
}

/// Generate a keypair for the environment `environment` owned by `user`.
///
/// Ed25519 rather than RSA: the keys are short, every ssh client in use
/// understands them, and there is no key size to get wrong.
pub(crate) fn generate(
    user: &str,
    environment: &str,
) -> Result<SshKeyPair, KeyError> {
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(KeyError::Generate)?;
    key.set_comment(comment(user, environment));

    Ok(SshKeyPair {
        // OpenSSH format, so it can be handed straight to `ssh -i` without
        // conversion.
        private_key: key
            .to_openssh(LineEnding::LF)
            .map_err(KeyError::Encode)?
            .to_string(),
        public_key: key.public_key().to_openssh().map_err(KeyError::Encode)?,
    })
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn a_generated_key_is_usable_by_ssh() {
        let pair = generate("ferris", "alpha").expect("generates");

        // `ssh -i` and the Oxide silo both want OpenSSH encoding, not PEM.
        assert!(pair
            .private_key
            .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert!(pair.private_key.ends_with('\n'));
        assert!(pair.public_key.starts_with("ssh-ed25519 "));

        // Round trip through the parser both halves will meet in the wild.
        let parsed =
            PrivateKey::from_openssh(&pair.private_key).expect("parses back");
        assert_eq!(parsed.algorithm(), Algorithm::Ed25519);
        assert_eq!(
            parsed.public_key().to_openssh().expect("encodes"),
            pair.public_key,
        );
    }

    #[test]
    fn the_comment_says_which_environment_it_opens() {
        let pair = generate("ferris", "alpha").expect("generates");
        assert!(
            pair.public_key.ends_with(" vw ferris/alpha"),
            "got {:?}",
            pair.public_key,
        );
    }

    #[test]
    fn every_environment_gets_its_own_key() {
        let one = generate("ferris", "alpha").expect("generates");
        let two = generate("ferris", "alpha").expect("generates");
        assert_ne!(one.private_key, two.private_key);
        assert_ne!(one.public_key, two.public_key);
    }
}
