use std::time::Duration;

use anyhow::{bail, Context, Result};
use rusty_s3::actions::ListObjectsV2;
use rusty_s3::{Bucket, Credentials, S3Action, UrlStyle};

use crate::repo::Backend;

const SIGNATURE_TTL: Duration = Duration::from_secs(600);
const MAX_OBJECT_BYTES: u64 = 1024 * 1024 * 1024;
const MAX_LIST_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct S3Location {
    pub bucket: String,
    pub prefix: String,
}

pub fn is_remote(location: &str) -> bool {
    location.starts_with("s3://")
}

pub fn parse_location(raw: &str) -> Result<S3Location> {
    let rest = raw
        .strip_prefix("s3://")
        .context("remote repositories use s3://bucket/prefix")?;
    let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
    let valid_bucket = (3..=63).contains(&bucket.len())
        && bucket
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.')
        && !bucket.starts_with(['-', '.'])
        && !bucket.ends_with(['-', '.']);
    if !valid_bucket {
        bail!("invalid bucket name in {raw:?}");
    }
    let prefix = prefix.trim_matches('/').to_string();
    if !prefix.is_empty() {
        crate::repo::backend::validate_key(&prefix)
            .with_context(|| format!("invalid prefix in {raw:?}"))?;
    }
    Ok(S3Location {
        bucket: bucket.to_string(),
        prefix,
    })
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

fn is_loopback(url: &url::Url) -> bool {
    matches!(
        url.host_str(),
        Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
    )
}

pub struct S3Backend {
    bucket: Bucket,
    credentials: Credentials,
    prefix: String,
    agent: ureq::Agent,
}

impl S3Backend {
    pub fn from_env(location: &str) -> Result<Self> {
        let parsed = parse_location(location)?;
        let region = env_first(&[
            "AISESSION_BACKUP_S3_REGION",
            "AWS_REGION",
            "AWS_DEFAULT_REGION",
        ])
        .unwrap_or_else(|| "us-east-1".to_string());
        let custom = env_first(&["AISESSION_BACKUP_S3_ENDPOINT", "AWS_ENDPOINT_URL"]);
        let (endpoint, style) = match custom {
            Some(endpoint) => (endpoint, UrlStyle::Path),
            None => (
                format!("https://s3.{region}.amazonaws.com"),
                UrlStyle::VirtualHost,
            ),
        };
        let endpoint: url::Url = endpoint.parse().context("invalid S3 endpoint URL")?;
        if endpoint.scheme() != "https" && !is_loopback(&endpoint) {
            bail!("the S3 endpoint must use https, except for localhost");
        }
        let key = env_first(&["AWS_ACCESS_KEY_ID"]).context("AWS_ACCESS_KEY_ID is not set")?;
        let secret =
            env_first(&["AWS_SECRET_ACCESS_KEY"]).context("AWS_SECRET_ACCESS_KEY is not set")?;
        let credentials = match env_first(&["AWS_SESSION_TOKEN"]) {
            Some(token) => Credentials::new_with_token(key, secret, token),
            None => Credentials::new(key, secret),
        };
        Self::new(endpoint, style, parsed, region, credentials)
    }

    pub fn new(
        endpoint: url::Url,
        style: UrlStyle,
        location: S3Location,
        region: String,
        credentials: Credentials,
    ) -> Result<Self> {
        let bucket = Bucket::new(endpoint, style, location.bucket, region)
            .map_err(|err| anyhow::anyhow!("invalid S3 bucket: {err}"))?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(300)))
            .build()
            .into();
        Ok(Self {
            bucket,
            credentials,
            prefix: location.prefix,
            agent,
        })
    }

    fn object(&self, key: &str) -> Result<String> {
        crate::repo::backend::validate_key(key)?;
        Ok(if self.prefix.is_empty() {
            key.to_string()
        } else {
            format!("{}/{key}", self.prefix)
        })
    }

    fn read_body(response: &mut ureq::http::Response<ureq::Body>, limit: u64) -> Result<Vec<u8>> {
        Ok(response
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()?)
    }

    fn failure(
        action: &str,
        key: &str,
        response: &mut ureq::http::Response<ureq::Body>,
    ) -> anyhow::Error {
        let status = response.status().as_u16();
        let body = Self::read_body(response, 4096).unwrap_or_default();
        let text = String::from_utf8_lossy(&body);
        let code = text
            .split("<Code>")
            .nth(1)
            .and_then(|rest| rest.split("</Code>").next())
            .unwrap_or("")
            .to_string();
        anyhow::anyhow!(
            "S3 {action} {} failed with HTTP {status} {}",
            crate::ui::sanitize(key),
            crate::ui::sanitize(&code)
        )
    }

    pub fn create_bucket(&self) -> Result<()> {
        let url = self
            .bucket
            .create_bucket(&self.credentials)
            .sign(SIGNATURE_TTL);
        let mut response = self.agent.put(url.as_str()).send_empty()?;
        match response.status().as_u16() {
            200 | 409 => Ok(()),
            _ => Err(Self::failure(
                "create bucket",
                self.bucket.name(),
                &mut response,
            )),
        }
    }
}

