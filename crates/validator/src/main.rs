use anyhow::{Context, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct Config {
    domain: DomainConfig,
    limits: LimitsConfig,
    validation: ValidationConfig,
    github: GithubConfig,
}

#[derive(Debug, Deserialize)]
struct DomainConfig {
    name: String,
    project: String,
    zone_id: String,
}

#[derive(Debug, Deserialize)]
struct LimitsConfig {
    max_per_user: usize,
}

#[derive(Debug, Deserialize)]
struct ValidationConfig {
    allowed_record_types: Vec<String>,
    min_ttl: u32,
    max_ttl: u32,
}

#[derive(Debug, Deserialize)]
struct GithubConfig {
    repo_owner: String,
    repo_name: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct DomainEntry {
    subdomain: String,
    owner: Owner,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    records: Vec<Record>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Owner {
    github_username: String,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
struct Record {
    #[serde(rename = "type")]
    ty: String,
    value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ttl: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
enum ValidationError {
    #[error("schema validation failed: {0}")]
    Schema(String),
    #[error("subdomain format invalid: {0}")]
    InvalidSubdomain(String),
    #[error("subdomain '{0}' is reserved")]
    Reserved(String),
    #[error("subdomain '{0}' is blocked")]
    Blocked(String),
    #[error("record type '{0}' not allowed")]
    DisallowedRecordType(String),
    #[error("ttl {0} out of range ({1}-{2})")]
    InvalidTtl(u32, u32, u32),
    #[error("owner mismatch: expected '{expected}', got '{actual}'")]
    OwnerMismatch { expected: String, actual: String },
    #[error("user limit exceeded: max {0} domains")]
    UserLimitExceeded(usize),
    #[error("hijack attempt: previous owner was '{previous}', pr author is '{author}'")]
    HijackAttempt { previous: String, author: String },
    #[error("json parse error: {0}")]
    Parse(String),
}

fn main() -> Result<()> {
    let config = load_config()?;
    let pr_author = env::var("GITHUB_ACTOR").unwrap_or_default();
    let base_branch = env::var("GITHUB_BASE_REF").unwrap_or_else(|_| "main".to_string());

    if pr_author.is_empty() {
        eprintln!("warning: GITHUB_ACTOR not set, skipping author checks");
    }

    let schema = load_schema()?;
    let reserved = load_reserved()?;
    let blocked = load_blocked()?;

    let changed_files = get_changed_files(&base_branch)?;
    let domain_files: Vec<_> = changed_files
        .into_iter()
        .filter(|f| f.starts_with("domains/") && f.ends_with(".json"))
        .collect();

    if domain_files.is_empty() {
        println!("no domain files changed, skipping validation");
        return Ok(());
    }

    let existing_domains = load_all_domains()?;
    let mut errors = Vec::new();

    for file in &domain_files {
        match validate_domain_file(
            file,
            &pr_author,
            &config,
            &schema,
            &reserved,
            &blocked,
            &existing_domains,
            &base_branch,
        ) {
            Ok(_) => println!("ok: {}", file),
            Err(e) => {
                eprintln!("fail: {} -> {}", file, e);
                errors.push((file.clone(), e));
            }
        }
    }

    if !errors.is_empty() {
        eprintln!("\n{} validation error(s) found", errors.len());
        std::process::exit(1);
    }

    println!("all validations passed");
    Ok(())
}

fn load_config() -> Result<Config> {
    let content = fs::read_to_string("config.toml").context("read config.toml")?;
    let config: Config = toml::from_str(&content).context("parse config.toml")?;
    Ok(config)
}

fn load_schema() -> Result<serde_json::Value> {
    let content = fs::read_to_string("schema.json").context("read schema.json")?;
    let schema = serde_json::from_str(&content).context("parse schema.json")?;
    Ok(schema)
}

fn load_reserved() -> Result<Vec<String>> {
    let content = fs::read_to_string("reserved.json").context("read reserved.json")?;
    let list: Vec<String> = serde_json::from_str(&content).context("parse reserved.json")?;
    Ok(list)
}

fn load_blocked() -> Result<Vec<String>> {
    let content = fs::read_to_string("blocked.json").context("read blocked.json")?;
    let list: Vec<String> = serde_json::from_str(&content).context("parse blocked.json")?;
    Ok(list)
}

fn get_changed_files(base_branch: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "diff",
            "--name-only",
            "--diff-filter=ACM",
            &format!("origin/{}...HEAD", base_branch),
        ])
        .output()
        .context("run git diff")?;

    if !output.status.success() {
        let output = Command::new("git")
            .args(["diff", "--name-only", "--diff-filter=ACM", &format!("{}...HEAD", base_branch)])
            .output()
            .context("run git diff fallback")?;

        if !output.status.success() {
            anyhow::bail!("git diff failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        return Ok(stdout.lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().map(|s| s.to_string()).filter(|s| !s.is_empty()).collect())
}

fn load_all_domains() -> Result<HashMap<String, DomainEntry>> {
    let mut map = HashMap::new();
    let entries = fs::read_dir("domains").context("read domains/ dir")?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = fs::read_to_string(&path).context(format!("read {:?}", path))?;
        let domain: DomainEntry = match serde_json::from_str(&content) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let key = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
        map.insert(key, domain);
    }

    Ok(map)
}

fn validate_domain_file(
    file: &str,
    pr_author: &str,
    config: &Config,
    schema: &serde_json::Value,
    reserved: &[String],
    blocked: &[String],
    existing: &HashMap<String, DomainEntry>,
    base_branch: &str,
) -> Result<(), ValidationError> {
    let content = fs::read_to_string(file).map_err(|e| ValidationError::Parse(e.to_string()))?;
    let domain: DomainEntry =
        serde_json::from_str(&content).map_err(|e| ValidationError::Parse(e.to_string()))?;

    let compiled = jsonschema::JSONSchema::compile(schema)
        .map_err(|e| ValidationError::Schema(e.to_string()))?;
    let instance = serde_json::from_str(&content).map_err(|e| ValidationError::Parse(e.to_string()))?;
    let errors: Vec<_> = compiled.validate(&instance).err().into_iter().flatten().collect();
    if !errors.is_empty() {
        let msg = errors.iter().map(|e| e.to_string()).collect::<Vec<_>>().join("; ");
        return Err(ValidationError::Schema(msg));
    }

    let re = Regex::new(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$").unwrap();
    if !re.is_match(&domain.subdomain) {
        return Err(ValidationError::InvalidSubdomain(domain.subdomain.clone()));
    }

    if reserved.contains(&domain.subdomain) {
        return Err(ValidationError::Reserved(domain.subdomain.clone()));
    }
    if blocked.contains(&domain.subdomain) {
        return Err(ValidationError::Blocked(domain.subdomain.clone()));
    }

    for record in &domain.records {
        if !config.validation.allowed_record_types.contains(&record.ty) {
            return Err(ValidationError::DisallowedRecordType(record.ty.clone()));
        }
        if let Some(ttl) = record.ttl {
            if ttl < config.validation.min_ttl || ttl > config.validation.max_ttl {
                return Err(ValidationError::InvalidTtl(
                    ttl,
                    config.validation.min_ttl,
                    config.validation.max_ttl,
                ));
            }
        }
    }

    if !pr_author.is_empty() && domain.owner.github_username != pr_author {
        return Err(ValidationError::OwnerMismatch {
            expected: pr_author.to_string(),
            actual: domain.owner.github_username.clone(),
        });
    }

    let stem = std::path::Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    if existing.contains_key(stem) {
        // check if this file existed in base branch
        let base_content = get_file_at_base(file, base_branch);
        if let Ok(base_json) = base_content {
            if let Ok(base_domain) = serde_json::from_str::<DomainEntry>(&base_json) {
                if base_domain.owner.github_username != domain.owner.github_username {
                    return Err(ValidationError::HijackAttempt {
                        previous: base_domain.owner.github_username,
                        author: domain.owner.github_username,
                    });
                }
            }
        }
    }

    let user_count = existing
        .values()
        .filter(|d| d.owner.github_username == domain.owner.github_username)
        .count();

    if !existing.contains_key(stem) && user_count + 1 > config.limits.max_per_user {
        return Err(ValidationError::UserLimitExceeded(config.limits.max_per_user));
    }

    Ok(())
}

fn get_file_at_base(file: &str, base_branch: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["show", &format!("origin/{}:{}", base_branch, file)])
        .output()
        .context("git show")?;

    if !output.status.success() {
        let output = Command::new("git")
            .args(["show", &format!("{}:{}", base_branch, file)])
            .output()
            .context("git show fallback")?;

        if !output.status.success() {
            anyhow::bail!("file not found in base branch");
        }

        return Ok(String::from_utf8_lossy(&output.stdout).to_string());
    }

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}
