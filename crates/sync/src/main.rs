use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::fs;
use std::path::Path;

#[derive(Debug, Deserialize)]
struct Config {
    domain: DomainConfig,
}

#[derive(Debug, Deserialize)]
struct DomainConfig {
    name: String,
    zone_id: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct DomainEntry {
    subdomain: String,
    owner: Owner,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    records: Vec<Record>,
}

#[derive(Debug, Deserialize, Serialize)]
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

#[derive(Debug, Serialize)]
struct CloudflareRecord {
    #[serde(rename = "type")]
    ty: String,
    name: String,
    content: String,
    ttl: u32,
}

#[derive(Debug, Deserialize)]
struct CloudflareListResponse {
    result: Vec<CloudflareApiRecord>,
    success: bool,
}

#[derive(Debug, Deserialize, Clone)]
struct CloudflareApiRecord {
    id: String,
    #[serde(rename = "type")]
    ty: String,
    name: String,
    content: String,
    ttl: u32,
}

#[derive(Debug, Deserialize)]
struct CloudflareSingleResponse {
    result: Option<CloudflareApiRecord>,
    success: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    let config = load_config()?;
    let token = env::var("CLOUDFLARE_API_TOKEN").context("CLOUDFLARE_API_TOKEN not set")?;
    let client = reqwest::Client::new();
    let base_url = format!(
        "https://api.cloudflare.com/client/v4/zones/{}/dns_records",
        config.domain.zone_id
    );

    let domains = load_all_domains()?;
    let existing = get_existing_records(&client, &token, &base_url).await?;
    let mut synced = HashMap::new();

    for (name, entry) in &domains {
        let full_name = format!("{}.{}", entry.subdomain, config.domain.name);

        for record in &entry.records {
            if record.ty == "URL" {
                println!("skip URL record for {} - use page rules or workers", full_name);
                continue;
            }

            let desired = CloudflareRecord {
                ty: record.ty.clone(),
                name: full_name.clone(),
                content: record.value.clone(),
                ttl: record.ttl.unwrap_or(300),
            };

            let key = format!("{}:{}", desired.ty, desired.name);
            if let Some(existing_record) = existing.get(&key) {
                if existing_record.content != desired.content || existing_record.ttl != desired.ttl {
                    println!("update {} {}", desired.ty, desired.name);
                    update_record(&client, &token, &base_url, &existing_record.id, &desired).await?;
                } else {
                    println!("unchanged {} {}", desired.ty, desired.name);
                }
            } else {
                println!("create {} {}", desired.ty, desired.name);
                create_record(&client, &token, &base_url, &desired).await?;
            }

            synced.insert(key, true);
        }
    }

    let deleted = get_deleted_domains()?;
    for subdomain in deleted {
        let full_name = format!("{}.{}", subdomain, config.domain.name);
        for (key, record) in &existing {
            if record.name == full_name && !synced.contains_key(key) {
                println!("delete {} {}", record.ty, record.name);
                delete_record(&client, &token, &base_url, &record.id).await?;
            }
        }
    }

    println!("sync complete");
    Ok(())
}

fn load_config() -> Result<Config> {
    let content = fs::read_to_string("config.toml").context("read config.toml")?;
    let config: Config = toml::from_str(&content).context("parse config.toml")?;
    Ok(config)
}

fn load_all_domains() -> Result<HashMap<String, DomainEntry>> {
    let mut map = HashMap::new();
    let entries = fs::read_dir("domains").context("read domains/")?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let content = fs::read_to_string(&path).context("read domain file")?;
        let domain: DomainEntry = serde_json::from_str(&content).context("parse domain file")?;
        let key = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        map.insert(key, domain);
    }

    Ok(map)
}

fn get_deleted_domains() -> Result<Vec<String>> {
    let before = env::var("GITHUB_EVENT_BEFORE").unwrap_or_else(|_| "HEAD~1".to_string());
    let after = env::var("GITHUB_EVENT_AFTER").unwrap_or_else(|_| "HEAD".to_string());

    let output = std::process::Command::new("git")
        .args([
            "diff",
            "--name-only",
            "--diff-filter=D",
            &before,
            &after,
        ])
        .output()
        .context("git diff for deleted files")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let deleted: Vec<String> = stdout
        .lines()
        .filter(|f| f.starts_with("domains/") && f.ends_with(".json"))
        .map(|f| {
            Path::new(f)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .collect();

    Ok(deleted)
}

async fn get_existing_records(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
) -> Result<HashMap<String, CloudflareApiRecord>> {
    let mut map = HashMap::new();
    let mut page = 1;

    loop {
        let resp = client
            .get(base_url)
            .header("Authorization", format!("Bearer {}", token))
            .query(&[("page", page.to_string()), ("per_page", "100".to_string())])
            .send()
            .await
            .context("list cloudflare records")?;

        let body: CloudflareListResponse = resp.json().await.context("parse cloudflare response")?;
        if !body.success {
            anyhow::bail!("cloudflare list failed");
        }

        if body.result.is_empty() {
            break;
        }

        for record in body.result {
            let key = format!("{}:{}", record.ty, record.name);
            map.insert(key, record);
        }

        page += 1;
    }

    Ok(map)
}

async fn create_record(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    record: &CloudflareRecord,
) -> Result<()> {
    let resp = client
        .post(base_url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(record)
        .send()
        .await
        .context("create cloudflare record")?;

    let status = resp.status();
    let text = resp.text().await.context("read create response")?;
    if !status.is_success() {
        anyhow::bail!("cloudflare create failed: {} {}", status, text);
    }

    println!("create response: {}", text);
    Ok(())
}

async fn update_record(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    record_id: &str,
    record: &CloudflareRecord,
) -> Result<()> {
    let url = format!("{}/{}", base_url, record_id);
    let resp = client
        .patch(&url)
        .header("Authorization", format!("Bearer {}", token))
        .header("Content-Type", "application/json")
        .json(record)
        .send()
        .await
        .context("update cloudflare record")?;

    let status = resp.status();
    let text = resp.text().await.context("read update response")?;
    if !status.is_success() {
        anyhow::bail!("cloudflare update failed: {} {}", status, text);
    }

    println!("update response: {}", text);
    Ok(())
}

async fn delete_record(
    client: &reqwest::Client,
    token: &str,
    base_url: &str,
    record_id: &str,
) -> Result<()> {
    let url = format!("{}/{}", base_url, record_id);
    let resp = client
        .delete(&url)
        .header("Authorization", format!("Bearer {}", token))
        .send()
        .await
        .context("delete cloudflare record")?;

    let status = resp.status();
    if !status.is_success() {
        anyhow::bail!("cloudflare delete failed: {}", status);
    }

    Ok(())
}
