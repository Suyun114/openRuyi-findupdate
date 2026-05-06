use anyhow::{anyhow, Context as _, Result};
use regex::Regex;
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    process::Command,
};
use winnow::{
    ascii::alphanumeric1,
    combinator::{alt, repeat, separated_pair, terminated},
    token::take_until,
    PResult, Parser,
};

type Context = HashMap<String, String>;

const CONFIG_SEPARATOR: &str = "::";

fn take_type<'a>(input: &mut &'a str) -> PResult<&'a str> {
    take_until(0.., CONFIG_SEPARATOR).parse_next(input)
}

fn kv_key_inner(input: &mut &str) -> PResult<()> {
    repeat(1.., alt((alphanumeric1, "_"))).parse_next(input)
}

fn kv_key<'a>(input: &mut &'a str) -> PResult<&'a str> {
    kv_key_inner.recognize().parse_next(input)
}

fn kv_pair<'a>(input: &mut &'a str) -> PResult<(&'a str, &'a str)> {
    separated_pair(kv_key, "=", take_until(0.., ";")).parse_next(input)
}

fn kv_pairs<'a>(input: &mut &'a str) -> PResult<Vec<(&'a str, &'a str)>> {
    repeat(1.., terminated(kv_pair, ";")).parse_next(input)
}

fn config_line<'a>(input: &mut &'a str) -> PResult<(&'a str, Vec<(&'a str, &'a str)>)> {
    separated_pair(take_type, CONFIG_SEPARATOR, kv_pairs).parse_next(input)
}

pub(crate) fn parse_spec<P: AsRef<Path>>(spec: P) -> Result<Context> {
    let spec_path = spec.as_ref().to_str().context("Invalid spec path")?;

    // Try rpmspec first
    let version = match Command::new("rpmspec")
        .args(["-q", "--srpm", "--qf", "%{version}", spec_path])
        .output()
    {
        Ok(output) if output.status.success() => {
            String::from_utf8(output.stdout)?.trim().to_string()
        }
        _ => {
            // Fallback: resolve macro-based version
            let content = std::fs::read_to_string(spec.as_ref())?;
            resolve_version_with_rpmspec(&content, spec.as_ref())?
        }
    };

    // Reject unresolved macros
    if version.contains('%') {
        return Err(anyhow!("Unresolved macros in version: {version}"));
    }

    let mut context = HashMap::new();
    context.insert("VER".to_string(), version);
    Ok(context)
}

/// Find all %define/%global lines needed to resolve the Version field,
/// construct a minimal spec, and feed it to rpmspec.
fn resolve_version_with_rpmspec(content: &str, spec_path: &Path) -> Result<String> {
    let version_re = Regex::new(r"(?m)^Version:\s*(.+)$").unwrap();
    let version_line = version_re
        .captures(content)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().trim().to_string())
        .ok_or_else(|| anyhow!("No Version field in spec"))?;

    // No macros — just return the version
    if !version_line.contains("%{") {
        return Ok(version_line);
    }

    // Extract all %define / %global definitions
    let define_re = Regex::new(r"(?m)^%(?:define|global)\s+(\w+)\s+(.+)$").unwrap();
    let defines: HashMap<String, String> = define_re
        .captures_iter(content)
        .map(|c| (c[1].to_string(), c[2].trim().to_string()))
        .collect();

    // DFS: find all macros referenced in the version line and their transitive deps
    // Matches %{foo}, %{?foo}, %{!?foo}, %{?foo:...}
    let macro_ref_re = Regex::new(r"%\{!?\??(\w+)").unwrap();
    let mut needed: HashMap<String, String> = HashMap::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Seed queue with macros from the version line
    for cap in macro_ref_re.captures_iter(&version_line) {
        let name = cap[1].to_string();
        if name != "nil" && !visited.contains(&name) {
            visited.insert(name.clone());
            queue.push_back(name);
        }
    }

    while let Some(name) = queue.pop_front() {
        if let Some(val) = defines.get(&name) {
            needed.insert(name.clone(), val.clone());
            for cap in macro_ref_re.captures_iter(val) {
                let dep = cap[1].to_string();
                if dep != "nil" && !visited.contains(&dep) {
                    visited.insert(dep.clone());
                    queue.push_back(dep);
                }
            }
        }
    }

    // Build minimal spec — defines must come first
    let dir = spec_path.parent().unwrap_or(Path::new("."));
    let tmp_path = dir.join(".findupdate-tmp.spec");
    let mut minimal = String::new();
    for (name, val) in &needed {
        minimal.push_str(&format!("%global {} {}\n", name, val));
    }
    minimal.push_str("Name: dummy\nVersion: ");
    minimal.push_str(&version_line);
    minimal.push_str("\nRelease: 1\nSummary: dummy\nLicense: MIT\n%description\ndummy\n");

    std::fs::write(&tmp_path, &minimal)?;

    let output = Command::new("rpmspec")
        .args(["-q", "--srpm", "--qf", "%{version}", tmp_path.to_str().unwrap()])
        .output();

    std::fs::remove_file(&tmp_path).ok();

    match output {
        Ok(o) if o.status.success() => {
            let v = String::from_utf8(o.stdout)?.trim().to_string();
            if v.is_empty() {
                Err(anyhow!("rpmspec returned empty version"))
            } else {
                Ok(v)
            }
        }
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            Err(anyhow!("rpmspec failed: {}", stderr.trim()))
        }
        Err(e) => Err(anyhow!("rpmspec error: {e}")),
    }
}

pub(crate) fn parse_check_update(content: &mut &str) -> Result<Context> {
    let parsed = config_line(content).map_err(|err| anyhow!("Invalid config line: {}", err))?;
    let mut context = HashMap::new();
    let config = parsed.1;
    context.insert("type".to_string(), parsed.0.to_string());

    for (k, v) in config {
        context.insert(k.to_string(), v.to_string());
    }

    Ok(context)
}


#[test]
fn test_take_type() {
    let test = &mut "test::1";
    let res = take_type(test);

    assert_eq!(res, Ok("test"));
    assert_eq!(test, &mut "::1");
}

#[test]
fn test_kv_key() {
    let test = &mut "a_b123";
    let res = kv_key(test);
    assert_eq!(res, Ok("a_b123"));
    assert_eq!(test, &mut "");
}

#[test]
fn test_kv() {
    let test = &mut "a=b;";
    let res = kv_pair(test);
    assert_eq!(res, Ok(("a", "b")));
    assert_eq!(test, &mut ";")
}

#[test]
fn test_kv_pairs() {
    let test = &mut "a=b;b=d;";
    let res = kv_pairs(test);

    assert_eq!(res, Ok(vec![("a", "b"), ("b", "d")]));
    assert_eq!(test, &mut "");
}

