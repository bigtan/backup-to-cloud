use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use std::time::SystemTime;

use aes::Aes128;
use anyhow::{anyhow, Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STD;
use base64::Engine;
use cipher::generic_array::GenericArray;
use cipher::{BlockEncrypt, KeyInit};
use chrono::Local;
use hmac::{Hmac, Mac};
use httpdate::fmt_http_date;
use quick_xml::de::from_str as xml_from_str;
use reqwest::blocking::{Client, Response};
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use rsa::pkcs8::DecodePublicKey;
use rsa::rand_core::OsRng;
use rsa::{Pkcs1v15Encrypt, RsaPublicKey};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use sha1::Sha1;
use tracing::{info, warn};
use url::Url;
use uuid::Uuid;
use urlencoding::decode as url_decode;

const API_BASE: &str = "https://api.cloud.189.cn";
const UPLOAD_BASE: &str = "https://upload.cloud.189.cn";
const LOGIN_URL: &str = "https://cloud.189.cn/unifyLoginForPC.action";
const OPEN_APP_CONF_URL: &str = "https://open.e.189.cn/api/logbox/oauth2/appConf.do";
const OPEN_ENCRYPT_CONF_URL: &str = "https://open.e.189.cn/api/logbox/config/encryptConf.do";
const OPEN_LOGIN_SUBMIT_URL: &str = "https://open.e.189.cn/api/logbox/oauth2/loginSubmit.do";
const ROOT_FOLDER_ID: &str = "-11";
const SLICE_SIZE: usize = 10 * 1024 * 1024;
const UPLOAD_PART_MAX_RETRIES: u32 = 3;
const UPLOAD_PART_BACKOFF_BASE_MS: u64 = 800;
const UPLOAD_REQ_MAX_RETRIES: u32 = 3;
const UPLOAD_REQ_BACKOFF_BASE_MS: u64 = 800;

#[derive(Debug, Serialize, Deserialize, Default)]
struct Cloud189Config {
    #[serde(default)]
    user: Option<User>,
    #[serde(default)]
    session: Option<Session>,
    #[serde(default)]
    sson: Option<String>,
    #[serde(default)]
    auth: Option<String>,
    #[serde(skip)]
    path: PathBuf,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct User {
    #[serde(default)]
    name: String,
    #[serde(default)]
    password: String,
}

#[derive(Debug, Serialize, Deserialize, Default, Clone)]
struct Session {
    #[serde(default, rename = "loginName")]
    login_name: String,
    #[serde(default, rename = "sessionKey")]
    key: String,
    #[serde(default, rename = "sessionSecret")]
    secret: String,
    #[serde(default, rename = "keepAlive")]
    keep_alive: i64,
    #[serde(default, rename = "getFileDiffSpan")]
    file_diff_span: i64,
    #[serde(default, rename = "getUserInfoSpan")]
    user_info_span: i64,
    #[serde(default, rename = "familySessionKey")]
    family_key: String,
    #[serde(default, rename = "familySessionSecret")]
    family_secret: String,
    #[serde(default, rename = "accessToken")]
    access_token: String,
    #[serde(default, rename = "refreshToken")]
    refresh_token: String,
}

impl Session {
    fn is_valid(&self) -> bool {
        !self.key.is_empty() && !self.secret.is_empty()
    }
}

#[derive(Debug)]
pub struct Cloud189Uploader {
    client: Cloud189Client,
}

impl Cloud189Uploader {
    pub fn new(
        config_path: Option<PathBuf>,
        username: Option<String>,
        password: Option<String>,
        use_qr: bool,
    ) -> Result<Self> {
        let config_path = config_path.unwrap_or_else(default_config_path);
        let mut config = load_config(&config_path)?;
        config.path = config_path.clone();

        let jar = Arc::new(Jar::default());
        let client = Client::builder()
            .cookie_provider(jar.clone())
            .build()
            .context("create http client")?;

        let mut api = Cloud189Client {
            client,
            jar,
            config,
        };
        api.init_cookies_from_config();

        if !api.has_valid_session() {
            if use_qr || username.is_none() || password.is_none() {
                api.login_qr()?;
            } else {
                let user = username.ok_or_else(|| anyhow!("missing CLOUD189_USERNAME"))?;
                let pass = password.ok_or_else(|| anyhow!("missing CLOUD189_PASSWORD"))?;
                api.login(&user, &pass)?;
            }
        }

        Ok(Self { client: api })
    }

    pub fn upload(&mut self, file_path: &str, dest_path: &str) -> Result<bool> {
        let remote_dir = if dest_path.trim().is_empty() {
            "/"
        } else {
            dest_path
        };
        if !remote_dir.starts_with('/') {
            anyhow::bail!("Cloud189 dest path must start with '/'");
        }
        let folder_id = self.client.resolve_folder(remote_dir, true)?;
        self.client.upload_file(Path::new(file_path), &folder_id)?;
        Ok(true)
    }
}

#[derive(Debug)]
struct Cloud189Client {
    client: Client,
    jar: Arc<Jar>,
    config: Cloud189Config,
}

#[derive(Debug, Deserialize)]
struct LoginSubmitResp {
    #[serde(rename = "result")]
    result: i32,
    #[serde(rename = "msg")]
    msg: String,
    #[serde(rename = "toUrl")]
    to_url: String,
}

#[derive(Debug, Deserialize)]
struct AppConfResp {
    #[serde(rename = "data")]
    data: AppConfData,
}

#[derive(Debug, Deserialize)]
struct AppConfData {
    #[serde(rename = "accountType")]
    account_type: String,
    #[serde(rename = "appKey")]
    app_key: String,
    #[serde(rename = "clientType")]
    client_type: i32,
    #[serde(rename = "isOauth2")]
    is_oauth2: bool,
    #[serde(rename = "mailSuffix")]
    mail_suffix: String,
    #[serde(rename = "paramId")]
    param_id: String,
    #[serde(rename = "returnUrl")]
    return_url: String,
}

#[derive(Debug, Deserialize)]
struct EncryptConfResp {
    #[serde(rename = "data")]
    data: EncryptConfData,
}

#[derive(Debug, Deserialize)]
struct EncryptConfData {
    #[serde(rename = "pre")]
    pre: String,
    #[serde(rename = "pubKey")]
    pub_key: String,
}

#[derive(Debug, Deserialize)]
struct SessionResp {
    #[serde(rename = "loginName", default)]
    login_name: String,
    #[serde(rename = "sessionKey", default)]
    key: String,
    #[serde(rename = "sessionSecret", default)]
    secret: String,
    #[serde(rename = "accessToken", default)]
    access_token: String,
    #[serde(rename = "refreshToken", default)]
    refresh_token: String,
}

#[derive(Debug, Deserialize)]
struct ListFilesResp {
    #[serde(rename = "res_code")]
    res_code: i32,
    #[serde(rename = "res_message")]
    res_message: String,
    #[serde(rename = "fileListAO")]
    file_list: FileListAo,
}

#[derive(Debug, Deserialize)]
struct FileListAo {
    #[serde(rename = "count")]
    count: i32,
    #[serde(rename = "fileListSize")]
    file_list_size: i32,
    #[serde(rename = "folderList", default)]
    folders: Vec<Folder>,
}

#[derive(Debug, Deserialize)]
struct Folder {
    #[serde(deserialize_with = "de_id")]
    id: String,
    #[serde(rename = "name")]
    name: String,
}

#[derive(Debug, Deserialize)]
struct XmlListFiles {
    #[serde(rename = "fileList")]
    file_list: XmlFileList,
}

#[derive(Debug, Deserialize)]
struct XmlFileList {
    #[serde(rename = "count")]
    count: Option<i32>,
    #[serde(rename = "fileListSize")]
    file_list_size: Option<i32>,
    #[serde(rename = "folder", default)]
    folders: Vec<XmlFolder>,
}

#[derive(Debug, Deserialize)]
struct XmlFolder {
    #[serde(rename = "id")]
    id: String,
    #[serde(rename = "name")]
    name: String,
}

struct ListFilesResult {
    folders: Vec<Folder>,
    count: Option<i32>,
    page_size: Option<i32>,
}

#[derive(Debug, Deserialize)]
struct MkdirResp {
    #[serde(rename = "res_code")]
    res_code: i32,
    #[serde(rename = "res_message")]
    res_message: String,
    #[serde(deserialize_with = "de_id")]
    id: String,
}

#[derive(Debug, Deserialize)]
struct XmlMkdirResp {
    #[serde(rename = "res_code")]
    res_code: Option<i32>,
    #[serde(rename = "res_message")]
    res_message: Option<String>,
    #[serde(rename = "id")]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InitUploadResp {
    #[serde(rename = "code")]
    code: String,
    #[serde(rename = "data")]
    data: InitUploadData,
}

#[derive(Debug, Deserialize)]
struct InitUploadData {
    #[serde(rename = "uploadFileId")]
    upload_file_id: String,
    #[serde(rename = "fileDataExists")]
    file_data_exists: i32,
}

#[derive(Debug, Deserialize)]
struct UploadUrlsResp {
    #[serde(rename = "code")]
    code: String,
    #[serde(rename = "uploadUrls")]
    upload_urls: std::collections::HashMap<String, UploadUrl>,
}

#[derive(Debug, Deserialize)]
struct UploadUrl {
    #[serde(rename = "requestURL")]
    request_url: String,
    #[serde(rename = "requestHeader")]
    request_header: String,
}

#[derive(Debug, Deserialize)]
struct CommitUploadResp {
    #[serde(rename = "code")]
    code: String,
}

#[derive(Debug, Deserialize)]
struct QrCodeResp {
    #[serde(rename = "uuid")]
    uuid: String,
    #[serde(rename = "encryuuid")]
    encryuuid: String,
    #[serde(rename = "encodeuuid")]
    encodeuuid: String,
}

#[derive(Debug, Deserialize)]
struct QrCodeState {
    #[serde(rename = "redirectUrl", default)]
    redirect_url: String,
    #[serde(rename = "status")]
    status: i32,
    #[serde(skip)]
    sson: Option<String>,
}

#[derive(Debug)]
struct PartInfo {
    index: usize,
    name: String,
    offset: u64,
    len: u64,
}

#[derive(Debug)]
struct FileHashes {
    file_md5: String,
    slice_md5: String,
    parts: Vec<PartInfo>,
}

fn default_config_path() -> PathBuf {
    let home = dirs::home_dir().expect("home dir");
    home.join(".config").join("cloud189").join("config.json")
}

fn load_config(path: &Path) -> Result<Cloud189Config> {
    if !path.exists() {
        return Ok(Cloud189Config {
            path: path.to_path_buf(),
            ..Default::default()
        });
    }
    let data = fs::read(path).with_context(|| format!("read config {}", path.display()))?;
    let mut cfg: Cloud189Config = serde_json::from_slice(&data).unwrap_or_default();
    cfg.path = path.to_path_buf();
    Ok(cfg)
}

fn save_config(config: &Cloud189Config) -> Result<()> {
    if let Some(parent) = config.path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let data = serde_json::to_vec_pretty(config).context("serialize config")?;
    fs::write(&config.path, data).with_context(|| format!("write {}", config.path.display()))
}

impl Cloud189Client {
    fn init_cookies_from_config(&self) {
        if let Some(sson) = self.config.sson.as_ref() {
            let url = Url::parse("https://e.189.cn").expect("url");
            let cookie = format!("SSON={}", sson);
            self.jar.add_cookie_str(&cookie, &url);
        }
        if let Some(auth) = self.config.auth.as_ref() {
            let cookie = format!("COOKIE_LOGIN_USER={}", auth);
            let url_cloud = Url::parse("https://cloud.189.cn").expect("url");
            let url_mobile = Url::parse("https://m.cloud.189.cn").expect("url");
            self.jar.add_cookie_str(&cookie, &url_cloud);
            self.jar.add_cookie_str(&cookie, &url_mobile);
        }
    }

    fn has_valid_session(&self) -> bool {
        self.config
            .session
            .as_ref()
            .map(|s| s.is_valid())
            .unwrap_or(false)
    }

    fn login(&mut self, username: &str, password: &str) -> Result<()> {
        let params = vec![
            ("appId".to_string(), "9317140619".to_string()),
            ("clientType".to_string(), "10020".to_string()),
            ("timeStamp".to_string(), format!("{}", chrono_millis())),
            (
                "returnURL".to_string(),
                "https://m.cloud.189.cn/zhuanti/2020/loginErrorPc/index.html".to_string(),
            ),
        ];
        let login_url = with_query(LOGIN_URL, &params)?;
        let resp = self
            .client
            .get(login_url)
            .send()
            .context("request login url")?;

        let referer = resp.url().to_string();
        let url = resp.url().clone();
        let lt = url
            .query_pairs()
            .find(|(k, _)| k == "lt")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        let req_id = url
            .query_pairs()
            .find(|(k, _)| k == "reqId")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        let app_key = url
            .query_pairs()
            .find(|(k, _)| k == "appId")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();

        let app_conf = self.fetch_app_conf(&referer, &req_id, &lt, &app_key)?;
        let encrypt_conf = self.fetch_encrypt_conf(&referer)?;
        let (enc_user, enc_pass) =
            encrypt_credentials(&encrypt_conf.pub_key, &encrypt_conf.pre, username, password)?;

        let submit_params = vec![
            ("version".to_string(), "v2.0".to_string()),
            ("appKey".to_string(), app_conf.app_key),
            ("accountType".to_string(), app_conf.account_type),
            ("userName".to_string(), enc_user),
            ("epd".to_string(), enc_pass),
            ("captchaType".to_string(), "".to_string()),
            ("validateCode".to_string(), "".to_string()),
            ("smsValidateCode".to_string(), "".to_string()),
            ("captchaToken".to_string(), "".to_string()),
            ("returnUrl".to_string(), referer.clone()),
            ("mailSuffix".to_string(), app_conf.mail_suffix),
            ("dynamicCheck".to_string(), "FALSE".to_string()),
            ("clientType".to_string(), app_conf.client_type.to_string()),
            ("cb_SaveName".to_string(), "0".to_string()),
            ("isOauth2".to_string(), app_conf.is_oauth2.to_string()),
            ("state".to_string(), "".to_string()),
            ("paramId".to_string(), app_conf.param_id),
        ];

        let resp = self
            .client
            .post(OPEN_LOGIN_SUBMIT_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::REFERER, &referer)
            .header("Reqid", &req_id)
            .header("lt", &lt)
            .body(encode_form_urlencoded(&submit_params))
            .send()
            .context("login submit")?;

        let sson = extract_cookie(&resp, "SSON");
        let submit: LoginSubmitResp = parse_json_response(resp, "login submit")?;
        if submit.result != 0 {
            return Err(anyhow!("login failed: {}", submit.msg));
        }
        self.config.sson = sson.clone();

        let session = self.fetch_session(&submit.to_url)?;
        self.config.session = Some(Session {
            login_name: session.login_name,
            key: session.key,
            secret: session.secret,
            access_token: session.access_token,
            refresh_token: session.refresh_token,
            ..Default::default()
        });
        self.config.user = Some(User {
            name: username.to_string(),
            password: String::new(),
        });

        save_config(&self.config)?;
        Ok(())
    }

    fn login_qr(&mut self) -> Result<()> {
        let params = vec![
            ("appId".to_string(), "9317140619".to_string()),
            ("clientType".to_string(), "10020".to_string()),
            ("timeStamp".to_string(), format!("{}", chrono_millis())),
            (
                "returnURL".to_string(),
                "https://m.cloud.189.cn/zhuanti/2020/loginErrorPc/index.html".to_string(),
            ),
        ];
        let login_url = with_query(LOGIN_URL, &params)?;
        let resp = self
            .client
            .get(login_url)
            .send()
            .context("request login url")?;

        let referer = resp.url().to_string();
        let url = resp.url().clone();
        let lt = url
            .query_pairs()
            .find(|(k, _)| k == "lt")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        let req_id = url
            .query_pairs()
            .find(|(k, _)| k == "reqId")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();
        let app_key = url
            .query_pairs()
            .find(|(k, _)| k == "appId")
            .map(|(_, v)| v.to_string())
            .unwrap_or_default();

        let app_conf = self.fetch_app_conf(&referer, &req_id, &lt, &app_key)?;

        let uuid_url = with_query(
            "https://open.e.189.cn/api/logbox/oauth2/getUUID.do",
            &[("appId".to_string(), app_key)],
        )?;
        let resp = self
            .client
            .get(uuid_url)
            .send()
            .context("get qr uuid")?;
        let qr: QrCodeResp = resp.json().context("decode qr uuid")?;

        let decoded_uuid = url_decode(&qr.encodeuuid)
            .context("decode qr uuid")?
            .into_owned();
        let mut qr_params = url::form_urlencoded::Serializer::new(String::new());
        qr_params.append_pair("REQID", &req_id);
        qr_params.append_pair("uuid", &decoded_uuid);
        let qr_url = format!(
            "https://open.e.189.cn/api/logbox/oauth2/image.do?{}",
            qr_params.finish()
        );

        info!("Open this QR URL in your browser to scan and log in:\n{}\n", qr_url);

        loop {
            let state = self.query_qr_state(&qr, &app_conf, &referer)?;
            match state.status {
                -106 => {
                    info!("QR code not scanned yet. Waiting...");
                }
                -11002 => {
                    info!("QR code scanned but not confirmed. Waiting...");
                }
                0 => {
                    if let Some(sson) = state.sson.clone() {
                        self.config.sson = Some(sson);
                    }
                    let session = self.fetch_session(&state.redirect_url)?;
                    self.config.session = Some(Session {
                        login_name: session.login_name,
                        key: session.key,
                        secret: session.secret,
                        access_token: session.access_token,
                        refresh_token: session.refresh_token,
                        ..Default::default()
                    });
                    save_config(&self.config)?;
                    info!("QR code login successful");
                    return Ok(());
                }
                _ => {
                    warn!("Unexpected QR login status: {}", state.status);
                    return Err(anyhow!("unknown qr login status: {}", state.status));
                }
            }
            sleep(Duration::from_secs(3));
        }
    }

    fn fetch_app_conf(
        &self,
        referer: &str,
        req_id: &str,
        lt: &str,
        app_key: &str,
    ) -> Result<AppConfData> {
        let params = vec![
            ("version".to_string(), "2.0".to_string()),
            ("appKey".to_string(), app_key.to_string()),
        ];
        let resp = self
            .client
            .post(OPEN_APP_CONF_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::ORIGIN, "https://open.e.189.cn")
            .header(reqwest::header::REFERER, referer)
            .header("Reqid", req_id)
            .header("lt", lt)
            .body(encode_form_urlencoded(&params))
            .send()
            .context("get app conf")?;
        let data: AppConfResp = parse_json_response(resp, "app conf")?;
        Ok(data.data)
    }

    fn fetch_encrypt_conf(&self, referer: &str) -> Result<EncryptConfData> {
        let params = vec![("appId".to_string(), "cloud".to_string())];
        let resp = self
            .client
            .post(OPEN_ENCRYPT_CONF_URL)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json")
            .header(reqwest::header::REFERER, referer)
            .body(encode_form_urlencoded(&params))
            .send()
            .context("get encrypt conf")?;
        let data: EncryptConfResp = parse_json_response(resp, "encrypt conf")?;
        Ok(data.data)
    }

    fn query_qr_state(
        &self,
        qr: &QrCodeResp,
        app_conf: &AppConfData,
        referer: &str,
    ) -> Result<QrCodeState> {
        let mut url = Url::parse("https://open.e.189.cn/api/logbox/oauth2/qrcodeLoginState.do")?;
        let timestamp = chrono_millis();
        let date = Local::now().format("%Y-%m-%d%H:%M:%S9").to_string();
        url.query_pairs_mut()
            .append_pair("appId", &app_conf.app_key)
            .append_pair("encryuuid", &qr.encryuuid)
            .append_pair("date", &date)
            .append_pair("uuid", &qr.uuid)
            .append_pair("returnUrl", &app_conf.return_url)
            .append_pair("clientType", &app_conf.client_type.to_string())
            .append_pair("timeStamp", &timestamp.to_string())
            .append_pair("cb_SaveName", "0")
            .append_pair("isOauth2", &app_conf.is_oauth2.to_string())
            .append_pair("state", "")
            .append_pair("paramId", &app_conf.param_id);

        let resp = self
            .client
            .get(url)
            .header(reqwest::header::REFERER, referer)
            .send()
            .context("query qr state")?;
        let sson = extract_cookie(&resp, "SSON");
        let mut state: QrCodeState = resp.json().context("decode qr state")?;
        if state.status == 0 {
            state.sson = sson;
        }
        Ok(state)
    }

    fn fetch_session(&self, redirect_url: &str) -> Result<SessionResp> {
        let params = vec![("redirectURL".to_string(), redirect_url.to_string())];
        let url = with_client_params(Url::parse(&format!("{API_BASE}/getSessionForPC.action"))?);
        let resp = self
            .client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .header(reqwest::header::ACCEPT, "application/json;charset=UTF-8")
            .body(encode_form_urlencoded(&params))
            .send()
            .context("get session")?;
        let session: SessionResp = resp.json().context("decode session")?;
        Ok(session)
    }

    fn resolve_folder(&self, remote_dir: &str, create: bool) -> Result<String> {
        let mut current = ROOT_FOLDER_ID.to_string();
        let cleaned = remote_dir.trim();
        if cleaned == "/" || cleaned.is_empty() {
            return Ok(current);
        }
        if !cleaned.starts_with('/') {
            return Err(anyhow!("remote_dir must start with '/'"));
        }
        for segment in cleaned.trim_matches('/').split('/') {
            if segment.is_empty() {
                continue;
            }
            let folders = self.list_folders(&current)?;
            if let Some(folder) = folders.iter().find(|f| f.name == segment) {
                current = folder.id.clone();
                continue;
            }
            if !create {
                return Err(anyhow!("remote directory not found: {}", segment));
            }
            let new_id = self.create_folder(&current, segment)?;
            current = new_id;
        }
        Ok(current)
    }

    fn list_folders(&self, parent_id: &str) -> Result<Vec<Folder>> {
        let mut page = 1;
        let mut folders = Vec::new();
        loop {
            let params = vec![
                ("folderId".to_string(), parent_id.to_string()),
                ("fileType".to_string(), "0".to_string()),
                ("mediaType".to_string(), "0".to_string()),
                ("mediaAttr".to_string(), "0".to_string()),
                ("iconOption".to_string(), "0".to_string()),
                ("orderBy".to_string(), "filename".to_string()),
                ("descending".to_string(), "true".to_string()),
                ("pageNum".to_string(), page.to_string()),
                ("pageSize".to_string(), "100".to_string()),
            ];
            let url = with_query(&format!("{API_BASE}/listFiles.action"), &params)?;
            let url = with_client_params(url);
            let mut req = self.client.get(url);
            req = self.apply_signature(req, "GET", "/listFiles.action", None)?;
            let resp = req.send().context("list files")?;
            let text = resp.text().context("read list files body")?;
            let data = parse_list_files_text(&text)?;
            folders.extend(data.folders);
            match (data.count, data.page_size) {
                (Some(count), Some(page_size)) => {
                    if page * page_size >= count {
                        break;
                    }
                }
                _ => break,
            }
            page += 1;
        }
        Ok(folders)
    }

    fn create_folder(&self, parent_id: &str, name: &str) -> Result<String> {
        let params = vec![
            ("folderName".to_string(), name.to_string()),
            ("relativePath".to_string(), "".to_string()),
            ("parentFolderId".to_string(), parent_id.to_string()),
        ];
        let url = with_client_params(Url::parse(&format!("{API_BASE}/createFolder.action"))?);
        let mut req = self
            .client
            .post(url)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(encode_form_urlencoded(&params));
        req = self.apply_signature(req, "POST", "/createFolder.action", None)?;
        let resp = req.send().context("create folder")?;
        let text = resp.text().context("read create folder body")?;
        let id = parse_mkdir_text(&text)?;
        Ok(id)
    }

    fn upload_file(&self, local: &Path, parent_id: &str) -> Result<()> {
        let hashes = compute_hashes(local)?;
        let file_name = local
            .file_name()
            .and_then(|v| v.to_str())
            .ok_or_else(|| anyhow!("invalid file name"))?;
        let file_size = local.metadata()?.len();

        let init_params = vec![
            ("parentFolderId".to_string(), parent_id.to_string()),
            ("fileName".to_string(), file_name.to_string()),
            ("fileSize".to_string(), file_size.to_string()),
            ("sliceSize".to_string(), SLICE_SIZE.to_string()),
            ("fileMd5".to_string(), hashes.file_md5.clone()),
            ("sliceMd5".to_string(), hashes.slice_md5.clone()),
            (
                "extend".to_string(),
                r#"{"opScene":"1","relativepath":"","rootfolderid":""}"#.to_string(),
            ),
        ];
        let init = self.upload_get_with_retry("/person/initMultiUpload", &init_params)?;
        let init_resp: InitUploadResp = init.json().context("decode init upload")?;
        if !is_success_code(&init_resp.code) {
            return Err(anyhow!("init upload error: {}", init_resp.code));
        }
        let upload_id = init_resp.data.upload_file_id;
        if init_resp.data.file_data_exists == 1 {
            self.commit_upload(&upload_id, None, None, None)?;
            return Ok(());
        }

        let mut part_info = Vec::with_capacity(hashes.parts.len());
        for part in &hashes.parts {
            part_info.push(format!("{}-{}", part.index + 1, part.name));
        }
        let url_params = vec![
            ("partInfo".to_string(), part_info.join(",")),
            ("uploadFileId".to_string(), upload_id.clone()),
        ];
        let url_resp = self.upload_get_with_retry("/person/getMultiUploadUrls", &url_params)?;
        let urls: UploadUrlsResp = url_resp.json().context("decode upload urls")?;
        if !is_success_code(&urls.code) {
            return Err(anyhow!("get upload urls error: {}", urls.code));
        }

        for part in &hashes.parts {
            let key = format!("partNumber_{}", part.index + 1);
            let info = urls
                .upload_urls
                .get(&key)
                .ok_or_else(|| anyhow!("missing upload url for {}", key))?;
            self.upload_part(local, part, info)?;
        }

        self.commit_upload(&upload_id, None, None, None)?;
        Ok(())
    }

    fn upload_part(&self, local: &Path, part: &PartInfo, info: &UploadUrl) -> Result<()> {
        let headers = parse_request_headers(&info.request_header)?;
        let mut attempt = 0;
        info!(
            "Starting upload part {} (offset={}, len={})",
            part.index + 1,
            part.offset,
            part.len
        );
        loop {
            attempt += 1;
            let mut file = File::open(local)?;
            file.seek(SeekFrom::Start(part.offset))?;
            let body = reqwest::blocking::Body::sized(file.take(part.len), part.len);
            let resp = self
                .client
                .put(&info.request_url)
                .headers(headers.clone())
                .body(body)
                .send();

            match resp {
                Ok(resp) if resp.status().is_success() => {
                    info!("Upload part {} completed", part.index + 1);
                    return Ok(());
                }
                Ok(resp) => {
                    if attempt >= UPLOAD_PART_MAX_RETRIES {
                        return Err(anyhow!("upload part failed: {}", resp.status()));
                    }
                }
                Err(err) => {
                    if attempt >= UPLOAD_PART_MAX_RETRIES {
                        return Err(anyhow!("upload part failed: {}", err));
                    }
                }
            }

            let backoff = UPLOAD_PART_BACKOFF_BASE_MS * attempt as u64;
            sleep(Duration::from_millis(backoff));
        }
    }

    fn commit_upload(
        &self,
        upload_id: &str,
        file_md5: Option<&str>,
        slice_md5: Option<&str>,
        lazy_check: Option<&str>,
    ) -> Result<()> {
        let mut params = vec![("uploadFileId".to_string(), upload_id.to_string())];
        if let (Some(f), Some(s), Some(l)) = (file_md5, slice_md5, lazy_check) {
            params.push(("fileMd5".to_string(), f.to_string()));
            params.push(("sliceMd5".to_string(), s.to_string()));
            params.push(("lazyCheck".to_string(), l.to_string()));
        }
        let resp = self.upload_get_with_retry("/person/commitMultiUploadFile", &params)?;
        let result: CommitUploadResp = resp.json().context("decode commit")?;
        if !is_success_code(&result.code) {
            return Err(anyhow!("commit upload error: {}", result.code));
        }
        Ok(())
    }

    fn upload_get(&self, path: &str, params: &[(String, String)]) -> Result<Response> {
        let secret = self
            .config
            .session
            .as_ref()
            .ok_or_else(|| anyhow!("missing session"))?
            .secret
            .clone();
        let plain = encode_param_plain(params);
        let encrypted = aes_ecb_hex(plain.as_bytes(), &secret)?;
        let mut url = Url::parse(&format!("{UPLOAD_BASE}{path}?params={}", encrypted))?;
        url = with_client_params(url);
        let mut req = self.client.get(url);
        req = self.apply_signature(req, "GET", path, Some(&encrypted))?;
        req = req
            .header("decodefields", "familyId,parentFolderId,fileName,fileMd5,fileSize,sliceMd5,sliceSize,albumId,extend,lazyCheck,isLog")
            .header(reqwest::header::ACCEPT, "application/json;charset=UTF-8")
            .header(reqwest::header::CACHE_CONTROL, "no-cache");
        let resp = req.send().context("upload get")?;
        Ok(resp)
    }

    fn upload_get_with_retry(
        &self,
        path: &str,
        params: &[(String, String)],
    ) -> Result<Response> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let resp = self.upload_get(path, params);
            match resp {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    if attempt >= UPLOAD_REQ_MAX_RETRIES {
                        return Err(err);
                    }
                }
            }
            let backoff = UPLOAD_REQ_BACKOFF_BASE_MS * attempt as u64;
            sleep(Duration::from_millis(backoff));
        }
    }

    fn apply_signature(
        &self,
        mut req: reqwest::blocking::RequestBuilder,
        method: &str,
        path: &str,
        params: Option<&str>,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let session = match self.config.session.as_ref() {
            Some(s) if s.is_valid() => s,
            _ => return Ok(req),
        };
        let date = fmt_http_date(SystemTime::now());
        let mut data = format!(
            "SessionKey={}&Operate={}&RequestURI={}&Date={}",
            session.key, method, path, date
        );
        if let Some(p) = params {
            data.push_str("&params=");
            data.push_str(p);
        }
        let signature = hmac_sha1_hex(&data, session.secret.as_bytes());
        req = req
            .header("Date", date)
            .header("user-agent", "desktop")
            .header("SessionKey", session.key.clone())
            .header("Signature", signature)
            .header("X-Request-ID", Uuid::new_v4().to_string());
        Ok(req)
    }
}

