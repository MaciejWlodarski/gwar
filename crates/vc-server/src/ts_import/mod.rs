//! Moving a TeamSpeak 3 server to Gwar (docs/teamspeak-import.md): read it over ServerQuery,
//! work out the plan, and write it into a stopped Gwar server's database.

pub mod apply;
pub mod plan;
pub mod source;

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result, bail};

/// Reads `--passwords`: one `name = password` per line (quotes optional; `#` comments), the name a
/// channel's path ("Parent/Child"), its name, or `server`.
pub fn parse_passwords(text: &str) -> Result<BTreeMap<String, String>> {
    let unquote = |s: &str| {
        let s = s.trim();
        s.strip_prefix('"').and_then(|s| s.strip_suffix('"')).unwrap_or(s).replace("\\\"", "\"")
    };
    let mut out = BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // The name may itself contain '=' only when quoted.
        let split = match line.strip_prefix('"') {
            Some(rest) => rest.find('"').map(|end| end + 2).and_then(|end| line[end..].find('=').map(|eq| end + eq)),
            None => line.find('='),
        };
        let Some(eq) = split else { bail!("line {}: expected name = password", n + 1) };
        out.insert(unquote(&line[..eq]), unquote(&line[eq + 1..]));
    }
    Ok(out)
}

pub fn read_passwords(path: &Path) -> Result<BTreeMap<String, String>> {
    parse_passwords(&std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?)
}

#[cfg(test)]
mod tests;
