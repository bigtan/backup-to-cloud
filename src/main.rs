use anyhow::{Context, Result};
use chrono::Local;
use estan::uploader::{BaiduPanUploader, Cloud189Uploader, Uploader};
use serde::Deserialize;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info, warn};

#[derive(Debug, Deserialize)]
struct Config {
    app: AppConfig,
    backups: Vec<BackupItem>,
}

#[derive(Debug, Deserialize)]
struct AppConfig {
    #[serde(default)]
    baidu_enabled: Option<bool>,
    #[serde(alias = "app_key")]
    baidu_app_key: Option<String>,
    #[serde(alias = "app_secret")]
    baidu_app_secret: Option<String>,
    baidu_config: Option<String>,
    #[serde(default)]
    cloud189_enabled: Option<bool>,
    cloud189_config: Option<String>,
    cloud189_username: Option<String>,
    cloud189_password: Option<String>,
    cloud189_use_qr: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct BackupItem {
    source_dir: Option<String>,
    source_path: Option<String>,
    command: Option<String>,
    command_workdir: Option<String>,
    keep_command_source: Option<bool>,
    remote_dir: String,
    archive_name: String,
    keep_archive: Option<bool>,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    let mut args = env::args().skip(1);
    let config_path = args.next().unwrap_or_else(|| "backup.toml".to_string());
    if args.next().is_some() {
        anyhow::bail!("Too many arguments");
    }

    let config = load_config(&config_path)?;
    let AppConfig {
        baidu_enabled,
        baidu_app_key,
        baidu_app_secret,
        baidu_config,
        cloud189_enabled,
        cloud189_config,
        cloud189_username,
        cloud189_password,
        cloud189_use_qr,
    } = config.app;
    let baidu_config = baidu_config.map(PathBuf::from);
    let cloud189_config = cloud189_config.map(PathBuf::from);

    let has_baidu_key = baidu_app_key
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let has_baidu_secret = baidu_app_secret
        .as_deref()
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let baidu_enabled = baidu_enabled.unwrap_or(false);
    if baidu_enabled && !(has_baidu_key && has_baidu_secret) {
        anyhow::bail!("Baidu uploader enabled but baidu_app_key/baidu_app_secret are incomplete");
    }
    let baidu_uploader = if baidu_enabled {
        let app_key = baidu_app_key.context("Missing baidu_app_key (or app_key)")?;
        let app_secret = baidu_app_secret.context("Missing baidu_app_secret (or app_secret)")?;
        Some(
            Box::new(BaiduPanUploader::new(app_key, app_secret, baidu_config)?)
                as Box<dyn Uploader>,
        )
    } else {
        None
    };

    let cloud189_enabled = cloud189_enabled.unwrap_or(false);
    let cloud189_uploader = if cloud189_enabled {
        let (username, password, use_qr) =
            resolve_cloud189_credentials(cloud189_username, cloud189_password, cloud189_use_qr);
        let username_present = username
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let password_present = password
            .as_deref()
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if !use_qr {
            if username_present ^ password_present {
                anyhow::bail!(
                    "Cloud189 uploader enabled with password login, but username/password are incomplete"
                );
            }
            if !username_present && !password_present {
                anyhow::bail!(
                    "Cloud189 uploader enabled requires either cloud189_use_qr=true or both username/password"
                );
            }
        }
        Some(Box::new(Cloud189Uploader::new(
            cloud189_config,
            username,
            password,
            use_qr,
        )?) as Box<dyn Uploader>)
    } else {
        None
    };

    let mut uploaders: Vec<Box<dyn Uploader>> = Vec::new();
    if let Some(uploader) = baidu_uploader {
        uploaders.push(uploader);
    }
    if let Some(uploader) = cloud189_uploader {
        uploaders.push(uploader);
    }

    if uploaders.is_empty() {
        anyhow::bail!("No cloud uploader enabled");
    }

    let mut failures: Vec<String> = Vec::new();

    for item in config.backups {
        let date = Local::now().format("%Y%m%d").to_string();
        let base_name = normalize_archive_name(&item.archive_name);
        let source_path = resolve_source_path(&item, &date, base_name)?;
        if let Some(command) = item.command.as_deref() {
            let expanded_command = expand_placeholders(command, &date, base_name);
            info!("Running command for backup item: {}", base_name);
            let workdir = item
                .command_workdir
                .as_deref()
                .map(|dir| expand_placeholders(dir, &date, base_name));
            if let Err(err) = run_command(&expanded_command, workdir.as_deref()) {
                let message = format!("[{base_name}] command failed: {err}");
                error!("{}", message);
                failures.push(message);
                continue;
            }
        }

        if !source_path.exists() {
            let message = format!("[{base_name}] source path not found: {}", source_path.display());
            error!("{}", message);
            failures.push(message);
            continue;
        }
        if !source_path.is_dir() && !source_path.is_file() {
            let message = format!(
                "[{base_name}] source path is not a file or directory: {}",
                source_path.display()
            );
            error!("{}", message);
            failures.push(message);
            continue;
        }

        let archive_path = build_archive_path(base_name, &date)?;
        info!("Creating archive: {}", archive_path.display());
        if let Err(err) = create_archive(&source_path, &archive_path) {
            let message = format!("[{base_name}] create archive failed: {err}");
            error!("{}", message);
            failures.push(message);
            continue;
        }

        let remote_dir = expand_placeholders(&item.remote_dir, &date, base_name);
        let mut upload_failed = false;
        for uploader in uploaders.iter_mut() {
            info!("Uploading to {}", uploader.name());
            if let Err(err) = uploader.upload(
                archive_path
                    .to_str()
                    .context("Archive path is not valid UTF-8")?,
                &remote_dir,
            ) {
                upload_failed = true;
                let message = format!("[{base_name}] upload failed on {}: {}", uploader.name(), err);
                error!("{}", message);
                failures.push(message);
            }
        }

        if upload_failed {
            warn!(
                "Archive retained because one or more uploads failed: {}",
                archive_path.display()
            );
            continue;
        }

        if !item.keep_archive.unwrap_or(false) {
            fs::remove_file(&archive_path).with_context(|| {
                format!(
                    "Failed to remove archive file after upload: {}",
                    archive_path.display()
                )
            })?;
        }
        if item.command.is_some()
            && !item.keep_command_source.unwrap_or(true)
            && source_path.is_file()
        {
            fs::remove_file(&source_path).with_context(|| {
                format!(
                    "Failed to remove command output file: {}",
                    source_path.display()
                )
            })?;
        }
    }

    if !failures.is_empty() {
        anyhow::bail!(
            "Backup finished with {} failure(s):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }

    info!("Backup uploaded successfully");
    Ok(())
}

fn load_config(path: &str) -> Result<Config> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file: {}", path))?;
    let config: Config = toml::from_str(&contents).context("Failed to parse config file")?;
    if config.backups.is_empty() {
        anyhow::bail!("No backups configured");
    }
    Ok(config)
}

fn resolve_cloud189_credentials(
    username: Option<String>,
    password: Option<String>,
    use_qr: Option<bool>,
) -> (Option<String>, Option<String>, bool) {
    let username = username.or_else(|| env::var("CLOUD189_USERNAME").ok());
    let password = password.or_else(|| env::var("CLOUD189_PASSWORD").ok());
    let use_qr = use_qr
        .or_else(|| env::var("CLOUD189_USE_QR").ok().and_then(parse_env_bool))
        .unwrap_or(false);
    (username, password, use_qr)
}

fn parse_env_bool(value: String) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn normalize_archive_name(archive_name: &str) -> &str {
    if archive_name.trim().is_empty() {
        "backup"
    } else {
        archive_name.trim()
    }
}

fn expand_placeholders(input: &str, date: &str, archive_name: &str) -> String {
    input
        .replace("{date}", date)
        .replace("{archive_name}", archive_name)
}

fn build_archive_path(archive_name: &str, date: &str) -> Result<PathBuf> {
    let file_name = format!("{archive_name}-{date}.tar.zst");
    let cwd = env::current_dir()?;
    let mut output_path = cwd.join(&file_name);
    if output_path.exists() {
        let mut counter = 1usize;
        loop {
            let candidate = cwd.join(format!("{archive_name}-{date}-{counter}.tar.zst"));
            if !candidate.exists() {
                output_path = candidate;
                break;
            }
            counter += 1;
        }
    }
    Ok(output_path)
}

fn resolve_source_path(item: &BackupItem, date: &str, archive_name: &str) -> Result<PathBuf> {
    let candidate = item
        .source_path
        .as_deref()
        .or(item.source_dir.as_deref())
        .context("Missing source_path/source_dir in backup item")?;
    let expanded = expand_placeholders(candidate, date, archive_name);
    let trimmed = expanded.trim();
    if trimmed.is_empty() {
        anyhow::bail!("source_path/source_dir cannot be empty");
    }
    Ok(PathBuf::from(trimmed))
}

fn run_command(command: &str, workdir: Option<&str>) -> Result<()> {
    let mut cmd = if cfg!(windows) {
        let mut command_builder = Command::new("cmd");
        command_builder.args(["/C", command]);
        command_builder
    } else {
        let mut command_builder = Command::new("sh");
        command_builder.args(["-c", command]);
        command_builder
    };

    if let Some(dir) = workdir {
        let dir_path = Path::new(dir);
        if !dir_path.is_dir() {
            anyhow::bail!("Command workdir is not a directory: {}", dir);
        }
        cmd.current_dir(dir_path);
    }

    let status = cmd
        .status()
        .with_context(|| format!("Failed to run command: {}", command))?;
    if !status.success() {
        anyhow::bail!("Command failed with exit code: {}", status);
    }
    Ok(())
}

fn create_archive(source_path: &Path, output_path: &Path) -> Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("Failed to create archive file: {}", output_path.display()))?;
    let encoder = zstd::Encoder::new(file, 10).context("Failed to initialize zstd encoder")?;
    let mut builder = tar::Builder::new(encoder);

