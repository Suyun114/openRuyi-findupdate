use crate::filter::VersionStr;
use aho_corasick::AhoCorasickBuilder;
use anyhow::{anyhow, Result};
use log::{info, warn};
use owo_colors::colored::*;
use rayon::prelude::*;
use regex::Regex;
use reqwest::blocking::Client;
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use version_compare::{compare_to, Cmp};
use walkdir::WalkDir;

mod checker;
mod cli;
mod filter;
mod parser;

const VCS_VERSION_NUMBERS: &[&str] = &["+git", "+hg", "+svn", "+bzr"];

#[derive(Debug)]
struct CheckerResult {
    name: String,
    before: String,
    after: String,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct CheckResultOutput {
    name: String,
    before: String,
    after: String,
    warnings: Vec<String>,
}

fn collect_spec(dir: &Path) -> Result<Vec<PathBuf>> {
    let walker = WalkDir::new(dir).min_depth(1).max_depth(3);
    let result = walker
        .into_iter()
        .filter_map(|x| {
            let entry = x.ok()?;
            let name = entry.file_name().to_string_lossy();
            if name == "spec" || name.ends_with(".spec") {
                entry.path().canonicalize().ok()
            } else {
                None
            }
        })
        .collect();

    Ok(result)
}

fn read_toml_config(path: &Path) -> Result<HashMap<String, String>> {
    let content = std::fs::read_to_string(path)?;
    let map: HashMap<String, String> = toml::from_str(&content)?;
    Ok(map)
}

fn normalize_name(path: &Path) -> Cow<'_, str> {
    let p = path.parent().unwrap_or(path);
    let p = p.file_name().unwrap_or(p.as_os_str());

    p.to_string_lossy()
}


fn update_version<P: AsRef<Path>>(new: &str, spec: P) -> Result<String> {
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .open(spec.as_ref())?;
    let mut content = String::new();
    f.read_to_string(&mut content)?;
    let re = Regex::new(r"(?m)^Version:(\s*).+$").unwrap();
    let replaced = re.replace(&content, |caps: &regex::Captures| {
        format!("Version:{}{}", &caps[1], new)
    });

    f.seek(SeekFrom::Start(0))?;
    let bytes = replaced.as_bytes();
    f.write_all(bytes)?;
    f.set_len(bytes.len() as u64)?;

    Ok(replaced.to_string())
}


fn check_update_worker<P: AsRef<Path>>(
    client: &Client,
    spec: P,
    dry_run: bool,
    comply: bool,
    toml_override: Option<&str>,
) -> Result<CheckerResult> {
    let s = parser::parse_spec(spec.as_ref())?;
    let current_version = s.get("VER").ok_or_else(|| {
        anyhow!(
            "{}: 'Version' field is missing!",
            spec.as_ref().display()
        )
    })?;

    let current_version = current_version.trim();
    let mut warnings = Vec::new();
    let config = if let Some(raw) = toml_override {
        let raw = format!("{};", raw);
        parser::parse_check_update(&mut raw.as_str())?
    } else {
        return Err(anyhow!(
            "{}: no CHKUPDATE config available (TOML not provided)",
            spec.as_ref().display()
        ));
    };
    let new_version = checker::check_update(&config, client)?;
    let new_version = new_version.trim();
    let new_version = new_version.strip_prefix('v').unwrap_or(new_version);
    let new_version = if comply {
        let new_version_before_modification = new_version;
        let complied = new_version.compily_with_aosc();
        if new_version_before_modification != complied {
            warnings.push(format!(
                "Compliance mode enabled, was '{}'",
                new_version_before_modification
            ));
        }
        complied
    } else {
        new_version.to_string()
    };
    let new_version = new_version.as_str();
    let name = normalize_name(spec.as_ref()).to_string();
    if current_version == new_version {
        return Ok(CheckerResult {
            name,
            warnings,
            before: current_version.to_string(),
            after: new_version.to_string(),
        });
    }
    let snapshot_version = AhoCorasickBuilder::new().build(VCS_VERSION_NUMBERS);
    if current_version.contains('+') && !comply {
        warnings.push(format!("Compound version number '{}'", current_version));
        if let Some(version) = snapshot_version?.find(current_version) {
            warnings.push(format!(
                "Version number indicates a snapshot ({}) is used",
                VCS_VERSION_NUMBERS[version.pattern()]
            ))
        }
    }
    if let Ok(ret) = compare_to(current_version, new_version, Cmp::Gt) {
        if ret {
            warnings.push(format!(
                "Possible downgrade from the current version ({} -> {})",
                current_version, new_version
            ));
        }
    } else {
        warnings.push(format!(
            "Versions not comparable: `{}` and `{}`",
            current_version, new_version
        ));
    }

    if !dry_run {
        update_version(new_version, spec.as_ref())?;
    }

    Ok(CheckerResult {
        name,
        warnings,
        before: current_version.to_string(),
        after: new_version.to_string(),
    })
}

