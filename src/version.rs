//! Semantic versions (`major.minor.patch`) and version requirements.
//!
//! Requirements use the same operators a package author writes in a
//! manifest `depends` line:
//!
//! * `*` — any version
//! * `1.2.3` or `=1.2.3` — exactly that version
//! * `>=1.2.3`, `<=1.2.3`, `>1.2.3`, `<1.2.3` — comparisons
//! * `^1.2.3` — compatible: `>=1.2.3` and `<2.0.0` (or `<0.3.0` for
//!   `^0.2.3`, `<0.0.4` for `^0.0.3`, following the usual caret rules)

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// A `major.minor.patch` version. Compared numerically, component by
/// component; `1.10.0` is newer than `1.9.0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl Version {
    pub fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { major, minor, patch }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl FromStr for Version {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 3 {
            return Err(format!("version must be major.minor.patch, got {:?}", s));
        }
        let mut nums = [0u32; 3];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!("version component {:?} is not a number", p));
            }
            nums[i] = p
                .parse::<u32>()
                .map_err(|_| format!("version component {:?} is too large", p))?;
        }
        Ok(Version::new(nums[0], nums[1], nums[2]))
    }
}

/// A requirement on a version, as written in a manifest or on the CLI.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum VersionReq {
    /// `*`: any version at all.
    Any,
    /// `=1.2.3` (or a bare `1.2.3`).
    Exact(Version),
    /// `>=1.2.3`.
    GreaterOrEqual(Version),
    /// `<=1.2.3`.
    LessOrEqual(Version),
    /// `>1.2.3`.
    Greater(Version),
    /// `<1.2.3`.
    Less(Version),
    /// `^1.2.3`: compatible with 1.2.3 (see module docs).
    Caret(Version),
}

impl VersionReq {
    /// Parse a requirement string. A bare version (`1.2.3`) means exact.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s == "*" {
            return Ok(VersionReq::Any);
        }
        for (prefix, make) in [
            (">=", VersionReq::GreaterOrEqual as fn(Version) -> VersionReq),
            ("<=", VersionReq::LessOrEqual as fn(Version) -> VersionReq),
            ("^", VersionReq::Caret as fn(Version) -> VersionReq),
            ("=", VersionReq::Exact as fn(Version) -> VersionReq),
            (">", VersionReq::Greater as fn(Version) -> VersionReq),
            ("<", VersionReq::Less as fn(Version) -> VersionReq),
        ] {
            if let Some(rest) = s.strip_prefix(prefix) {
                let v = rest.trim().parse::<Version>()?;
                return Ok(make(v));
            }
        }
        Ok(VersionReq::Exact(s.parse::<Version>()?))
    }

    /// Does `version` satisfy this requirement?
    pub fn matches(&self, version: &Version) -> bool {
        match self {
            VersionReq::Any => true,
            VersionReq::Exact(v) => version == v,
            VersionReq::GreaterOrEqual(v) => version >= v,
            VersionReq::LessOrEqual(v) => version <= v,
            VersionReq::Greater(v) => version > v,
            VersionReq::Less(v) => version < v,
            VersionReq::Caret(v) => {
                if version < v {
                    return false;
                }
                // Upper bound: first breaking version.
                let upper = if v.major > 0 {
                    Version::new(v.major + 1, 0, 0)
                } else if v.minor > 0 {
                    Version::new(0, v.minor + 1, 0)
                } else {
                    Version::new(0, 0, v.patch + 1)
                };
                version.cmp(&upper) == Ordering::Less
            }
        }
    }
}

impl fmt::Display for VersionReq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VersionReq::Any => write!(f, "*"),
            VersionReq::Exact(v) => write!(f, "={}", v),
            VersionReq::GreaterOrEqual(v) => write!(f, ">={}", v),
            VersionReq::LessOrEqual(v) => write!(f, "<={}", v),
            VersionReq::Greater(v) => write!(f, ">{}", v),
            VersionReq::Less(v) => write!(f, "<{}", v),
            VersionReq::Caret(v) => write!(f, "^{}", v),
        }
    }
}

/// One dependency: a package name plus a version requirement.
///
/// Written as `name` (any version) or `name <req>`, e.g. `fs >=1.0.0`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Dependency {
    pub name: String,
    pub req: VersionReq,
}

impl Dependency {
    pub fn new(name: &str, req: VersionReq) -> Self {
        Self { name: name.to_string(), req }
    }

    /// Parse `name` or `name <req>`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.is_empty() {
            return Err("empty dependency".to_string());
        }
        let mut parts = s.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap();
        validate_package_name(name)?;
        let req = match parts.next() {
            Some(rest) => {
                let rest = rest.trim();
                if rest.is_empty() {
                    VersionReq::Any
                } else {
                    VersionReq::parse(rest)?
                }
            }
            None => VersionReq::Any,
        };
        Ok(Dependency { name: name.to_string(), req })
    }
}

impl fmt::Display for Dependency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.req {
            VersionReq::Any => write!(f, "{}", self.name),
            req => write!(f, "{} {}", self.name, req),
        }
    }
}

