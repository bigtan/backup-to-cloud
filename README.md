# backup-to-cloud

A general-purpose backup tool that packages directories or files into a dated
`tar.zst` archive and uploads to Baidu Pan and Cloud189. It supports multiple
backup targets defined in a TOML config file.

## Features
- Package a directory or file into `tar.zst` (zstd high compression)
- Optional: run a command to generate a file, then archive it
- Append date to archive name
- Upload to Baidu Pan with automatic token caching/refresh
- Upload to Cloud189 with session caching
- Enable either or both clouds per config
- Multiple backup entries in one config
- Optional keep-or-delete archive after upload

## Build
```bash
cargo build --release
```

## Configuration
Create `backup.toml` (see `backup.example.toml`):
```toml
[app]
# baidu_enabled = true
baidu_app_key = "your_baidu_app_key"
baidu_app_secret = "your_baidu_app_secret"
# baidu_config = "C:/path/to/baidu_pan_config.json"
# cloud189_enabled = true
# cloud189_config = "C:/path/to/cloud189/config.json"
# cloud189_username = "your_cloud189_username"
# cloud189_password = "your_cloud189_password"
# cloud189_use_qr = false

[[backups]]
source_dir = "/srv/data/project-a"
remote_dir = "/backups/project-a"
archive_name = "project-a"
keep_archive = false

[[backups]]
command = "mysqldump -u root -pYourPass mydb > /var/backups/mysql/mydb-{date}.sql"
source_path = "/var/backups/mysql/mydb-{date}.sql"
keep_command_source = false
remote_dir = "/backups/mysql/{archive_name}/{date}"
archive_name = "mydb"
keep_archive = false
```

- `archive_name` becomes `archive_name-YYYYMMDD.tar.zst`; if that file exists, a numeric suffix is appended
- `keep_archive` defaults to `false`
- `source_path` can be a file or directory; `source_dir` is kept for compatibility
- `command` runs in the system shell (`cmd /C` on Windows, `sh -c` on Unix)
- `command_workdir` sets the working directory for `command`
- `keep_command_source` defaults to `true` and only applies when `command` is set
- Command content is not logged to avoid leaking secrets in logs
- Normal file/directory backups never modify the source data
- `command`, `command_workdir`, `source_dir`, `source_path`, and `remote_dir` support placeholders: `{date}` and `{archive_name}`
- Cloud189 credentials can be provided via config or env: `CLOUD189_USERNAME`, `CLOUD189_PASSWORD`, `CLOUD189_USE_QR=1`
- `baidu_app_key` / `baidu_app_secret` also accept legacy keys `app_key` / `app_secret`
- `baidu_enabled` / `cloud189_enabled` default to `false`; only enabled when explicitly set to `true`
- When `baidu_enabled = true`, both `baidu_app_key` and `baidu_app_secret` are required
- When `cloud189_enabled = true`, set either `cloud189_use_qr = true` or provide both username/password (config or env)
- Backup items continue running even if one item fails; the process exits with an error summary when any failures occurred

## Run
```bash
backup-to-cloud backup.toml
```

If no config path is provided, it defaults to `backup.toml` in the current
directory.

## systemd (daily at 02:00)
Edit the placeholders in these files:
- `backup-to-cloud.service`
- `backup-to-cloud.timer`

Install (system scope):
```bash
sudo cp backup-to-cloud.service /etc/systemd/system/
sudo cp backup-to-cloud.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now backup-to-cloud.timer
```

## Case: backup for vaultwarden (docker compose, sqlite)
If you use bind mounts and sqlite, you can back up the compose directory and its
data files directly. A minimal example:
```toml
[[backups]]
source_dir = "/srv/vaultwarden"
remote_dir = "/backups/vaultwarden"
archive_name = "vaultwarden"
keep_archive = false
```

Notes:
- Ensure the bind-mounted data (sqlite db, attachments, config) lives inside the
  compose directory so it is included.
- For stronger consistency, you can stop the service briefly during backup.
