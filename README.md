# backup-to-baidu

A general-purpose backup tool that packages directories or files into a dated
`tar.zst` archive and uploads to Baidu Pan. It supports multiple backup targets
defined in a TOML config file.

## Features
- Package a directory or file into `tar.zst` (zstd high compression)
- Optional: run a command to generate a file, then archive it
- Append date to archive name
- Upload to Baidu Pan with automatic token caching/refresh
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
app_key = "your_app_key"
app_secret = "your_app_secret"
# baidu_config = "C:/path/to/baidu_pan_config.json"

[[backups]]
source_dir = "D:/data/project-a"
remote_dir = "/backups/project-a"
archive_name = "project-a"
keep_archive = false

[[backups]]
command = "mysqldump -u root -pYourPass mydb > D:/backup/mysql/mydb.sql"
source_path = "D:/backup/mysql/mydb.sql"
keep_command_source = false
remote_dir = "/backups/mysql"
archive_name = "mydb"
keep_archive = false
```

- `archive_name` becomes `archive_name-YYYYMMDD.tar.zst`
- `keep_archive` defaults to `false`
- `source_path` can be a file or directory; `source_dir` is kept for compatibility
- `command` runs in the system shell (`cmd /C` on Windows, `sh -c` on Unix)
- `command_workdir` sets the working directory for `command`
- `keep_command_source` defaults to `true` and only applies when `command` is set
- Normal file/directory backups never modify the source data

## Run
```bash
backup-to-baidu backup.toml
```

If no config path is provided, it defaults to `backup.toml` in the current
directory.

## systemd (daily at 02:00)
Edit the placeholders in these files:
- `backup-to-baidu.service`
- `backup-to-baidu.timer`

Install (system scope):
```bash
sudo cp backup-to-baidu.service /etc/systemd/system/
sudo cp backup-to-baidu.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now backup-to-baidu.timer
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
