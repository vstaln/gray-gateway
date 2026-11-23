//! Private managed files referenced by typed Discord components.

use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::multipart::Part;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::channels::discord::component_protocol::FileRef;
use crate::channels::discord::durable::{Store, uuid_hex, with_conn};

pub const DEFAULT_MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;
pub const DEFAULT_MAX_TOTAL_BYTES: u64 = 100 * 1024 * 1024;
pub const DEFAULT_MAX_FILES: usize = 32;

#[derive(Debug, Clone)]
pub struct FileStore {
    store: Store,
    root: PathBuf,
    conversation_root: PathBuf,
    max_file_bytes: u64,
    max_total_bytes: u64,
    max_files: usize,
    client: reqwest::Client,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FileMetadata {
    pub id: String,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub sha256: String,
    pub expires_at: f64,
}

impl FileStore {
    pub fn new(
        store: Store,
        root: impl Into<PathBuf>,
        conversation_root: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let root = root.into();
        let conversation_root = conversation_root.into();
        std::fs::create_dir_all(&root).map_err(|_| "cannot create media directory".to_string())?;
        set_mode(&root, 0o700)?;
        std::fs::create_dir_all(&conversation_root)
            .map_err(|_| "cannot create conversation media directory".to_string())?;
        set_mode(&conversation_root, 0o700)?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(20))
            .build()
            .map_err(|_| "cannot build media client".to_string())?;
        Ok(Self {
            store,
            root,
            conversation_root,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            max_total_bytes: DEFAULT_MAX_TOTAL_BYTES,
            max_files: DEFAULT_MAX_FILES,
            client,
        })
    }

    pub fn with_limits(
        mut self,
        max_file_bytes: u64,
        max_total_bytes: u64,
        max_files: usize,
    ) -> Self {
        self.max_file_bytes = max_file_bytes;
        self.max_total_bytes = max_total_bytes;
        self.max_files = max_files;
        self
    }

    pub fn import_bytes(
        &self,
        owner_id: &str,
        name: &str,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<FileRef, String> {
        if owner_id.trim().is_empty() {
            return Err("file owner is required".to_string());
        }
        if bytes.is_empty() || bytes.len() as u64 > self.max_file_bytes {
            return Err("file exceeds the configured size limit".to_string());
        }
        let name = safe_name(name);
        if name.is_empty() {
            return Err("file name is invalid".to_string());
        }
        let media_type = normalize_media_type(media_type);
        if !allowed_media_type(&media_type) {
            return Err("file media type is not allowed".to_string());
        }
        let hash = hex_digest(bytes);
        if let Some(existing) = self.find_hash(owner_id, &hash)? {
            self.touch(&existing.id, 3600)?;
            return Ok(existing);
        }
        self.check_capacity(bytes.len() as u64)?;
        let id = uuid_hex();
        let path = self.root.join(&id);
        write_private(&path, bytes)?;
        let size = bytes.len() as u64;
        let expires_at = crate::channels::discord::durable::now_secs() + 3600.0;
        let result = with_conn(self.store.path(), |db| {
            db.execute(
                "INSERT INTO ui_component_files
                 (file_id,path,name,media_type,size,sha256,owner_id,expires_at,ref_count)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,1)",
                params![
                    id,
                    path.to_string_lossy(),
                    name,
                    media_type,
                    size as i64,
                    hash,
                    owner_id,
                    expires_at
                ],
            )
            .map_err(|_| "cannot store media metadata".to_string())
        });
        if let Err(error) = result {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        Ok(FileRef {
            id,
            name,
            media_type,
            size,
            sha256: hash,
        })
    }

    pub fn import_generated(&self, owner_id: &str, path: &Path) -> Result<FileRef, String> {
        let canonical = path
            .canonicalize()
            .map_err(|_| "generated file does not exist".to_string())?;
        let root = self
            .conversation_root
            .canonicalize()
            .map_err(|_| "conversation work directory is unavailable".to_string())?;
        if !canonical.starts_with(&root) {
            return Err("generated file is outside the conversation work directory".to_string());
        }
        let bytes =
            std::fs::read(&canonical).map_err(|_| "cannot read generated file".to_string())?;
        let name = canonical
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file");
        let media_type = infer_media_type(name);
        self.import_bytes(owner_id, name, media_type, &bytes)
    }

    pub async fn import_url(&self, owner_id: &str, url: &str) -> Result<FileRef, String> {
        self.import_url_authenticated(owner_id, url, None).await
    }

    /// Import a Discord CDN attachment with the bot credential. The token is
    /// used only as an HTTP header and is never persisted or returned.
    pub async fn import_url_authenticated(
        &self,
        owner_id: &str,
        url: &str,
        bot_token: Option<&str>,
    ) -> Result<FileRef, String> {
        let parsed = reqwest::Url::parse(url).map_err(|_| "media URL is invalid".to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("media URL must be absolute HTTP(S)".to_string());
        }
        let host = parsed.host_str().unwrap_or_default();
        if blocked_host(host)
            || resolved_host_is_blocked(host, parsed.port_or_known_default().unwrap_or(443)).await
        {
            return Err("media URL host is not allowed".to_string());
        }
        let mut request = self.client.get(parsed.clone());
        if let Some(token) = bot_token {
            request = request.header(reqwest::header::AUTHORIZATION, format!("Bot {token}"));
        }
        let response = request
            .send()
            .await
            .map_err(|_| "media URL could not be fetched".to_string())?;
        if !response.status().is_success() {
            return Err("media URL returned an unsuccessful status".to_string());
        }
        if response
            .content_length()
            .is_some_and(|size| size > self.max_file_bytes)
        {
            return Err("remote media exceeds the configured size limit".to_string());
        }
        let media_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("application/octet-stream")
            .split(';')
            .next()
            .unwrap_or("application/octet-stream")
            .to_string();
        let name = parsed
            .path_segments()
            .and_then(|mut segments| segments.next_back())
            .unwrap_or("download")
            .to_string();
        let mut bytes = Vec::new();
        let mut response = response;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "media URL read failed".to_string())?
        {
            if bytes.len() as u64 + chunk.len() as u64 > self.max_file_bytes {
                return Err("remote media exceeds the configured size limit".to_string());
            }
            bytes.extend_from_slice(&chunk);
        }
        self.import_bytes(owner_id, &name, &media_type, &bytes)
    }

