use std::cmp::Ordering;

/// A loosely-parsed version, e.g. "3.7.6-2" or "1.4.0", compared
/// numerically component by component so "2.9.0" < "2.10.0".
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Version {
    pub raw: String,
    components: Vec<u64>,
}

impl Version {
    pub fn parse(raw: &str) -> Version {
        let mut components = Vec::new();
        let mut current = String::new();
        for c in raw.chars() {
            if c.is_ascii_digit() {
                current.push(c);
            } else if !current.is_empty() {
                components.push(current.parse().unwrap_or(0));
                current.clear();
            }
        }
        if !current.is_empty() {
            components.push(current.parse().unwrap_or(0));
        }
        Version {
            raw: raw.to_string(),
            components,
        }
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        let len = self.components.len().max(other.components.len());
        for i in 0..len {
            let a = self.components.get(i).copied().unwrap_or(0);
            let b = other.components.get(i).copied().unwrap_or(0);
            match a.cmp(&b) {
                Ordering::Equal => continue,
                ord => return ord,
            }
        }
        self.raw.cmp(&other.raw)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