impl Backend for S3Backend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let object = self.object(key)?;
        let url = self
            .bucket
            .get_object(Some(&self.credentials), &object)
            .sign(SIGNATURE_TTL);
        let mut response = self.agent.get(url.as_str()).call()?;
        match response.status().as_u16() {
            200 => Ok(Some(Self::read_body(&mut response, MAX_OBJECT_BYTES)?)),
            404 => Ok(None),
            _ => Err(Self::failure("get", key, &mut response)),
        }
    }

    fn put(&self, key: &str, bytes: &[u8]) -> Result<()> {
        let object = self.object(key)?;
        let url = self
            .bucket
            .put_object(Some(&self.credentials), &object)
            .sign(SIGNATURE_TTL);
        let mut response = self.agent.put(url.as_str()).send(bytes)?;
        match response.status().as_u16() {
            200 => Ok(()),
            _ => Err(Self::failure("put", key, &mut response)),
        }
    }

    fn put_if_absent(&self, key: &str, bytes: &[u8]) -> Result<bool> {
        let object = self.object(key)?;
        let url = self
            .bucket
            .put_object(Some(&self.credentials), &object)
            .sign(SIGNATURE_TTL);
        let mut response = self
            .agent
            .put(url.as_str())
            .header("If-None-Match", "*")
            .send(bytes)?;
        match response.status().as_u16() {
            200 => Ok(true),
            412 | 409 => Ok(false),
            _ => Err(Self::failure("conditional put", key, &mut response)),
        }
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let object = self.object(key)?;
        let url = self
            .bucket
            .head_object(Some(&self.credentials), &object)
            .sign(SIGNATURE_TTL);
        let mut response = self.agent.head(url.as_str()).call()?;
        match response.status().as_u16() {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(Self::failure("head", key, &mut response)),
        }
    }

    fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let base = if self.prefix.is_empty() {
            String::new()
        } else {
            format!("{}/", self.prefix)
        };
        let wanted = format!("{base}{prefix}");
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut action = self.bucket.list_objects_v2(Some(&self.credentials));
            action.with_prefix(wanted.as_str());
            if let Some(token) = &token {
                action.with_continuation_token(token.as_str());
            }
            let url = action.sign(SIGNATURE_TTL);
            let mut response = self.agent.get(url.as_str()).call()?;
            if response.status().as_u16() != 200 {
                return Err(Self::failure("list", prefix, &mut response));
            }
            let body = Self::read_body(&mut response, MAX_LIST_BYTES)?;
            let text = String::from_utf8(body).context("S3 list response is not UTF-8")?;
            let parsed = ListObjectsV2::parse_response(&text)
                .map_err(|err| anyhow::anyhow!("invalid S3 list response: {err}"))?;
            for item in parsed.contents {
                let Some(relative) = item.key.strip_prefix(&base) else {
                    continue;
                };
                if crate::repo::backend::validate_key(relative).is_ok() {
                    keys.push(relative.to_string());
                }
            }
            match parsed.next_continuation_token {
                Some(next) if Some(&next) != token.as_ref() => token = Some(next),
                _ => break,
            }
        }
        keys.sort();
        Ok(keys)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let object = self.object(key)?;
        let url = self
            .bucket
            .delete_object(Some(&self.credentials), &object)
            .sign(SIGNATURE_TTL);
        let mut response = self.agent.delete(url.as_str()).call()?;
        match response.status().as_u16() {
            200 | 204 | 404 => Ok(()),
            _ => Err(Self::failure("delete", key, &mut response)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_are_validated() {
        assert_eq!(
            parse_location("s3://my-bucket/team/ai").unwrap(),
            S3Location {
                bucket: "my-bucket".into(),
                prefix: "team/ai".into()
            }
        );
        assert_eq!(parse_location("s3://my-bucket").unwrap().prefix, "");
        assert_eq!(parse_location("s3://my-bucket/").unwrap().prefix, "");
        for bad in [
            "my-bucket/x",
            "s3://UPPER/x",
            "s3://ab",
            "s3://-bucket/x",
            "s3://bucket/../x",
            "s3://bucket/a\\b",
            "s3://bucket/a\u{7}",
        ] {
            assert!(parse_location(bad).is_err(), "{bad}");
        }
        assert!(is_remote("s3://b/x"));
        assert!(!is_remote("/tmp/repo"));
    }

    #[test]
    fn plain_http_is_refused_except_on_localhost() {
        let credentials = Credentials::new("k", "s");
        let location = parse_location("s3://bucket/x").unwrap();
        let remote: url::Url = "http://example.com".parse().unwrap();
        let local: url::Url = "http://127.0.0.1:9000".parse().unwrap();
        assert!(!is_loopback(&remote));
        assert!(is_loopback(&local));
        assert!(S3Backend::new(
            local,
            UrlStyle::Path,
            location,
            "us-east-1".into(),
            credentials
        )
        .is_ok());
    }

    #[test]
    fn object_keys_stay_inside_the_prefix() {
        let backend = S3Backend::new(
            "http://127.0.0.1:9000".parse().unwrap(),
            UrlStyle::Path,
            parse_location("s3://bucket/base").unwrap(),
            "us-east-1".into(),
            Credentials::new("k", "s"),
        )
        .unwrap();
        assert_eq!(backend.object("data/ab/cd").unwrap(), "base/data/ab/cd");
        for bad in ["../x", "/abs", "a//b", "a\\b", ""] {
            assert!(backend.object(bad).is_err(), "{bad}");
        }
    }
}
