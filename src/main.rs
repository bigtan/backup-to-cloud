mod baidu;

use anyhow::{Context, Result};
use baidu::BaiduPanUploader;
use chrono::Local;
use std::env;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
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
    source_dir: String,
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
        let source_path = Path::new(&item.source_dir);
        if !source_path.is_dir() {
            anyhow::bail!(
                "Source directory not found or not a directory: {}",
                item.source_dir
            );
        }

        let archive_path = build_archive_path(&item.archive_name)?;
        info!("Creating archive: {}", archive_path.display());
        create_archive(source_path, &archive_path)?;

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

fn create_archive(source_dir: &Path, output_path: &Path) -> Result<()> {
    let file = File::create(output_path)
        .with_context(|| format!("Failed to create archive file: {}", output_path.display()))?;
    let encoder = zstd::Encoder::new(file, 19)
        .context("Failed to initialize zstd encoder")?;
    let mut builder = tar::Builder::new(encoder);

    let base_name = source_dir
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("backup");

    builder
        .append_dir_all(base_name, source_dir)
        .with_context(|| format!("Failed to append directory: {}", source_dir.display()))?;
    builder.finish().context("Failed to finish tar archive")?;
    let encoder = builder.into_inner().context("Failed to finalize tar builder")?;
    encoder.finish().context("Failed to finish zstd encoding")?;
    Ok(())
}
