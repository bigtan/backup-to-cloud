use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use reqwest::blocking::{multipart, Client};
use serde::{Deserialize, Serialize};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

const BASE_URL: &str = "https://pan.baidu.com/rest/2.0/xpan/";
const OAUTH_URL: &str = "https://openapi.baidu.com/oauth/2.0/";
const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4MB per chunk

#[derive(Debug, Serialize, Deserialize)]
struct TokenData {
    access_token: String,
    refresh_token: Option<String>,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: i64,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    errno: i32,
    baidu_name: Option<String>,
    total: Option<u64>,
    used: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct PrecreateResponse {
    errno: i32,
    uploadid: Option<String>,
    #[allow(dead_code)]
    return_type: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct CreateResponse {
    errno: i32,
    #[allow(dead_code)]
    fs_id: Option<u64>,
}

/// Baidu Pan uploader with OAuth2 authentication
pub struct BaiduPanUploader {
    app_key: String,
    app_secret: String,
    config_file: PathBuf,
    token_data: Option<TokenData>,
    client: Client,
}

impl BaiduPanUploader {
    /// Create a new Baidu Pan uploader
    pub fn new(app_key: String, app_secret: String, config_file: Option<PathBuf>) -> Result<Self> {
        let config_file = config_file.unwrap_or_else(|| {
            let home_dir = dirs::home_dir().expect("Failed to get home directory");
            let config_dir = home_dir.join(".baidu");
            std::fs::create_dir_all(&config_dir).ok();
            config_dir.join("baidu_pan_config.json")
        });

        let mut uploader = Self {
            app_key,
            app_secret,
            config_file,
            token_data: None,
            client: Client::new(),
        };

        uploader.load_tokens()?;

        // --- 逻辑修正部分 ---
        if uploader.token_data.is_none() {
            // 情况1: 本地没有任何token, 必须手动授权
            info!("No local token found. Starting authorization flow.");
            uploader.perform_authorization()?;
        } else if !uploader.is_token_valid() {
            // 情况2: Token存在但已过期, 尝试自动续期
            info!("Access token expired. Attempting to refresh...");
            match uploader.refresh_access_token() {
                Ok(_) => {
                    info!("Access token refreshed successfully during init.");
                }
                Err(e) => {
                    // 自动续期失败 (例如 refresh_token 也过期了)
                    warn!(
                        "Failed to refresh token: {}. Falling back to manual authorization.",
                        e
                    );
                    uploader.perform_authorization()?;
                }
            }
        }
        // 情况3: Token存在且有效, 无需任何操作
        // --- 逻辑修正结束 ---

        Ok(uploader)
    }

    /// Load tokens from config file
    fn load_tokens(&mut self) -> Result<()> {
        if !self.config_file.exists() {
            warn!("Token config file not found");
            return Ok(());
        }

        let mut file = File::open(&self.config_file)
            .context("Failed to open token config file")?;
        let mut contents = String::new();
        file.read_to_string(&mut contents)
            .context("Failed to read token config file")?;

        let token_data: TokenData = serde_json::from_str(&contents)
            .context("Failed to parse token data")?;

        self.token_data = Some(token_data);
        info!("Tokens loaded from config file");
        Ok(())
    }

    /// Save tokens to config file
    fn save_tokens(&mut self, token_response: TokenResponse) -> Result<()> {
        let expires_at = Utc::now() + Duration::seconds(token_response.expires_in - 300);

        let token_data = TokenData {
            access_token: token_response.access_token,
            refresh_token: token_response.refresh_token.or_else(|| {
                self.token_data
                    .as_ref()
                    .and_then(|t| t.refresh_token.clone())
            }),
            expires_at,
        };

        let json = serde_json::to_string_pretty(&token_data)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&self.config_file)
            .context("Failed to open config file for writing")?;

        file.write_all(json.as_bytes())
            .context("Failed to write token data")?;

        self.token_data = Some(token_data);
        info!("Tokens saved successfully");
        Ok(())
    }

    /// Check if current token is valid
    fn is_token_valid(&self) -> bool {
        if let Some(token_data) = &self.token_data {
            Utc::now() < token_data.expires_at
        } else {
            false
        }
    }

    /// Get authorization URL
    fn get_authorization_url(&self) -> String {
        format!(
            "{}authorize?response_type=code&client_id={}&redirect_uri=oob&scope=basic,netdisk",
            OAUTH_URL, self.app_key
        )
    }

    /// Get access token using authorization code
    fn get_access_token(&mut self, code: &str) -> Result<()> {
        info!("Requesting access token with authorization code");
        let url = format!(
            "{}token?grant_type=authorization_code&code={}&client_id={}&client_secret={}&redirect_uri=oob",
            OAUTH_URL, code, self.app_key, self.app_secret
        );

        let response = self.client.get(&url).send()?;
        let token_response: TokenResponse = response.json()?;

        if let Some(error) = token_response.error {
            anyhow::bail!(
                "Failed to get access token: {}",
                token_response.error_description.unwrap_or(error)
            );
        }

        self.save_tokens(token_response)?;
        info!("Access token obtained successfully");
        Ok(())
    }

    /// Refresh access token
    fn refresh_access_token(&mut self) -> Result<()> {
        let refresh_token = self
            .token_data
            .as_ref()
            .and_then(|t| t.refresh_token.as_ref())
            .context("No refresh token available")?;

        info!("Refreshing access token");
        let url = format!(
            "{}token?grant_type=refresh_token&refresh_token={}&client_id={}&client_secret={}",
            OAUTH_URL, refresh_token, self.app_key, self.app_secret
        );

        let response = self.client.get(&url).send()?;
        let token_response: TokenResponse = response.json()?;

        if let Some(error) = token_response.error {
            anyhow::bail!(
                "Failed to refresh token: {}",
                token_response.error_description.unwrap_or(error)
            );
        }

        self.save_tokens(token_response)?;
        info!("Access token refreshed successfully");
        Ok(())
    }

    /// Get valid access token, refreshing if necessary
    fn get_valid_access_token(&mut self) -> Result<String> {
        if !self.is_token_valid() {
            self.refresh_access_token()?;
        }

        Ok(self
            .token_data
            .as_ref()
            .map(|t| t.access_token.clone())
            .context("No access token available")?)
    }

    /// Perform authorization flow
    fn perform_authorization(&mut self) -> Result<()> {
        let auth_url = self.get_authorization_url();
        info!("Authorization required for first use or expired token");
        info!("1. 在浏览器中打开以下链接: {}", auth_url);
        info!("2. 登录并授权后，你将得到一个授权码 (code)。");

        println!("3. 请将授权码粘贴到此处并按 Enter: ");
        let mut code = String::new();
        std::io::stdin().read_line(&mut code)?;

        self.get_access_token(code.trim())?;

        // Verify authorization
        let user_info = self.get_user_info()?;
        if user_info.errno == 0 {
            info!(
                "授权成功！你好, {}!",
                user_info.baidu_name.unwrap_or_else(|| "用户".to_string())
            );
            if let (Some(total), Some(used)) = (user_info.total, user_info.used) {
                info!(
                    "网盘总空间: {:.2} GB",
                    total as f64 / (1024_f64.powi(3))
                );
                info!("已用空间: {:.2} GB", used as f64 / (1024_f64.powi(3)));
            }
        } else {
            anyhow::bail!("Failed to get user info");
        }

        Ok(())
    }

    /// Get user information
    fn get_user_info(&mut self) -> Result<UserInfo> {
        let access_token = self.get_valid_access_token()?;
        let url = format!(
            "https://pan.baidu.com/rest/2.0/xpan/nas?method=uinfo&access_token={}",
            access_token
        );

        let response = self.client.get(&url).send()?;
        let user_info: UserInfo = response.json()?;
        Ok(user_info)
    }

    /// Calculate MD5 hash of file chunks
    fn calculate_block_list(&self, file_path: &Path) -> Result<Vec<String>> {
        let mut file = File::open(file_path)?;
        let mut block_list = Vec::new();
        let mut buffer = vec![0u8; CHUNK_SIZE];

        loop {
            let bytes_read = file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }
            let digest = md5::compute(&buffer[..bytes_read]);
            block_list.push(format!("{:x}", digest));
        }

        Ok(block_list)
    }

