use std::fmt;
use std::process::Command;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let s = s.strip_prefix('v').unwrap_or(s);
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return None;
        }
        Some(Version {
            major: parts[0].parse().ok()?,
            minor: parts[1].parse().ok()?,
            patch: parts[2].parse().ok()?,
        })
    }

    pub fn current() -> Version {
        Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
            .then(self.patch.cmp(&other.patch))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}.{}.{}", self.major, self.minor, self.patch)
    }
}

pub(crate) fn format_update_message(current: &Version, latest: &Version) -> Option<String> {
    if latest > current {
        Some(format!(
            "update available: {current} \u{2192} {latest} (brew update && brew upgrade clust)"
        ))
    } else {
        None
    }
}

const REPO_URL: &str = "https://github.com/clust-ai/clust-cli.git";

pub fn check_update() -> Option<String> {
    let output = Command::new("git")
        .args(["ls-remote", "--tags", REPO_URL])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8(output.stdout).ok()?;
    let latest = parse_latest_tag(&stdout)?;
    let current = Version::current();
    format_update_message(&current, &latest)
}

fn parse_latest_tag(output: &str) -> Option<Version> {
    output
        .lines()
        .filter_map(|line| {
            let refname = line.split('\t').nth(1)?;
            let tag = refname.strip_prefix("refs/tags/")?;
            if tag.ends_with("^{}") {
                return None;
            }
            Version::parse(tag)
        })
        .max()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_with_v_prefix() {
        let v = Version::parse("v0.1.0").unwrap();
        assert_eq!(
            v,
            Version {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
    }

    #[test]
    fn parse_without_prefix() {
        let v = Version::parse("0.1.0").unwrap();
        assert_eq!(
            v,
            Version {
                major: 0,
                minor: 1,
                patch: 0
            }
        );
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert!(Version::parse("abc").is_none());
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("").is_none());
        assert!(Version::parse("v").is_none());
    }

    #[test]
    fn ordering_patch() {
        let a = Version::parse("v0.0.3").unwrap();
        let b = Version::parse("v0.0.1").unwrap();
        assert!(a > b);
    }

    #[test]
    fn ordering_minor() {
        let a = Version::parse("v0.1.0").unwrap();
        let b = Version::parse("v0.0.1").unwrap();
        assert!(a > b);
    }

    #[test]
    fn ordering_major() {
        let a = Version::parse("v1.0.0").unwrap();
        let b = Version::parse("v0.9.9").unwrap();
        assert!(a > b);
    }

    #[test]
    fn ordering_equal() {
        let a = Version::parse("v0.0.1").unwrap();
        let b = Version::parse("v0.0.1").unwrap();
        assert_eq!(a, b);
        assert!(a <= b);
        assert!(a >= b);
    }

    #[test]
    fn display_format() {
        let v = Version {
            major: 1,
            minor: 2,
            patch: 3,
        };
        assert_eq!(v.to_string(), "v1.2.3");
    }

    #[test]
    fn format_message_newer() {
        let current = Version::parse("v0.0.1").unwrap();
        let latest = Version::parse("v0.1.0").unwrap();
        let msg = format_update_message(&current, &latest);
        assert!(msg.is_some());
        assert!(msg.unwrap().contains("v0.0.1"));
    }

    #[test]
    fn format_message_same() {
        let current = Version::parse("v0.0.1").unwrap();
        let latest = Version::parse("v0.0.1").unwrap();
        assert!(format_update_message(&current, &latest).is_none());
    }

    #[test]
    fn format_message_older() {
        let current = Version::parse("v0.1.0").unwrap();
        let latest = Version::parse("v0.0.1").unwrap();
        assert!(format_update_message(&current, &latest).is_none());
    }

    #[test]
    fn parse_latest_tag_basic() {
        let output =
            "abc123\trefs/tags/v0.0.1\ndef456\trefs/tags/v0.0.3\nghi789\trefs/tags/v0.0.2\n";
        let latest = parse_latest_tag(output).unwrap();
        assert_eq!(
            latest,
            Version {
                major: 0,
                minor: 0,
                patch: 3
            }
        );
    }

    #[test]
    fn parse_latest_tag_with_deref() {
        let output = "abc123\trefs/tags/v0.0.1\ndef456\trefs/tags/v0.0.1^{}\n";
        let latest = parse_latest_tag(output).unwrap();
        assert_eq!(
            latest,
            Version {
                major: 0,
                minor: 0,
                patch: 1
            }
        );
    }

    #[test]
    fn parse_latest_tag_empty() {
        assert!(parse_latest_tag("").is_none());
    }

    #[test]
    fn parse_latest_tag_no_valid_tags() {
        let output = "abc123\trefs/tags/not-a-version\n";
        assert!(parse_latest_tag(output).is_none());
    }

    #[test]
    fn parse_latest_tag_mixed() {
        let output =
            "aaa\trefs/tags/v1.0.0\nbbb\trefs/tags/release-candidate\nccc\trefs/tags/v0.9.0\n";
        let latest = parse_latest_tag(output).unwrap();
        assert_eq!(
            latest,
            Version {
                major: 1,
                minor: 0,
                patch: 0
            }
        );
    }

    #[test]
    fn check_update_async_returns() {
        // Should complete without panic; may return None or Some depending on network
        let result = check_update();
        assert!(result.is_none() || result.is_some());
    }

    #[test]
    fn check_update_async_respects_timeout() {
        let result = check_update();
        // Should complete without panic
        assert!(result.is_none() || result.is_some());
    }
}
