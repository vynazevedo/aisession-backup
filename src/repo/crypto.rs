use age::secrecy::ExposeSecret;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub(crate) fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner.finalize());
    outer.finalize().into()
}

pub(crate) struct Encryption {
    identity: age::x25519::Identity,
    recipient: age::x25519::Recipient,
}
impl Encryption {
    pub fn generate() -> Self {
        let identity = age::x25519::Identity::generate();
        let recipient = identity.to_public();
        Self {
            identity,
            recipient,
        }
    }
    pub fn recipient(&self) -> String {
        self.recipient.to_string()
    }
    pub fn seal_identity(&self, passphrase: &str) -> Result<String> {
        if passphrase.is_empty() {
            bail!("passphrase must not be empty")
        }
        let mut output = Vec::new();
        let encryptor = age::Encryptor::with_user_passphrase(passphrase.to_owned().into());
        let mut writer = encryptor.wrap_output(&mut output)?;
        writer.write_all(self.identity.to_string().expose_secret().as_bytes())?;
        writer.finish()?;
        Ok(hex::encode(output))
    }
    pub fn unlock(sealed: &str, recipient: &str, passphrase: &str) -> Result<Self> {
        let bytes = hex::decode(sealed).context("invalid encrypted repository identity")?;
        let decryptor =
            age::Decryptor::new(&bytes[..]).context("invalid encrypted repository identity")?;
        if !decryptor.is_scrypt() {
            bail!("repository identity is not passphrase encrypted")
        }
        let identity = age::scrypt::Identity::new(passphrase.to_owned().into());
        let reader = decryptor
            .decrypt(std::iter::once(&identity as &dyn age::Identity))
            .context("wrong passphrase or corrupt repository identity")?;
        let mut text = String::new();
        reader
            .take(1025)
            .read_to_string(&mut text)
            .context("wrong passphrase or corrupt repository identity")?;
        if text.len() > 1024 {
            bail!("repository identity too large")
        }
        let identity: age::x25519::Identity = text
            .parse()
            .map_err(|_| anyhow::anyhow!("invalid repository identity"))?;
        let encryption = Self {
            recipient: identity.to_public(),
            identity,
        };
        if encryption.recipient() != recipient {
            bail!("repository recipient does not match identity")
        }
        Ok(encryption)
    }
    fn mac_key(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"aisession-backup/snapshot-mac/v1");
        hash.update(self.identity.to_string().expose_secret().as_bytes());
        hash.finalize().into()
    }

    pub fn mac(&self, bytes: &[u8]) -> String {
        hex::encode(hmac_sha256(&self.mac_key(), bytes))
    }

    pub fn verify_mac(&self, bytes: &[u8], mac: &str) -> bool {
        let expected = self.mac(bytes);
        expected.len() == mac.len()
            && expected
                .bytes()
                .zip(mac.bytes())
                .fold(0u8, |diff, (a, b)| diff | (a ^ b))
                == 0
    }

    pub fn id(&self, bytes: &[u8]) -> String {
        let mut hash = Sha256::new();
        hash.update(self.identity.to_string().expose_secret().as_bytes());
        hash.update(bytes);
        hex::encode(hash.finalize())
    }
    pub fn encrypt(&self, bytes: &[u8]) -> Result<Vec<u8>> {
        let encryptor = age::Encryptor::with_recipients(std::iter::once(
            &self.recipient as &dyn age::Recipient,
        ))?;
        let mut output = Vec::new();
        let mut writer = encryptor.wrap_output(&mut output)?;
        writer.write_all(bytes)?;
        writer.finish()?;
        Ok(output)
    }
    pub fn decrypt(&self, bytes: &[u8], limit: usize) -> Result<Vec<u8>> {
        let reader = age::Decryptor::new(bytes)?
            .decrypt(std::iter::once(&self.identity as &dyn age::Identity))?;
        let mut output = Vec::new();
        reader.take(limit as u64 + 1).read_to_end(&mut output)?;
        if output.len() > limit {
            bail!("encrypted repository object exceeds size limit")
        }
        Ok(output)
    }
}