fn encrypt_credentials(
    pub_key: &str,
    prefix: &str,
    user: &str,
    pass: &str,
) -> Result<(String, String)> {
    let pem = normalize_public_key(pub_key);
    let key = RsaPublicKey::from_public_key_pem(&pem).context("parse public key")?;
    let enc_user = key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, user.as_bytes())
        .context("encrypt username")?;
    let enc_pass = key
        .encrypt(&mut OsRng, Pkcs1v15Encrypt, pass.as_bytes())
        .context("encrypt password")?;
    Ok((
        format!("{}{}", prefix, hex::encode(enc_user)),
        format!("{}{}", prefix, hex::encode(enc_pass)),
    ))
}

fn normalize_public_key(raw: &str) -> String {
    let mut base64 = String::new();
    for line in raw.lines() {
        if line.contains("BEGIN PUBLIC KEY") || line.contains("END PUBLIC KEY") {
            continue;
        }
        for ch in line.chars() {
            let ok = ch.is_ascii_alphanumeric() || ch == '+' || ch == '/' || ch == '=';
            if ok {
                base64.push(ch);
            }
        }
    }
    if base64.is_empty() {
        base64 = raw
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '+' || *ch == '/' || *ch == '=')
            .collect();
    }
    let mut wrapped = String::new();
    let mut i = 0;
    while i < base64.len() {
        let end = usize::min(i + 64, base64.len());
        wrapped.push_str(&base64[i..end]);
        wrapped.push('\n');
        i = end;
    }
    format!(
        "-----BEGIN PUBLIC KEY-----\n{}-----END PUBLIC KEY-----",
        wrapped
    )
}

