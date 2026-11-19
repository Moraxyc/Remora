//! The owner's password, kept as an argon2id hash.

use std::fmt;

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};

/// The PHC string of an argon2id hash, such as `$argon2id$v=19$m=19456,t=2,p=1$…`.
#[derive(Clone)]
pub struct OwnerPassword(String);

impl OwnerPassword {
    /// Hashes `password` with a fresh salt and the default argon2id parameters.
    pub fn hash(password: &str) -> Self {
        let hash = Argon2::default()
            .hash_password(password.as_bytes())
            .expect("argon2 hashes any password with default parameters");
        Self(hash.to_string())
    }

    /// Takes a PHC string, which has to be an argon2id hash.
    pub fn parse(phc: &str) -> Result<Self, InvalidPasswordHash> {
        let hash = PasswordHash::new(phc).map_err(|_| InvalidPasswordHash)?;
        if hash.algorithm != argon2::ARGON2ID_IDENT {
            return Err(InvalidPasswordHash);
        }
        Ok(Self(phc.to_owned()))
    }

    /// Whether `password` is the one hashed. It takes as long as hashing, so call it off the
    /// async runtime.
    pub fn verify(&self, password: &str) -> bool {
        let hash = PasswordHash::new(&self.0).expect("checked when parsed");
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    }

    pub fn as_phc(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for OwnerPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OwnerPassword(..)")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidPasswordHash;

impl fmt::Display for InvalidPasswordHash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("expected an argon2id hash, as `remora hash-password` makes")
    }
}

impl std::error::Error for InvalidPasswordHash {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verifies_only_the_hashed_password() {
        let hash = OwnerPassword::hash("correct horse");
        assert!(hash.as_phc().starts_with("$argon2id$"));
        let parsed = OwnerPassword::parse(hash.as_phc()).unwrap();
        assert!(parsed.verify("correct horse"));
        assert!(!parsed.verify("correct horse "));
        assert!(!parsed.verify(""));
    }

    #[test]
    fn salts_every_hash() {
        assert_ne!(
            OwnerPassword::hash("same").as_phc(),
            OwnerPassword::hash("same").as_phc()
        );
    }

    #[test]
    fn accepts_only_argon2id() {
        for phc in [
            "",
            "hunter2",
            "$argon2i$v=19$m=16,t=2,p=1$c2FsdHNhbHQ$O0Mz2Ls9zJ7NnUoQxJbZbA",
        ] {
            assert_eq!(OwnerPassword::parse(phc).err(), Some(InvalidPasswordHash));
        }
    }

    #[test]
    fn debug_hides_the_hash() {
        assert_eq!(
            format!("{:?}", OwnerPassword::hash("x")),
            "OwnerPassword(..)"
        );
    }
}
