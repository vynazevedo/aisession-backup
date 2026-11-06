use crate::engine::Passphrase;
use anyhow::{bail, Result};
use std::io::{Read, Write};

pub const MAGIC: &[u8] = b"age-encryption.org/v1";

pub fn encrypt<R: Read, W: Write>(mut input: R, output: W, passphrase: &Passphrase) -> Result<()> {
    let Passphrase::Value(value) = passphrase else {
        bail!("encryption requires a passphrase")
    };
    if value.is_empty() {
        bail!("empty passphrase")
    }
    let encryptor = age::Encryptor::with_user_passphrase(value.clone().into());
    let mut writer = encryptor.wrap_output(output)?;
    std::io::copy(&mut input, &mut writer)?;
    writer.finish()?;
    Ok(())
}

pub fn decrypt<R: Read, W: Write>(input: R, mut output: W, passphrase: &Passphrase) -> Result<()> {
    let Passphrase::Value(value) = passphrase else {
        bail!("encrypted archive requires a passphrase")
    };
    let decryptor = age::Decryptor::new(input)?;
    if !decryptor.is_scrypt() {
        bail!("archive is not passphrase encrypted")
    }
    let identity = age::scrypt::Identity::new(value.clone().into());
    let reader = decryptor.decrypt(std::iter::once(&identity as &dyn age::Identity))?;
    let limit = crate::archive::total_limit()?;
    let mut reader = reader.take(limit + 1);
    if std::io::copy(&mut reader, &mut output)? > limit {
        bail!("encrypted archive exceeds maximum total size")
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_roundtrip_and_wrong_password() {
        let key = Passphrase::Value("synthetic passphrase".into());
        let mut encrypted = Vec::new();
        encrypt(&b"synthetic payload"[..], &mut encrypted, &key).unwrap();
        assert!(encrypted.starts_with(MAGIC));
        let mut plain = Vec::new();
        decrypt(&encrypted[..], &mut plain, &key).unwrap();
        assert_eq!(plain, b"synthetic payload");
        assert!(decrypt(
            &encrypted[..],
            Vec::new(),
            &Passphrase::Value("wrong".into())
        )
        .is_err());
    }
}