fn extract_cookie(resp: &Response, name: &str) -> Option<String> {
    resp.cookies()
        .find(|c| c.name() == name)
        .map(|c| c.value().to_string())
}

fn aes_ecb_hex(data: &[u8], secret: &str) -> Result<String> {
    if secret.len() < 16 {
        return Err(anyhow!("session secret too short"));
    }
    let key = &secret.as_bytes()[0..16];
    let cipher = Aes128::new_from_slice(key).context("init aes")?;
    let mut buf = data.to_vec();
    let pad = 16 - (buf.len() % 16);
    buf.extend(std::iter::repeat(pad as u8).take(pad));
    for chunk in buf.chunks_mut(16) {
        let block = GenericArray::from_mut_slice(chunk);
        cipher.encrypt_block(block);
    }
    Ok(hex::encode(buf))
}

fn encode_form_urlencoded(params: &[(String, String)]) -> String {
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in params {
        serializer.append_pair(k, v);
    }
    serializer.finish()
}

fn encode_param_plain(params: &[(String, String)]) -> String {
    let mut out = String::new();
    for (k, v) in params {
        if !out.is_empty() {
            out.push('&');
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

fn with_query(base: &str, params: &[(String, String)]) -> Result<Url> {
    let mut url = Url::parse(base)?;
    for (k, v) in params {
        url.query_pairs_mut().append_pair(k, v);
    }
    Ok(url)
}

fn with_client_params(mut url: Url) -> Url {
    url.query_pairs_mut()
        .append_pair("rand", &chrono_millis().to_string())
        .append_pair("clientType", "TELEPC")
        .append_pair("version", "7.1.8.0")
        .append_pair("channelId", "web_cloud.189.cn");
    url
}

fn compute_hashes(path: &Path) -> Result<FileHashes> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut global = md5::Context::new();
    let mut slice_hexes = Vec::new();
    let mut parts = Vec::new();

    let mut offset: u64 = 0;
    let mut buf = vec![0u8; SLICE_SIZE];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        global.consume(&buf[..n]);
        let slice = md5::compute(&buf[..n]);
        let slice_hex = hex::encode_upper(slice.0);
        slice_hexes.push(slice_hex);
        let name = BASE64_STD.encode(slice.0);
        parts.push(PartInfo {
            index: parts.len(),
            name,
            offset,
            len: n as u64,
        });
        offset += n as u64;
    }

    let file_md5 = format!("{:x}", global.finalize());
    let slice_md5 = if parts.len() > 1 {
        let joined = slice_hexes.join("\n");
        format!("{:x}", md5::compute(joined.as_bytes()))
    } else {
        file_md5.clone()
    };
    Ok(FileHashes {
        file_md5,
        slice_md5,
        parts,
    })
}

fn parse_request_headers(raw: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    for item in raw.split('&') {
        let mut iter = item.splitn(2, '=');
        let key = iter.next().ok_or_else(|| anyhow!("invalid header pair"))?;
        let value = iter.next().unwrap_or("");
        let name = HeaderName::from_bytes(key.as_bytes()).context("header name")?;
        let value = HeaderValue::from_str(value).context("header value")?;
        headers.insert(name, value);
    }
    Ok(headers)
}

fn hmac_sha1_hex(data: &str, key: &[u8]) -> String {
    let mut mac = <Hmac<Sha1> as Mac>::new_from_slice(key).expect("hmac");
    mac.update(data.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn parse_json_response<T: DeserializeOwned>(resp: Response, label: &str) -> Result<T> {
    let status = resp.status();
    let text = resp.text().context("read response body")?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("decode {label} json (status {status})"))?;
    if value.get("data").is_none() {
        return Err(anyhow!(
            "decode {label}: missing field `data` (status {status}) body: {}",
            snippet(&text)
        ));
    }
    serde_json::from_value(value).with_context(|| format!("decode {label} body"))
}

fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    let max = 200;
    if trimmed.len() <= max {
        trimmed.to_string()
    } else {
        format!("{}...", &trimmed[..max])
    }
}

fn parse_list_files_text(text: &str) -> Result<ListFilesResult> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('<') {
        let data: XmlListFiles = xml_from_str(text).context("decode list files xml")?;
        let folders = data
            .file_list
            .folders
            .into_iter()
            .map(|f| Folder {
                id: f.id,
                name: f.name,
            })
            .collect();
        return Ok(ListFilesResult {
            folders,
            count: data.file_list.count,
            page_size: data.file_list.file_list_size,
        });
    }
    let data: ListFilesResp = serde_json::from_str(text)
        .with_context(|| format!("decode list files json body: {}", snippet(text)))?;
    if data.res_code != 0 {
        return Err(anyhow!("list files error: {}", data.res_message));
    }
    Ok(ListFilesResult {
        folders: data.file_list.folders,
        count: Some(data.file_list.count),
        page_size: Some(data.file_list.file_list_size),
    })
}

fn parse_mkdir_text(text: &str) -> Result<String> {
    let trimmed = text.trim_start();
    if trimmed.starts_with('<') {
        let data: XmlMkdirResp = xml_from_str(text).context("decode create folder xml")?;
        let res_code = data.res_code.unwrap_or(0);
        if res_code != 0 {
            let msg = data
                .res_message
                .unwrap_or_else(|| "unknown error".to_string());
            return Err(anyhow!("create folder error: {}", msg));
        }
        return data.id.ok_or_else(|| anyhow!("create folder missing id"));
    }
    let data: MkdirResp = serde_json::from_str(text)
        .with_context(|| format!("decode create folder json body: {}", snippet(text)))?;
    if data.res_code != 0 {
        return Err(anyhow!("create folder error: {}", data.res_message));
    }
    Ok(data.id)
}

fn is_success_code(code: &str) -> bool {
    let c = code.trim();
    c == "0" || c.eq_ignore_ascii_case("success")
}

fn chrono_millis() -> i64 {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time");
    now.as_millis() as i64
}

fn de_id<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    use serde::de::Error;
    let val = serde_json::Value::deserialize(deserializer)?;
    match val {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => Ok(n.to_string()),
        _ => Err(Error::custom("invalid id")),
    }
}