    pub fn metadata(&self, owner_id: &str, file_id: &str) -> Result<FileMetadata, String> {
        with_conn(self.store.path(), |db| {
            db.query_row(
                "SELECT file_id,name,media_type,size,sha256,expires_at FROM ui_component_files WHERE file_id=?1 AND owner_id=?2",
                params![file_id, owner_id],
                |row| Ok(FileMetadata { id: row.get(0)?, name: row.get(1)?, media_type: row.get(2)?, size: row.get::<_, i64>(3)? as u64, sha256: row.get(4)?, expires_at: row.get(5)? }),
            )
            .optional()
            .map_err(|_| "media metadata query failed".to_string())?
            .ok_or_else(|| "managed file was not found".to_string())
        })
    }

    pub fn open_for_conversation(
        &self,
        owner_id: &str,
        conversation: &str,
        file_id: &str,
    ) -> Result<PathBuf, String> {
        let metadata = self.metadata(owner_id, file_id)?;
        if metadata.expires_at < crate::channels::discord::durable::now_secs() {
            return Err("managed file has expired".to_string());
        }
        let destination_dir = self
            .conversation_root
            .join(safe_name(conversation))
            .join(&metadata.id);
        std::fs::create_dir_all(&destination_dir)
            .map_err(|_| "cannot create conversation file directory".to_string())?;
        set_mode(&destination_dir, 0o700)?;
        let source = self.managed_path(file_id)?;
        let destination = destination_dir.join(&metadata.name);
        std::fs::copy(&source, &destination).map_err(|_| "cannot copy managed file".to_string())?;
        set_mode(&destination, 0o600)?;
        Ok(destination)
    }

    pub async fn multipart_part(&self, owner_id: &str, file_id: &str) -> Result<Part, String> {
        let metadata = self.metadata(owner_id, file_id)?;
        if metadata.expires_at < crate::channels::discord::durable::now_secs() {
            return Err("managed file has expired".to_string());
        }
        let path = self.managed_path(file_id)?;
        let bytes = tokio::fs::read(path)
            .await
            .map_err(|_| "cannot read managed file".to_string())?;
        Part::bytes(bytes)
            .file_name(metadata.name)
            .mime_str(&metadata.media_type)
            .map_err(|_| "managed media type is invalid".to_string())
    }

    pub fn cleanup(&self, retention_secs: u64) -> Result<usize, String> {
        let cutoff = crate::channels::discord::durable::now_secs() - retention_secs as f64;
        let expired: Vec<(String, String)> = with_conn(self.store.path(), |db| {
            let mut stmt = db
                .prepare("SELECT file_id,path FROM ui_component_files WHERE expires_at < ?1")
                .map_err(|_| "cannot list expired media".to_string())?;
            let rows = stmt
                .query_map(params![cutoff], |row| Ok((row.get(0)?, row.get(1)?)))
                .map_err(|_| "cannot list expired media".to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|_| "cannot list expired media".to_string())
        })?;
        for (_id, path) in &expired {
            let _ = std::fs::remove_file(path);
        }
        with_conn(self.store.path(), |db| {
            for (id, _) in &expired {
                db.execute(
                    "DELETE FROM ui_component_files WHERE file_id=?1",
                    params![id],
                )
                .map_err(|_| "cannot prune media metadata".to_string())?;
            }
            Ok(expired.len())
        })
    }

    fn managed_path(&self, file_id: &str) -> Result<PathBuf, String> {
        if file_id.is_empty()
            || file_id.contains('/')
            || file_id.contains('\\')
            || file_id.contains("..")
        {
            return Err("managed file ID is invalid".to_string());
        }
        Ok(self.root.join(file_id))
    }