fn print_results(results: &[Result<CheckerResult>], version_only: bool) {
    if version_only {
        for result in results.iter().flatten() {
            println!("{}", result.after);
        }
    } else {
        println!("The following packages were updated:");
        println!("{:<30}{:^44}\t\tIssues", "Name", "Version");
        for result in results.iter().flatten() {
            if result.before == result.after {
                continue;
            }
            println!(
                "{:<30}{:>20} -> {:<20}\t\t{}",
                result.name.cyan(),
                result.before.red(),
                result.after.green(),
                result.warnings.join("; ").yellow()
            );
        }
        println!("\nErrors:");
        for result in results {
            if let Err(e) = result {
                println!("{}", e.bold());
            }
        }
    }
}

fn main() {
    let args = cli::build_cli().get_matches();
    env_logger::init();
    let mut pattern = None;
    if let Some(p) = args.get_one::<String>("INCLUDE") {
        pattern = Some(Regex::new(p).unwrap());
    }
    let dry_run = args.get_flag("DRY_RUN");
    let comply_with_aosc = args.get_flag("COMPLY");
    let version_only = args.get_flag("VERSION_ONLY");
    let update_checksum = args.get_flag("UPDATE_CHECKSUM");
    let current_path = std::env::current_dir().expect("Failed to get current dir.");
    let workdir = args
        .get_one::<String>("DIR")
        .map(|d| {
            let path = Path::new(d).canonicalize().unwrap();
            let specs = path.join("SPECS");
            if specs.is_dir() { specs } else { path }
        })
        .unwrap_or_else(|| Path::new(".").canonicalize().unwrap());

    let toml_config = if let Some(toml_path) = args.get_one::<String>("TOML") {
        let path = Path::new(toml_path);
        let map = read_toml_config(path).expect("Failed to read TOML file");
        Some(map)
    } else {
        None
    };

    std::env::set_current_dir(workdir).expect("Failed to set current directory");
    let mut files = collect_spec(Path::new(".")).unwrap();

    if let Some(pattern) = pattern {
        files.retain(|x| {
            if let Some(name) = x.parent().map(|p| p.to_string_lossy()) {
                pattern.is_match(&name)
            } else {
                false
            }
        });
    }

    if let Some(ref config) = toml_config {
        files.retain(|x| {
            let name = normalize_name(x);
            config.get(name.as_ref()).map_or(false, |v| !v.is_empty())
        });
    }

    if dry_run {
        warn!("Dry-run mode: files will not be updated.");
    }
    let total = files.len();
    info!("Checking updates for {} packages ...", total);
    let current = Arc::new(AtomicUsize::new(1));

    let results: Vec<_> = files
        .par_iter()
        .map_init(Client::new, |c, f| {
            let name = normalize_name(f);
            let current = current.fetch_add(1, Ordering::SeqCst);
            info!("[{}/{}] Checking {} ...", current, total, &name);
            let toml_override = toml_config
                .as_ref()
                .and_then(|cfg| cfg.get(name.as_ref()))
                .map(|s| s.as_str());
            check_update_worker(c, f, dry_run, comply_with_aosc, toml_override)
                .map_err(|e| anyhow!("{}: {:?}", name.cyan(), e))
        })
        .collect();

    print_results(&results, version_only);

    if update_checksum {
        // Update checksum via `acbs-build -gw`
        // execute: sudo ciel shell -- acbs-build -gw [packages]
        let mut packages = vec![];
        for result in results.iter().flatten() {
            if result.before == result.after {
                continue;
            }
            packages.push(result.name.as_str());
        }
        let arg = packages.join(" ");

        // add -E to pass CIEL_INST environment variable
        println!(
            "Updating checksum via: sudo -E ciel shell -- acbs-build -gw {}",
            arg
        );
        if !dry_run {
            if let Err(err) = Command::new("sudo")
                .args(["-E", "ciel", "shell", "--", "acbs-build", "-gw", &arg])
                .status()
            {
                println!("Failed with {}", err);
            }
        }
    }

    let log = args.get_one::<String>("LOG");
    let json = args.get_one::<String>("JSON");
    if log.is_some() || json.is_some() {
        let items: Vec<_> = results
            .par_iter()
            .filter_map(|x| {
                if let Ok(ret) = x {
                    if ret.after == ret.before {
                        return None;
                    }

                    Some(CheckResultOutput {
                        name: ret.name.to_owned(),
                        before: ret.before.to_owned(),
                        after: ret.after.to_owned(),
                        warnings: ret.warnings.to_vec(),
                    })
                } else {
                    None
                }
            })
            .collect();

        if let Some(log) = log {
            let log = Path::new(log);
            let log = if log.is_absolute() {
                Cow::Borrowed(log)
            } else {
                Cow::Owned(current_path.join(log))
            };

            let mut f = File::create(&*log).unwrap();
            for i in &items {
                writeln!(f, "{}", i.name).unwrap();
            }

            info!("Wrote results to {}", log.display());
        }

        if let Some(json) = json {
            let json = Path::new(json);
            let json = if json.is_absolute() {
                Cow::Borrowed(json)
            } else {
                Cow::Owned(current_path.join(json))
            };

            let mut f = File::create(&*json).unwrap();
            serde_json::to_writer(&mut f, &items).unwrap();
            info!("Wrote results to {}", json.display());
        }
    }
}