/// Package names: start with an ASCII letter or digit, then ASCII
/// letters, digits, `-` or `_`.
pub fn validate_package_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("package name is empty".to_string());
    }
    let mut chars = name.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(format!("package name {:?} must start with a letter or digit", name));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!(
            "package name {:?} may only contain letters, digits, '-' and '_'",
            name
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_parse_and_order() {
        assert_eq!("1.2.3".parse::<Version>().unwrap(), Version::new(1, 2, 3));
        assert_eq!("0.0.0".parse::<Version>().unwrap(), Version::new(0, 0, 0));
        assert!("1.2".parse::<Version>().is_err());
        assert!("1.2.3.4".parse::<Version>().is_err());
        assert!("1.x.3".parse::<Version>().is_err());
        assert!("".parse::<Version>().is_err());
        assert!("-1.2.3".parse::<Version>().is_err());
        assert!(Version::new(1, 10, 0) > Version::new(1, 9, 0));
        assert!(Version::new(2, 0, 0) > Version::new(1, 99, 99));
        assert_eq!(format!("{}", Version::new(1, 2, 3)), "1.2.3");
    }

    #[test]
    fn requirement_matching() {
        let v = Version::new(1, 2, 3);
        assert!(VersionReq::parse("*").unwrap().matches(&v));
        assert!(VersionReq::parse("=1.2.3").unwrap().matches(&v));
        assert!(VersionReq::parse("1.2.3").unwrap().matches(&v));
        assert!(!VersionReq::parse("=1.2.4").unwrap().matches(&v));
        assert!(VersionReq::parse(">=1.2.3").unwrap().matches(&v));
        assert!(!VersionReq::parse(">=1.2.4").unwrap().matches(&v));
        assert!(VersionReq::parse("<=1.2.3").unwrap().matches(&v));
        assert!(VersionReq::parse(">1.2.2").unwrap().matches(&v));
        assert!(!VersionReq::parse(">1.2.3").unwrap().matches(&v));
        assert!(VersionReq::parse("<1.2.4").unwrap().matches(&v));
        assert!(!VersionReq::parse("<1.2.3").unwrap().matches(&v));
        // Caret: ^1.2.3 allows 1.9.9 but not 2.0.0.
        assert!(VersionReq::parse("^1.2.3").unwrap().matches(&Version::new(1, 9, 9)));
        assert!(!VersionReq::parse("^1.2.3").unwrap().matches(&Version::new(2, 0, 0)));
        assert!(!VersionReq::parse("^1.2.3").unwrap().matches(&Version::new(1, 2, 2)));
        // Caret on 0.x: ^0.2.3 allows 0.2.9 but not 0.3.0.
        assert!(VersionReq::parse("^0.2.3").unwrap().matches(&Version::new(0, 2, 9)));
        assert!(!VersionReq::parse("^0.2.3").unwrap().matches(&Version::new(0, 3, 0)));
        // Caret on 0.0.x: ^0.0.3 allows only 0.0.3.
        assert!(VersionReq::parse("^0.0.3").unwrap().matches(&Version::new(0, 0, 3)));
        assert!(!VersionReq::parse("^0.0.3").unwrap().matches(&Version::new(0, 0, 4)));
        assert!(VersionReq::parse(">=x").is_err());
        assert!(VersionReq::parse("").is_err());
    }

    #[test]
    fn requirement_display_roundtrip() {
        for s in ["*", "=1.2.3", ">=1.0.0", "<=2.0.0", ">0.1.0", "<3.0.0", "^1.2.3"] {
            let req = VersionReq::parse(s).unwrap();
            assert_eq!(req.to_string(), s, "display of {:?}", req);
            assert_eq!(VersionReq::parse(&req.to_string()).unwrap(), req);
        }
        // A bare version parses as exact and displays with '='.
        assert_eq!(VersionReq::parse("1.2.3").unwrap().to_string(), "=1.2.3");
    }

    #[test]
    fn dependency_parse() {
        let d = Dependency::parse("fs >=1.0.0").unwrap();
        assert_eq!(d.name, "fs");
        assert_eq!(d.req, VersionReq::GreaterOrEqual(Version::new(1, 0, 0)));
        assert_eq!(Dependency::parse("net").unwrap().req, VersionReq::Any);
        assert_eq!(Dependency::parse("shell").unwrap().to_string(), "shell");
        assert_eq!(Dependency::parse("fs >=1.0.0").unwrap().to_string(), "fs >=1.0.0");
        assert!(Dependency::parse("").is_err());
        assert!(Dependency::parse("bad name >=1.0.0").is_err());
        assert!(Dependency::parse("fs =>1.0.0").is_err());
        assert!(Dependency::parse("/etc >=1.0.0").is_err());
    }

    #[test]
    fn package_name_validation() {
        assert!(validate_package_name("shell").is_ok());
        assert!(validate_package_name("rustos-net").is_ok());
        assert!(validate_package_name("a1_b2").is_ok());
        assert!(validate_package_name("").is_err());
        assert!(validate_package_name("-x").is_err());
        assert!(validate_package_name("a b").is_err());
        assert!(validate_package_name("a/b").is_err());
    }
}