    let base_name = source_path
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("backup");

    if source_path.is_dir() {
        builder
            .append_dir_all(base_name, source_path)
            .with_context(|| format!("Failed to append directory: {}", source_path.display()))?;
    } else if source_path.is_file() {
        builder
            .append_path_with_name(source_path, base_name)
            .with_context(|| format!("Failed to append file: {}", source_path.display()))?;
    } else {
        anyhow::bail!(
            "Source path is not a file or directory: {}",
            source_path.display()
        );
    }
    builder.finish().context("Failed to finish tar archive")?;
    let encoder = builder
        .into_inner()
        .context("Failed to finalize tar builder")?;
    encoder.finish().context("Failed to finish zstd encoding")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_placeholders() {
        let result = expand_placeholders("/a/{archive_name}/{date}", "20260211", "demo");
        assert_eq!(result, "/a/demo/20260211");
    }

    #[test]
    fn test_parse_env_bool() {
        assert_eq!(parse_env_bool("1".to_string()), Some(true));
        assert_eq!(parse_env_bool("off".to_string()), Some(false));
        assert_eq!(parse_env_bool("invalid".to_string()), None);
    }

    #[test]
    fn test_normalize_archive_name() {
        assert_eq!(normalize_archive_name("  "), "backup");
        assert_eq!(normalize_archive_name(" project-a "), "project-a");
    }
}
