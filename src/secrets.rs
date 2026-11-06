use crate::engine::SecretFinding;
use regex::bytes::Regex;
use std::io::Read;
use std::sync::OnceLock;

fn detectors() -> &'static Vec<(&'static str, Regex)> {
    static DETECTORS: OnceLock<Vec<(&str, Regex)>> = OnceLock::new();
    DETECTORS.get_or_init(|| {
        [
            ("anthropic", r"\bsk-ant-[A-Za-z0-9_-]{16,}"),
            ("openai", r"\bsk-(?:proj-|svcacct-)?[A-Za-z0-9_-]{20,}"),
            ("xai", r"\bxai-[A-Za-z0-9_-]{20,}"),
            ("google", r"\bAIza[A-Za-z0-9_-]{30,}"),
            (
                "github",
                r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,})",
            ),
            ("aws", r"\b(?:AKIA|ASIA)[A-Z0-9]{16}"),
            ("aws_secret", r#"(?i)\b(?:aws_secret_access_key|secret_access_key|secretAccessKey)["']?\s*[:=]\s*["']?[A-Za-z0-9/+=]{40}(?:[^A-Za-z0-9/+=]|$)"#),
            ("slack", r"\b(?:xox[baprs]-|xoxe[.-])[A-Za-z0-9.-]{10,}"),
            (
                "private_key",
                r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |ENCRYPTED )?PRIVATE KEY-----",
            ),
            ("bearer", r"(?i)\bBearer\s+[A-Za-z0-9._~+/-]{8,}=*"),
        ]
        .into_iter()
        .map(|(name, pattern)| (name, Regex::new(pattern).expect("valid detector")))
        .collect()
    })
}

pub fn scan(provider: &str, rel: &str, bytes: &[u8]) -> Vec<SecretFinding> {
    scan_reader(provider, rel, bytes).expect("byte slice reads cannot fail")
}

pub fn scan_reader(
    provider: &str,
    rel: &str,
    mut reader: impl Read,
) -> std::io::Result<Vec<SecretFinding>> {
    const CHUNK: usize = 64 * 1024;
    const OVERLAP: usize = 4096;
    let mut buffer = Vec::with_capacity(CHUNK + OVERLAP);
    let mut chunk = [0u8; CHUNK];
    let mut counts = vec![0usize; detectors().len()];
    let mut last_start = vec![None; detectors().len()];
    let mut offset = 0u64;
    loop {
        let n = reader.read(&mut chunk)?;
        buffer.extend_from_slice(&chunk[..n]);
        let commit = if n == 0 {
            buffer.len()
        } else {
            buffer.len().saturating_sub(OVERLAP)
        };
        for (index, (_, detector)) in detectors().iter().enumerate() {
            for found in detector.find_iter(&buffer) {
                let start = offset + found.start() as u64;
                if (offset == 0 || found.start() > 0)
                    && found.start() < commit
                    && last_start[index].is_none_or(|last| start > last)
                {
                    counts[index] += 1;
                    last_start[index] = Some(start);
                }
            }
        }
        if n == 0 {
            break;
        }
        let keep = buffer.len().min(OVERLAP + 1);
        let discard = buffer.len() - keep;
        buffer.drain(..discard);
        offset += discard as u64;
    }
    Ok(detectors()
        .iter()
        .zip(counts)
        .filter(|(_, count)| *count > 0)
        .map(|((name, _), count)| SecretFinding {
            provider: provider.into(),
            rel: rel.into(),
            detector: (*name).into(),
            count,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn findings_never_include_secrets() {
        let token = "ghp_abcdefghijklmnopqrstuvwxyz";
        let findings = scan("fake", "fixture", token.as_bytes());
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].count, 1);
        assert!(!serde_json::to_string(&findings).unwrap().contains(token));
        assert!(scan("fake", "fixture", b"ordinary text").is_empty());
    }
    #[test]
    fn word_boundaries_reject_token_suffixes_inside_words() {
        for text in [
            "task-abcdefghijklmnopqrstuvwxyz",
            "prefixghp_abcdefghijklmnopqrstuvwxyz",
            "prefixxai-abcdefghijklmnopqrstuvwxyz",
            "prefixBearer abcdefghijklmnop",
        ] {
            assert!(
                scan("fake", "fixture", text.as_bytes()).is_empty(),
                "{text}"
            );
        }
        assert_eq!(
            scan("fake", "fixture", b" sk-abcdefghijklmnopqrstuvwxyz")[0].count,
            1
        );
    }

    #[test]
    fn streaming_overlap_finds_boundary_tokens_exactly_once() {
        for boundary in [64 * 1024 - 4096, 64 * 1024, 2 * 64 * 1024] {
            let mut bytes = vec![b' '; boundary - 8];
            bytes.extend_from_slice(b"ghp_abcdefghijklmnopqrstuvwxyz");
            bytes.resize(3 * 64 * 1024, b' ');
            let findings = scan_reader("fake", "fixture", &bytes[..]).unwrap();
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].count, 1);
        }
    }

    #[test]
    fn scan_reader_never_requests_whole_file() {
        struct Chunks {
            remaining: usize,
        }
        impl std::io::Read for Chunks {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                assert!(buffer.len() <= 64 * 1024);
                let n = self.remaining.min(buffer.len());
                buffer[..n].fill(b' ');
                self.remaining -= n;
                Ok(n)
            }
        }
        assert!(scan_reader(
            "fake",
            "large",
            Chunks {
                remaining: 4 * 1024 * 1024
            }
        )
        .unwrap()
        .is_empty());
    }
    #[test]
    fn slack_xoxe_and_aws_secret_assignments_are_counted_without_values() {
        let input =
            b"xoxe-abcdefghijklmnop AWS_SECRET_ACCESS_KEY=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN";
        let findings = scan("fixture", "config", input);
        assert!(findings
            .iter()
            .any(|f| f.detector == "slack" && f.count == 1));
        assert!(findings
            .iter()
            .any(|f| f.detector == "aws_secret" && f.count == 1));
        let serialized = serde_json::to_string(&findings).unwrap();
        assert!(!serialized.contains("abcdefghijklmnop"));
        let dotted = scan("fixture", "config", b"xoxe.xoxb-abcdefghijklmnopqrstuvwxyz");
        assert!(dotted.iter().any(|f| f.detector == "slack" && f.count == 1));
        let ending = format!("aws_secret_access_key=\"{}=/\"", "a".repeat(38));
        assert!(scan("fixture", "config", ending.as_bytes())
            .iter()
            .any(|f| f.detector == "aws_secret" && f.count == 1));
        assert!(scan("fixture", "config", b"prefixxoxe-abcdefghijklmnop prefixAWS_SECRET_ACCESS_KEY=abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMN").is_empty());
    }
}
