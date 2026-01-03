mod baidu;

use anyhow::{Context, Result};
use baidu::BaiduPanUploader;
use chrono::Local;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::info;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Config {
    app: AppConfig,
    backups: Vec<BackupItem>,
}

#[derive(Debug, Deserialize)]
struct AppConfig {
    app_key: String,
    app_secret: String,
    baidu_config: Option<String>,
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
    let baidu_config = config.app.baidu_config.map(PathBuf::from);

    let mut uploader =
        BaiduPanUploader::new(config.app.app_key, config.app.app_secret, baidu_config)?;

    for item in config.backups {
        let source_path = resolve_source_path(&item)?;
        if let Some(command) = item.command.as_deref() {
            info!("Running command: {}", command);
            run_command(command, item.command_workdir.as_deref())?;
        }

        if !source_path.exists() {
            anyhow::bail!("Source path not found: {}", source_path.display());
        }
        if !source_path.is_dir() && !source_path.is_file() {
            anyhow::bail!("Source path is not a file or directory: {}", source_path.display());
        }

        let archive_path = build_archive_path(&item.archive_name)?;
        info!("Creating archive: {}", archive_path.display());
        create_archive(&source_path, &archive_path)?;

        uploader.upload(
            archive_path
                .to_str()
                .context("Archive path is not valid UTF-8")?,
            &item.remote_dir,
        )?;
        if !item.keep_archive.unwrap_or(false) {
            fs::remove_file(&archive_path).with_context(|| {
                format!(
                    "Failed to remove archive file after upload: {}",
                    archive_path.display()
                )
            })?;
        }
        if item.command.is_some() && !item.keep_command_source.unwrap_or(true) {
            if source_path.is_file() {
                fs::remove_file(&source_path).with_context(|| {
                    format!(
                        "Failed to remove command output file: {}",
                        source_path.display()
                    )
                })?;
            }
        }
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

fn build_archive_path(archive_name: &str) -> Result<PathBuf> {
    let date = Local::now().format("%Y%m%d").to_string();
    let base_name = if archive_name.trim().is_empty() {
        "backup"
    } else {
        archive_name.trim()
    };
    let file_name = format!("{base_name}-{date}.tar.zst");
    let output_path = env::current_dir()?.join(file_name);
    Ok(output_path)
}

fn resolve_source_path(item: &BackupItem) -> Result<PathBuf> {
    let candidate = item
        .source_path
        .as_deref()
        .or(item.source_dir.as_deref())
        .context("Missing source_path/source_dir in backup item")?;
    let trimmed = candidate.trim();
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
    let encoder = zstd::Encoder::new(file, 19)
        .context("Failed to initialize zstd encoder")?;
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
        anyhow::bail!("Source path is not a file or directory: {}", source_path.display());
    }
    builder.finish().context("Failed to finish tar archive")?;
    let encoder = builder.into_inner().context("Failed to finalize tar builder")?;
    encoder.finish().context("Failed to finish zstd encoding")?;
    Ok(())
}