    /// Upload file to Baidu Pan
    pub fn upload(&mut self, file_path: &str, dest_path: &str) -> Result<bool> {
        let access_token = self.get_valid_access_token()?;
        let local_path = Path::new(file_path);

        if !local_path.exists() {
            anyhow::bail!("Local file not found: {}", file_path);
        }

        let file_name = local_path
            .file_name()
            .and_then(|n| n.to_str())
            .context("Invalid file name")?;
        let remote_full_path = format!("{}/{}", dest_path.trim_end_matches('/'), file_name);
        let file_size = local_path.metadata()?.len();

        info!(
            "Starting upload: {} ({} bytes) to {}",
            file_path, file_size, remote_full_path
        );

        // 1. Calculate block list (MD5 of each chunk)
        debug!("Calculating block list...");
        let block_list = self.calculate_block_list(local_path)?;

        // 2. Precreate
        debug!("Sending precreate request...");
        let precreate_url = format!("{}file?method=precreate&access_token={}", BASE_URL, access_token);
        let precreate_data = serde_json::json!({
            "path": remote_full_path,
            "size": file_size,
            "isdir": 0,
            "autoinit": 1,
            "block_list": serde_json::to_string(&block_list)?,
        });

        let response = self
            .client
            .post(&precreate_url)
            .form(&precreate_data)
            .send()?;
        let precreate_result: PrecreateResponse = response.json()?;

        if precreate_result.errno != 0 {
            anyhow::bail!("Pre-upload failed: errno {}", precreate_result.errno);
        }

        let upload_id = precreate_result
            .uploadid
            .context("No upload ID returned")?;
        info!("Pre-upload successful. Upload ID: {}", upload_id);

        // 3. Upload chunks
        let mut file = File::open(local_path)?;
        let mut buffer = vec![0u8; CHUNK_SIZE];

        for (i, _) in block_list.iter().enumerate() {
            info!("Uploading chunk {}/{}", i + 1, block_list.len());

            let bytes_read = file.read(&mut buffer)?;
            if bytes_read == 0 {
                break;
            }

            let upload_url = format!(
                "https://d.pcs.baidu.com/rest/2.0/pcs/superfile2?method=upload&access_token={}&type=tmpfile&path={}&uploadid={}&partseq={}",
                access_token, remote_full_path, upload_id, i
            );

            let part = multipart::Part::bytes(buffer[..bytes_read].to_vec())
                .file_name("file");
            let form = multipart::Form::new().part("file", part);

            let response = self.client.post(&upload_url).multipart(form).send()?;

            if !response.status().is_success() {
                anyhow::bail!("Chunk {} upload failed: {}", i, response.status());
            }
        }

        info!("All chunks uploaded successfully");

        // 4. Create file
        debug!("Creating file...");
        let create_url = format!("{}file?method=create&access_token={}", BASE_URL, access_token);
        let create_data = serde_json::json!({
            "path": remote_full_path,
            "size": file_size,
            "isdir": 0,
            "uploadid": upload_id,
            "block_list": serde_json::to_string(&block_list)?,
        });

        let response = self.client.post(&create_url).form(&create_data).send()?;
        let create_result: CreateResponse = response.json()?;

        if create_result.errno == 0 {
            info!("File uploaded successfully to: {}", remote_full_path);
            Ok(true)
        } else {
            anyhow::bail!("Failed to create file: errno {}", create_result.errno);
        }
    }
}