    fn find_hash(&self, owner_id: &str, hash: &str) -> Result<Option<FileRef>, String> {
        with_conn(self.store.path(), |db| {
            db.query_row(
                "SELECT file_id,name,media_type,size,sha256 FROM ui_component_files WHERE owner_id=?1 AND sha256=?2",
                params![owner_id, hash],
                |row| Ok(FileRef { id: row.get(0)?, name: row.get(1)?, media_type: row.get(2)?, size: row.get::<_, i64>(3)? as u64, sha256: row.get(4)? }),
            )
            .optional()
            .map_err(|_| "cannot inspect media metadata".to_string())
        })
    }

    fn touch(&self, file_id: &str, ttl_secs: u64) -> Result<(), String> {
        with_conn(self.store.path(), |db| {
            db.execute("UPDATE ui_component_files SET expires_at=?1,ref_count=ref_count+1 WHERE file_id=?2", params![crate::channels::discord::durable::now_secs() + ttl_secs as f64, file_id]).map_err(|_| "cannot retain media".to_string())?;
            Ok(())
        })
    }

    fn check_capacity(&self, incoming: u64) -> Result<(), String> {
        let (count, total) = with_conn(self.store.path(), |db| {
            let count: i64 = db
                .query_row("SELECT count(*) FROM ui_component_files", [], |row| {
                    row.get(0)
                })
                .map_err(|_| "cannot inspect media count".to_string())?;
            let total: i64 = db
                .query_row(
                    "SELECT coalesce(sum(size),0) FROM ui_component_files",
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| "cannot inspect media size".to_string())?;
            Ok((count, total))
        })?;
        if count as usize >= self.max_files || total as u64 + incoming > self.max_total_bytes {
            return Err("managed media capacity exceeded".to_string());
        }
        Ok(())
    }
}

pub struct MediaResolver<'a> {
    pub files: &'a FileStore,
    pub owner_id: String,
}

impl crate::channels::discord::component_compile::ComponentIdAllocator for MediaResolver<'_> {
    fn allocate(
        &mut self,
        logical_id: &str,
        kind: &str,
    ) -> Result<String, crate::channels::discord::component_compile::CompileError> {
        Err(
            crate::channels::discord::component_compile::CompileError::new(
                "state_unavailable",
                logical_id,
                format!("media resolver cannot allocate {kind} state"),
            ),
        )
    }

    fn resolve_file(
        &self,
        file_id: &str,
    ) -> Result<FileRef, crate::channels::discord::component_compile::CompileError> {
        self.files
            .metadata(&self.owner_id, file_id)
            .map(|metadata| FileRef {
                id: metadata.id,
                name: metadata.name,
                media_type: metadata.media_type,
                size: metadata.size,
                sha256: metadata.sha256,
            })
            .map_err(|message| {
                crate::channels::discord::component_compile::CompileError::new(
                    "file_not_found",
                    file_id,
                    message,
                )
            })
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "cannot create managed file".to_string())?;
    file.write_all(bytes)
        .map_err(|_| "cannot write managed file".to_string())?;
    set_mode(path, 0o600)
}

fn set_mode(path: &Path, mode: u32) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .map_err(|_| "cannot set private file permissions".to_string())?;
    }
    #[cfg(not(unix))]
    let _ = (path, mode);
    Ok(())
}

fn safe_name(name: &str) -> String {
    let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let mut output = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
            output.push(ch);
        } else {
            output.push('_');
        }
    }
    output.chars().take(120).collect()
}

fn normalize_media_type(value: &str) -> String {
    value
        .split(';')
        .next()
        .unwrap_or(value)
        .trim()
        .to_ascii_lowercase()
}

fn allowed_media_type(value: &str) -> bool {
    matches!(
        value,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "text/plain"
            | "text/markdown"
            | "application/pdf"
            | "application/json"
            | "application/octet-stream"
            | "text/csv"
            | "application/zip"
            | "video/mp4"
            | "video/webm"
            | "video/quicktime"
            | "audio/mpeg"
            | "audio/ogg"
            | "audio/wav"
    )
}

pub fn infer_media_type(name: &str) -> &'static str {
    match name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" => "text/plain",
        "md" => "text/markdown",
        "pdf" => "application/pdf",
        "json" => "application/json",
        "csv" => "text/csv",
        "zip" => "application/zip",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "ogg" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn blocked_host(host: &str) -> bool {
    let host = host
        .trim_matches(|c| c == '[' || c == ']')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return true;
    }
    host.parse::<IpAddr>().is_ok_and(is_blocked_ip)
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified()
        }
        IpAddr::V6(ip) => {
            ip.is_loopback()
                || ip.is_unspecified()
                || (ip.segments()[0] & 0xfe00) == 0xfc00
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|mapped| is_blocked_ip(IpAddr::V4(mapped)))
        }
    }
}

async fn resolved_host_is_blocked(host: &str, port: u16) -> bool {
    if blocked_host(host) {
        return true;
    }
    match tokio::net::lookup_host((host, port)).await {
        Ok(addresses) => addresses
            .into_iter()
            .any(|address| is_blocked_ip(address.ip())),
        Err(_) => false,
    }
}
