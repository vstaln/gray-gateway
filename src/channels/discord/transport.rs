//! Discord transport: REST/gateway calls and shared constants.
//! Discord REST client with safe-mention defaults and bounded sends.
use reqwest::multipart::Form;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::channels::discord::component_compile::{CompiledMessage, CompiledModal};
use crate::channels::discord::component_media::FileStore;
use crate::channels::discord::component_protocol::FileRef;

/// Production Discord API base. Tests inject a loopback stub via `Rest::new`;
/// production call sites always use `Rest::production` (never read a base URL
/// from user config — same rule as the Python's "no api_base" comment).
pub const API_BASE: &str = "https://discord.com/api/v10";
/// Discord message content limit, measured in UTF-16 units (see text.rs).
pub const MAX_CONTENT_LEN: usize = 2000;
/// Legacy embed cap retained for pre-V2 queue compatibility. New messages do
/// not use embeds; they use the V2 component budget instead.
pub const MAX_EMBEDS: usize = 10;
/// Discord does not inherit SEND_MESSAGES into threads, so the bridge needs
/// this bit explicitly. The invite's full permission set lives in
/// `discord_check::INVITE_PERMISSIONS`.
pub const PERM_SEND_MESSAGES_IN_THREADS: u64 = 1 << 38;
/// Gateway intent bits for the Task 9 connect (discord.py Intents values).
pub const INTENT_GUILDS: u32 = 1 << 0;
pub const INTENT_GUILD_MESSAGES: u32 = 1 << 9;
pub const INTENT_DIRECT_MESSAGES: u32 = 1 << 12;
pub const INTENT_MESSAGE_CONTENT: u32 = 1 << 15;
/// Application flags read from GET /applications/@me (discord.py
/// ApplicationFlags): full and limited message-content variants.
pub const APP_FLAG_MESSAGE_CONTENT: u64 = 1 << 18;
pub const APP_FLAG_MESSAGE_CONTENT_LIMITED: u64 = 1 << 19;
/// Guild permission bits required in the home channel (doctor check).
pub const PERM_ADMINISTRATOR: u64 = 1 << 3;
pub const PERM_VIEW_CHANNEL: u64 = 1 << 10;
pub const PERM_SEND_MESSAGES: u64 = 1 << 11;
pub const PERM_READ_MESSAGE_HISTORY: u64 = 1 << 16;
pub const HOME_PERMS: u64 = PERM_VIEW_CHANNEL | PERM_SEND_MESSAGES | PERM_READ_MESSAGE_HISTORY;
pub const THREAD_PERMS: u64 =
    PERM_VIEW_CHANNEL | PERM_READ_MESSAGE_HISTORY | PERM_SEND_MESSAGES_IN_THREADS;

/// Bot user id (`GET /users/@me` → `id`).
pub type UserId = String;
/// Message id (`POST .../messages` → `id`).
pub type MessageId = String;

/// Channel metadata from `GET /channels/{id}`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: u64,
    pub kind: u64,
    pub guild_id: Option<String>,
    pub parent_id: Option<String>,
    pub permission_overwrites: Vec<Value>,
}

impl Channel {
    /// Guild channels always carry `guild_id`; DMs never do. This mirrors the
    /// Python's `isinstance(channel, discord.abc.GuildChannel)` without
    /// hardcoding the channel-type table.
    pub fn is_guild(&self) -> bool {
        self.guild_id.is_some()
    }

    /// Discord's public/private/announcement thread channel types.
    pub fn is_thread(&self) -> bool {
        matches!(self.kind, 10..=12)
    }
}

#[derive(Debug, Clone)]
pub enum TransportError {
    Auth(String),
    Forbidden(String),
    RateLimited(Option<f64>),
    Http(u16, String),
    Net(String),
    Invalid(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auth(_) => f.write_str("Discord authentication failed; check the bot token"),
            Self::Forbidden(_) => {
                f.write_str("Discord refused the request; check channel permissions")
            }
            Self::RateLimited(_) => f.write_str("Discord rate limit hit; backing off"),
            Self::Http(status @ (400 | 413), detail) => {
                write!(f, "Discord rejected the request (HTTP {status}): {detail}")
            }
            Self::Http(status, _) => write!(f, "Discord HTTP {status}"),
            Self::Net(_) => f.write_str("Discord request failed; check connectivity"),
            Self::Invalid(msg) => f.write_str(msg),
        }
    }
}
impl std::error::Error for TransportError {}

impl TransportError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::RateLimited(_) | Self::Net(_) => true,
            Self::Http(s, _) => *s == 429 || *s >= 500,
            _ => false,
        }
    }
}

/// A Discord request that has not answered in this long is given up as a
/// network error (retried by its caller), never waited on forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Uploads carry files and get longer.
const UPLOAD_TIMEOUT: Duration = Duration::from_secs(120);

/// Thin REST client. `base` is a constructor arg so tests can inject the
/// loopback stub; production call sites always pass `API_BASE` (never read a
/// base URL from user config — same rule as the Python's "no api_base").
#[derive(Debug, Clone)]
pub struct Rest {
    base: String,
    client: reqwest::Client,
    rate_limits: Arc<Mutex<RateState>>,
    /// Longest a send waits on a rate limit before it returns instead (see
    /// `with_budget`). `None` waits as long as Discord asks.
    budget: Option<Duration>,
}

#[derive(Debug, Clone)]
struct RateBucket {
    limit: u32,
    remaining: u32,
    reset_at: Instant,
}

#[derive(Debug)]
struct RateState {
    global: RateBucket,
    routes: HashMap<String, RateBucket>,
    route_buckets: HashMap<String, String>,
}

impl RateState {
    fn new() -> Self {
        Self {
            global: RateBucket {
                limit: 50,
                remaining: 50,
                reset_at: Instant::now() + Duration::from_secs(1),
            },
            routes: HashMap::new(),
            route_buckets: HashMap::new(),
        }
    }
}

/// Python `len(text)` counts code points, so bounds use `chars().count()` —
/// byte length would wrongly reject emoji-heavy messages under the cap.
fn check_bounds(text: &str, max: usize, msg: &str) -> Result<(), TransportError> {
    if text.trim().is_empty() || text.chars().count() > max {
        return Err(TransportError::Invalid(msg.to_string()));
    }
    Ok(())
}

impl Rest {
    pub fn new(base: &str, token: &str) -> Self {
        use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
        let mut headers = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&format!("Bot {token}")) {
            headers.insert(AUTHORIZATION, v);
        }
        if let Ok(v) = HeaderValue::from_str("application/json") {
            headers.insert(CONTENT_TYPE, v);
        }
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(
                "gray-discord/0.1 (+https://github.com/vstaln/gray-discord-plugin)",
            ),
        );
        // Every request is bounded. Without this a pooled connection that
        // died under us (a laptop's wifi, a NAT timeout) leaves a request
        // waiting forever, and the turn's messages freeze with it.
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tcp_keepalive(Duration::from_secs(30))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client builds");
        Self {
            base: base.trim_end_matches('/').to_string(),
            client,
            rate_limits: Arc::new(Mutex::new(RateState::new())),
            budget: None,
        }
    }

    /// A client whose message sends wait at most `budget` on a rate limit.
    /// Over it they return `RateLimited` with Discord's wait, nothing sent,
    /// for the caller to retry off the path that is waiting. Shares buckets
    /// with `self`.
    pub fn with_budget(&self, budget: Duration) -> Self {
        let mut rest = self.clone();
        rest.budget = Some(budget);
        rest
    }

    pub fn production(token: &str) -> Self {
        Self::new(API_BASE, token)
    }

    async fn classify(
        &self,
        status: reqwest::StatusCode,
        resp: reqwest::Response,
    ) -> Result<Value, TransportError> {
        let code = status.as_u16();
        if status.is_success() {
            // 204 No Content (typing/callbacks) has no body.
            let text = resp.text().await.unwrap_or_default();
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text)
                .map_err(|_| TransportError::Http(code, "bad response".into()));
        }
        if code == 401 || code == 403 {
            // Drain the body so the connection can be reused, then discard it:
            // error bodies may echo request context.
            let _ = resp.bytes().await;
            if code == 401 {
                return Err(TransportError::Auth("unauthorized".into()));
            }
            return Err(TransportError::Forbidden("forbidden".into()));
        }
        if code == 429 {
            let header_retry = resp
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<f64>().ok());
            let body_retry = resp
                .json::<Value>()
                .await
                .ok()
                .and_then(|v| v.get("retry_after").and_then(Value::as_f64));
            return Err(TransportError::RateLimited(header_retry.or(body_retry)));
        }
        if code == 400 || code == 413 {
            // Discord's validation body names the offending field paths
            // (it describes our own payload, never credentials); keep it so
            // the author can fix the document instead of guessing.
            let body = resp.json::<Value>().await.unwrap_or(Value::Null);
            return Err(TransportError::Http(
                code,
                discord_error_detail(code, &body),
            ));
        }
        let _ = resp.bytes().await;
        Err(TransportError::Http(code, "request failed".into()))
    }

    async fn wait_for_rate_limit(&self, route: &str) {
        let _ = self.wait_for_rate_limit_bounded(route, None, true).await;
    }

    /// `wait_for_rate_limit` within this client's budget (see `with_budget`).
    async fn wait_within_budget(&self, route: &str) -> Result<(), TransportError> {
        self.wait_for_rate_limit_bounded(route, self.budget, true)
            .await
            .map_err(|wait| TransportError::RateLimited(Some(wait.as_secs_f64())))
    }

    /// Waits until the route and global buckets allow a request. Over
    /// `budget` it returns the wait without sleeping and takes no slot.
    async fn wait_for_rate_limit_bounded(
        &self,
        route: &str,
        budget: Option<Duration>,
        include_global: bool,
    ) -> Result<(), Duration> {
        let deadline = budget.map(|max| Instant::now() + max);
        loop {
            let wait = {
                let mut state = match self.rate_limits.lock() {
                    Ok(state) => state,
                    Err(_) => return Ok(()),
                };
                let now = Instant::now();
                if now >= state.global.reset_at {
                    state.global.remaining = state.global.limit;
                    state.global.reset_at = now + Duration::from_secs(1);
                }
                let global_wait = if include_global && state.global.remaining == 0 {
                    state.global.reset_at.saturating_duration_since(now)
                } else {
                    Duration::ZERO
                };
                let bucket_key = state
                    .route_buckets
                    .get(route)
                    .cloned()
                    .unwrap_or_else(|| route.to_string());
                let route_wait = if let Some(bucket) = state.routes.get_mut(&bucket_key) {
                    if now >= bucket.reset_at {
                        bucket.remaining = bucket.limit;
                        bucket.reset_at = now + Duration::from_secs(1);
                    }
                    if bucket.remaining == 0 {
                        bucket.reset_at.saturating_duration_since(now)
                    } else {
                        Duration::ZERO
                    }
                } else {
                    Duration::ZERO
                };
                let wait = global_wait.max(route_wait);
                if wait.is_zero() {
                    // Consume a global and route slot only when the request is
                    // actually allowed to leave the client.
                    if include_global {
                        state.global.remaining = state.global.remaining.saturating_sub(1);
                    }
                    if let Some(bucket) = state.routes.get_mut(&bucket_key) {
                        bucket.remaining = bucket.remaining.saturating_sub(1);
                    }
                }
                wait
            };
            if wait.is_zero() {
                return Ok(());
            }
            if deadline
                .is_some_and(|deadline| wait >= deadline.saturating_duration_since(Instant::now()))
            {
                return Err(wait);
            }
            tokio::time::sleep(wait).await;
        }
    }

    fn observe_rate_limit(&self, route: &str, headers: &reqwest::header::HeaderMap) {
        let Ok(mut state) = self.rate_limits.lock() else {
            return;
        };
        let now = Instant::now();
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
        let bucket_name = header("x-ratelimit-bucket").unwrap_or(route);
        let limit: Option<u32> = header("x-ratelimit-limit").and_then(|v| v.parse().ok());
        let remaining: Option<u32> = header("x-ratelimit-remaining").and_then(|v| v.parse().ok());
        let reset_after: Option<f64> =
            header("x-ratelimit-reset-after").and_then(|v| v.parse().ok());
        if let (Some(limit), Some(remaining), Some(reset_after)) = (limit, remaining, reset_after) {
            let bucket = RateBucket {
                limit,
                remaining,
                reset_at: now + Duration::from_secs_f64(reset_after.max(0.0)),
            };
            // Keep the route lookup stable even when Discord gives us a
            // shared bucket hash; subsequent calls to this path can wait.
            state
                .route_buckets
                .insert(route.to_string(), bucket_name.to_string());
            state.routes.insert(bucket_name.to_string(), bucket);
        }
        if let Some(retry_after) = header("retry-after").and_then(|v| v.parse::<f64>().ok()) {
            state
                .route_buckets
                .insert(route.to_string(), route.to_string());
            state.routes.insert(
                route.to_string(),
                RateBucket {
                    limit: 1,
                    remaining: 0,
                    reset_at: now + Duration::from_secs_f64(retry_after.max(0.0)),
                },
            );
        }
        let global_limit = headers
            .get("x-ratelimit-global")
            .map(|v| v == "true")
            .unwrap_or(false);
        if global_limit
            && let (Some(limit), Some(remaining), Some(reset_after)) = (
                header("x-ratelimit-limit").and_then(|v| v.parse::<u32>().ok()),
                header("x-ratelimit-remaining").and_then(|v| v.parse::<u32>().ok()),
                header("x-ratelimit-reset-after").and_then(|v| v.parse::<f64>().ok()),
            )
        {
            state.global = RateBucket {
                limit,
                remaining,
                reset_at: now + Duration::from_secs_f64(reset_after.max(0.0)),
            };
        }
    }

    async fn get(&self, path: &str) -> Result<Value, TransportError> {
        let route = format!("GET {path}");
        self.wait_for_rate_limit(&route).await;
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .get(url)
            .send()
            .await
            .map_err(|e| TransportError::Net(trim_net_error(e)))?;
        self.observe_rate_limit(&route, resp.headers());
        let status = resp.status();
        self.classify(status, resp).await
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value, TransportError> {
        let route = format!("POST {path}");
        self.wait_within_budget(&route).await?;
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .post(url)
            .json(body)
            .send()
            .await
            .map_err(|e| TransportError::Net(trim_net_error(e)))?;
        self.observe_rate_limit(&route, resp.headers());
        let status = resp.status();
        self.classify(status, resp).await
    }

    async fn patch(&self, path: &str, body: &Value) -> Result<Value, TransportError> {
        let route = format!("PATCH {path}");
        self.wait_within_budget(&route).await?;
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .patch(url)
            .json(body)
            .send()
            .await
            .map_err(|e| TransportError::Net(trim_net_error(e)))?;
        self.observe_rate_limit(&route, resp.headers());
        let status = resp.status();
        self.classify(status, resp).await
    }

    /// Overwrite one of our own messages (the activity bubble). Mentions
    /// stay off. `Ok(false)` means the message is gone (404) and the caller
    /// should stop editing it; every other failure is transient.
    pub async fn edit_message(&self, channel: u64, message: &str, text: &str) -> bool {
        let body = json!({"content": text, "allowed_mentions": {"parse": []}});
        match self
            .patch(&format!("/channels/{channel}/messages/{message}"), &body)
            .await
        {
            Ok(_) => true,
            Err(TransportError::Http(404, _)) => false,
            Err(_) => true,
        }
    }

    /// GET /users/@me → bot user id. Proves the token works.
    pub async fn login(&self) -> Result<UserId, TransportError> {
        let v = self.get("/users/@me").await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// GET /applications/@me, raw: setup's token check reads the owner,
    /// intents and server count from it (`discord_check::parse_application`).
    pub async fn application_info(&self) -> Result<Value, TransportError> {
        self.get("/applications/@me").await
    }

    /// GET /applications/@me → (app id, message-content-intent flag).
    pub async fn application(&self) -> Result<(String, bool), TransportError> {
        let v = self.get("/applications/@me").await?;
        let id = v
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let flags = v.get("flags").and_then(Value::as_u64).unwrap_or(0);
        let intent =
            flags & APP_FLAG_MESSAGE_CONTENT != 0 || flags & APP_FLAG_MESSAGE_CONTENT_LIMITED != 0;
        Ok((id, intent))
    }

    /// GET /channels/{id} → channel metadata.
    pub async fn fetch_channel(&self, id: u64) -> Result<Channel, TransportError> {
        let v = self.get(&format!("/channels/{id}")).await?;
        let kind = v.get("type").and_then(Value::as_u64).unwrap_or(0);
        let guild_id = v
            .get("guild_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let parent_id = v
            .get("parent_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let permission_overwrites = v
            .get("permission_overwrites")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(Channel {
            id,
            kind,
            guild_id,
            parent_id,
            permission_overwrites,
        })
    }

    /// GET /channels/{id}/messages?after=&limit= — the backfill cursor
    /// replay. Discord serves newest-first; callers get oldest-first so a
    /// cursor only ever moves forward.
    pub async fn messages_after(
        &self,
        channel: u64,
        after: u64,
        limit: u32,
    ) -> Result<Vec<Value>, TransportError> {
        let limit = limit.clamp(1, 100);
        let value = self
            .get(&format!(
                "/channels/{channel}/messages?after={after}&limit={limit}"
            ))
            .await?;
        let mut list = value.as_array().cloned().unwrap_or_default();
        list.reverse();
        Ok(list)
    }

    /// GET channel + guild + member, then apply Discord's ordered channel
    /// overwrites (and the parent chain used by threads/categories).
    pub async fn effective_channel_permissions(
        &self,
        channel: &Channel,
        user_id: &str,
    ) -> Result<u64, TransportError> {
        let Some(guild_id) = channel.guild_id.as_deref() else {
            return Ok(u64::MAX);
        };
        let guild = self.get(&format!("/guilds/{guild_id}")).await?;
        let member = self
            .get(&format!("/guilds/{guild_id}/members/{user_id}"))
            .await?;
        if guild.get("owner_id").and_then(Value::as_str) == Some(user_id) {
            return Ok(u64::MAX);
        }
        let worn: Vec<&str> = member
            .get("roles")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut perms = role_permissions(&guild, guild_id, &worn);
        if perms & PERM_ADMINISTRATOR != 0 {
            return Ok(u64::MAX);
        }

        // Discord applies the parent/category overwrites before the child
        // channel. A short bound avoids a malformed cyclic API response.
        let mut chain = Vec::new();
        let mut next_parent = channel.parent_id.clone();
        let mut seen = std::collections::HashSet::new();
        while let Some(parent_id) = next_parent {
            if !seen.insert(parent_id.clone()) || chain.len() >= 4 {
                break;
            }
            let Ok(parent_id_num) = parent_id.parse::<u64>() else {
                break;
            };
            let parent = self.fetch_channel(parent_id_num).await?;
            next_parent = parent.parent_id.clone();
            chain.push(parent);
        }
        chain.push(channel.clone());
        for target in chain {
            apply_overwrites(
                &mut perms,
                &target.permission_overwrites,
                guild_id,
                user_id,
                &worn,
            );
        }
        Ok(perms)
    }

    /// GET /guilds/{guild}/members/{user} → guild-wide permission bits.
    /// The Python resolves channel overwrites via `permissions_for`; doctor
    /// checks the guild-level grant, which is the actionable diagnostic.
    /// A member's effective guild permissions, computed the way every
    /// client does (the member object itself carries no `permissions` field —
    /// reading one there is what made doctor fail with a bare HTTP 200):
    /// the guild owner holds everything implicitly, otherwise the
    /// `@everyone` role (its id is the guild id) ORs with each role the
    /// member wears.
    pub async fn guild_member_permissions(
        &self,
        guild_id: &str,
        user_id: &str,
    ) -> Result<u64, TransportError> {
        let guild = self.get(&format!("/guilds/{guild_id}")).await?;
        let member = self
            .get(&format!("/guilds/{guild_id}/members/{user_id}"))
            .await?;
        if guild.get("owner_id").and_then(Value::as_str) == Some(user_id) {
            return Ok(u64::MAX);
        }
        let roles = guild
            .get("roles")
            .and_then(Value::as_array)
            .ok_or_else(|| TransportError::Invalid("cannot read guild roles".into()))?;
        let worn: Vec<&str> = member
            .get("roles")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let mut perms: u64 = 0;
        for role in roles {
            let id = role.get("id").and_then(Value::as_str).unwrap_or("");
            let granted = role
                .get("permissions")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            if id == guild_id || worn.contains(&id) {
                perms |= granted;
            }
        }
        Ok(perms)
    }

    /// Send text as sequential chunks. Returns the first message id.
    pub async fn send(
        &self,
        channel: u64,
        text: &str,
        nonce: Option<&str>,
    ) -> Result<MessageId, TransportError> {
        check_bounds(text, 20000, "Reply must contain 1–20000 characters")?;
        let chunks = crate::channels::discord::text::split_message(text, MAX_CONTENT_LEN)
            .map_err(TransportError::Invalid)?;
        let mut first: Option<String> = None;
        for chunk in &chunks {
            let mut body = json!({"content": chunk, "allowed_mentions": {"parse": []}});
            if let Some(n) = nonce {
                body["nonce"] = json!(n);
            }
            // Deliberately no message_reference: replies never ping.
            let v = self
                .post(&format!("/channels/{channel}/messages"), &body)
                .await?;
            if first.is_none() {
                first = v.get("id").and_then(Value::as_str).map(str::to_string);
            }
        }
        first.ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Send a compiled Gray document. This is the only path used by agent and
    /// plugin document authors; the component tree cannot contain raw Discord
    /// IDs because the compiler allocated them.
    pub async fn send_compiled(
        &self,
        channel: u64,
        compiled: &CompiledMessage,
        files: &[FileRef],
        file_store: &FileStore,
        owner_id: &str,
        nonce: Option<&str>,
    ) -> Result<MessageId, TransportError> {
        if compiled.components.is_empty() {
            return Err(TransportError::Invalid("compiled message is empty".into()));
        }
        let mut body = json!({
            "flags": Self::compiled_flags(compiled),
            "components": compiled.components,
            "allowed_mentions": {"parse": []},
        });
        if let Some(nonce) = nonce {
            body["nonce"] = json!(nonce);
        }
        let value = if files.is_empty() {
            self.post(&format!("/channels/{channel}/messages"), &body)
                .await?
        } else {
            self.send_multipart(
                &format!("/channels/{channel}/messages"),
                &body,
                files,
                file_store,
                owner_id,
                "POST",
            )
            .await?
        };
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Replay a serialized compiled document with its original flags.
    pub async fn send_stored_document(
        &self,
        channel: u64,
        document: &Value,
        nonce: Option<&str>,
    ) -> Result<MessageId, TransportError> {
        let components = document
            .get("components")
            .and_then(Value::as_array)
            .ok_or_else(|| TransportError::Invalid("stored document has no components".into()))?;
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        let flags = document
            .get("flags")
            .and_then(Value::as_u64)
            .unwrap_or(crate::channels::discord::render::IS_COMPONENTS_V2);
        let mut body = json!({
            "flags": flags,
            "components": components,
            "allowed_mentions": {"parse": []},
        });
        if let Some(nonce) = nonce {
            body["nonce"] = json!(nonce);
        }
        self.post(&format!("/channels/{channel}/messages"), &body)
            .await?
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Replay a serialized document into an original interaction response,
    /// retaining its visibility flag across a restart.
    pub async fn edit_original_stored_document(
        &self,
        app_id: &str,
        token: &str,
        document: &Value,
    ) -> Result<MessageId, TransportError> {
        let components = document
            .get("components")
            .and_then(Value::as_array)
            .ok_or_else(|| TransportError::Invalid("stored document has no components".into()))?;
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        let flags = document
            .get("flags")
            .and_then(Value::as_u64)
            .unwrap_or(crate::channels::discord::render::IS_COMPONENTS_V2);
        self.patch(
            &format!("/webhooks/{app_id}/{token}/messages/@original"),
            &json!({
                "flags": flags,
                "components": components,
                "allowed_mentions": {"parse": []},
                "content": Value::Null,
                "embeds": [],
                "sticker_ids": [],
            }),
        )
        .await?
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Edit a previously sent compiled document.
    pub async fn edit_compiled(
        &self,
        channel: u64,
        message: &str,
        compiled: &CompiledMessage,
        files: &[FileRef],
        file_store: &FileStore,
        owner_id: &str,
    ) -> bool {
        if compiled.components.is_empty() {
            return false;
        }
        let body = json!({
            "flags": Self::compiled_flags(compiled),
            "components": compiled.components,
            "allowed_mentions": {"parse": []},
            "content": Value::Null,
            "embeds": [],
            "sticker_ids": [],
        });
        let result = if files.is_empty() {
            self.patch(&format!("/channels/{channel}/messages/{message}"), &body)
                .await
        } else {
            self.send_multipart(
                &format!("/channels/{channel}/messages/{message}"),
                &body,
                files,
                file_store,
                owner_id,
                "PATCH",
            )
            .await
        };
        match result {
            Ok(_) => true,
            Err(TransportError::Http(404, _)) => false,
            Err(_) => true,
        }
    }

    /// Open a modal from an interaction callback (response type 9).
    pub async fn open_modal(
        &self,
        interaction_id: &str,
        token: &str,
        modal: &CompiledModal,
    ) -> Result<(), TransportError> {
        self.interaction_callback(
            interaction_id,
            token,
            &json!({
                "type": 9,
                "data": {
                    "custom_id": modal.custom_id,
                    "title": modal.title,
                    "components": modal.components
                }
            }),
        )
        .await
    }

    /// Alias for callers that use Discord's "modal response" terminology.
    pub async fn modal_response(
        &self,
        interaction_id: &str,
        token: &str,
        modal: &CompiledModal,
    ) -> Result<(), TransportError> {
        self.open_modal(interaction_id, token, modal).await
    }

    /// Send a type-4 callback response with a compiled message.
    pub async fn callback(
        &self,
        interaction_id: &str,
        token: &str,
        compiled: &CompiledMessage,
        public: bool,
    ) -> Result<(), TransportError> {
        let flags = Self::compiled_flags(compiled)
            | if public {
                0
            } else {
                crate::channels::discord::component_compile::EPHEMERAL
            };
        self.interaction_callback(
            interaction_id,
            token,
            &json!({
                "type": 4,
                "data": {
                    "flags": flags,
                    "components": compiled.components,
                    "allowed_mentions": {"parse": []}
                }
            }),
        )
        .await
    }

    /// Defer a component update (response type 6).
    pub async fn defer_update(
        &self,
        interaction_id: &str,
        token: &str,
    ) -> Result<(), TransportError> {
        self.interaction_callback(interaction_id, token, &json!({"type": 6}))
            .await
    }

    /// Update the message associated with a component (response type 7).
    pub async fn update_message(
        &self,
        interaction_id: &str,
        token: &str,
        compiled: &CompiledMessage,
    ) -> Result<(), TransportError> {
        self.interaction_callback(
            interaction_id,
            token,
            &json!({
                "type": 7,
                "data": {
                    "flags": Self::compiled_flags(compiled),
                    "components": compiled.components,
                    "allowed_mentions": {"parse": []}
                }
            }),
        )
        .await
    }

    /// Respond to autocomplete with at most 25 bounded choices (type 8).
    pub async fn autocomplete_response(
        &self,
        interaction_id: &str,
        token: &str,
        choices: &[(String, String)],
    ) -> Result<(), TransportError> {
        if choices.len() > 25 {
            return Err(TransportError::Invalid(
                "autocomplete has more than 25 choices".into(),
            ));
        }
        let choices: Vec<Value> = choices
            .iter()
            .map(|(name, value)| json!({"name": name, "value": value}))
            .collect();
        self.interaction_callback(
            interaction_id,
            token,
            &json!({"type": 8, "data": {"choices": choices}}),
        )
        .await
    }

    /// Edit the original deferred response using a compiled document.
    pub async fn edit_original_compiled(
        &self,
        app_id: &str,
        token: &str,
        compiled: &CompiledMessage,
        public: bool,
    ) -> Result<MessageId, TransportError> {
        let flags = Self::compiled_flags(compiled)
            | if public {
                0
            } else {
                crate::channels::discord::component_compile::EPHEMERAL
            };
        let body = json!({
            "flags": flags,
            "components": compiled.components,
            "allowed_mentions": {"parse": []},
            "content": Value::Null,
            "embeds": [],
            "sticker_ids": [],
        });
        let value = self
            .patch(
                &format!("/webhooks/{app_id}/{token}/messages/@original"),
                &body,
            )
            .await?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    fn compiled_flags(compiled: &CompiledMessage) -> u64 {
        compiled.flags
            | if compiled.visibility
                == crate::channels::discord::component_protocol::Visibility::Ephemeral
            {
                crate::channels::discord::component_compile::EPHEMERAL
            } else {
                0
            }
    }

    /// Multipart request shared by compiled message sends and edits.
    async fn send_multipart(
        &self,
        path: &str,
        payload: &Value,
        files: &[FileRef],
        file_store: &FileStore,
        owner_id: &str,
        method: &str,
    ) -> Result<Value, TransportError> {
        let mut payload = payload.clone();
        payload["attachments"] = Value::Array(
            files
                .iter()
                .enumerate()
                .map(|(index, file)| json!({"id": index, "filename": file.name}))
                .collect(),
        );
        let mut form = Form::new().text(
            "payload_json",
            serde_json::to_string(&payload)
                .map_err(|_| TransportError::Invalid("compiled payload is invalid".into()))?,
        );
        for (index, file) in files.iter().enumerate() {
            let part = file_store
                .multipart_part(owner_id, &file.id)
                .await
                .map_err(TransportError::Invalid)?;
            let field = format!("files[{index}]");
            form = form.part(field, part);
        }
        let route = format!("{method} {path}");
        self.wait_within_budget(&route).await?;
        let url = format!("{}{}", self.base, path);
        let request = match method {
            "PATCH" => self.client.patch(url).multipart(form),
            _ => self.client.post(url).multipart(form),
        }
        .timeout(UPLOAD_TIMEOUT);
        let response = request
            .send()
            .await
            .map_err(|error| TransportError::Net(trim_net_error(error)))?;
        self.observe_rate_limit(&route, response.headers());
        let status = response.status();
        self.classify(status, response).await
    }

    /// Send a Components V2 message. The component tree is validated before
    /// any network request; V2 messages never include `content` or `embeds`.
    pub async fn send_v2(
        &self,
        channel: u64,
        components: &[Value],
        nonce: Option<&str>,
    ) -> Result<MessageId, TransportError> {
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        let mut body = json!({
            "flags": crate::channels::discord::render::IS_COMPONENTS_V2,
            "components": components,
            "allowed_mentions": {"parse": []},
        });
        if let Some(n) = nonce {
            body["nonce"] = json!(n);
        }
        let v = self
            .post(&format!("/channels/{channel}/messages"), &body)
            .await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Send a V2 message that uploads local files, referenced from the
    /// component tree as `attachment://<filename>`.
    pub async fn send_v2_uploads(
        &self,
        channel: u64,
        components: &[Value],
        uploads: &[crate::channels::discord::media_tags::Upload],
        nonce: Option<&str>,
    ) -> Result<MessageId, TransportError> {
        let mut body = json!({
            "flags": crate::channels::discord::render::IS_COMPONENTS_V2,
            "components": components,
            "allowed_mentions": {"parse": []},
        });
        if let Some(nonce) = nonce {
            body["nonce"] = json!(nonce);
        }
        let value = self
            .multipart_v2(
                reqwest::Method::POST,
                &format!("/channels/{channel}/messages"),
                body,
                uploads,
            )
            .await?;
        value
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Edit a V2 message and attach local files to it in the same request
    /// (a finished turn card gaining the answer's images). The attachment
    /// list is replaced by `uploads`.
    pub async fn edit_v2_uploads(
        &self,
        channel: u64,
        message: &str,
        components: &[Value],
        uploads: &[crate::channels::discord::media_tags::Upload],
    ) -> Result<(), TransportError> {
        let body = json!({
            "flags": crate::channels::discord::render::IS_COMPONENTS_V2,
            "components": components,
            "allowed_mentions": {"parse": []},
        });
        self.multipart_v2(
            reqwest::Method::PATCH,
            &format!("/channels/{channel}/messages/{message}"),
            body,
            uploads,
        )
        .await
        .map(|_| ())
    }

    /// One multipart V2 request: `payload_json` plus `files[n]`, with the
    /// `attachments` list naming each upload by index.
    async fn multipart_v2(
        &self,
        method: reqwest::Method,
        path: &str,
        mut body: Value,
        uploads: &[crate::channels::discord::media_tags::Upload],
    ) -> Result<Value, TransportError> {
        let components = body
            .get("components")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        crate::channels::discord::render::validate_components(&components)
            .map_err(TransportError::Invalid)?;
        body["attachments"] = uploads
            .iter()
            .enumerate()
            .map(|(index, upload)| json!({"id": index, "filename": upload.name}))
            .collect();
        let mut form = Form::new().text(
            "payload_json",
            serde_json::to_string(&body)
                .map_err(|_| TransportError::Invalid("upload payload is invalid".into()))?,
        );
        for (index, upload) in uploads.iter().enumerate() {
            let part = reqwest::multipart::Part::bytes(upload.bytes.clone())
                .file_name(upload.name.clone())
                .mime_str(&upload.media_type)
                .map_err(|_| TransportError::Invalid("upload media type is invalid".into()))?;
            form = form.part(format!("files[{index}]"), part);
        }
        let route = format!("{method} {path}");
        self.wait_within_budget(&route).await?;
        let response = self
            .client
            .request(method, format!("{}{}", self.base, path))
            .multipart(form)
            .timeout(UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|error| TransportError::Net(trim_net_error(error)))?;
        self.observe_rate_limit(&route, response.headers());
        let status = response.status();
        self.classify(status, response).await
    }

    /// Send model text as one or more bounded V2 Text Display messages.
    pub async fn send_text_v2(
        &self,
        channel: u64,
        text: &str,
        nonce: Option<&str>,
    ) -> Result<MessageId, TransportError> {
        check_bounds(
            text,
            200000,
            "Completed answer must contain 1–200000 characters",
        )?;
        let chunks = crate::channels::discord::text::split_message(text, MAX_CONTENT_LEN)
            .map_err(TransportError::Invalid)?;
        let mut first = None;
        for (index, chunk) in chunks.iter().enumerate() {
            let components = crate::channels::discord::render::text_message(chunk)
                .map_err(TransportError::Invalid)?;
            let id = self
                .send_v2(channel, &components, if index == 0 { nonce } else { None })
                .await?;
            if first.is_none() {
                first = Some(id);
            }
        }
        first.ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Edit a live V2 message. Legacy fields are explicitly cleared because
    /// Discord preserves message flags across edits.
    pub async fn edit_message_v2(&self, channel: u64, message: &str, components: &[Value]) -> bool {
        match self.edit_v2(channel, message, components).await {
            Ok(()) => true,
            Err(TransportError::Http(404, _)) | Err(TransportError::Invalid(_)) => false,
            Err(_) => true,
        }
    }

    /// [`Self::edit_message_v2`] with the failure kept: a streamed reply must
    /// tell a deleted message (stop editing it) from a transient error (try
    /// again next frame), and must never count a refused edit as delivered.
    ///
    /// Every message edited here was created as V2, so the body is the
    /// create body's shape: the flag plus components. The legacy fields
    /// (`content`, `embeds`, `sticker_ids`) only matter when converting a
    /// legacy message, and `sticker_ids` is not an Edit Message field.
    pub async fn edit_v2(
        &self,
        channel: u64,
        message: &str,
        components: &[Value],
    ) -> Result<(), TransportError> {
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        let body = json!({
            "flags": crate::channels::discord::render::IS_COMPONENTS_V2,
            "components": components,
            "allowed_mentions": {"parse": []},
        });
        self.patch(&format!("/channels/{channel}/messages/{message}"), &body)
            .await
            .map(|_| ())
    }

    /// Delete one of our own messages: a streamed preview the final answer
    /// could not land in, so the durable resend does not post it twice.
    pub async fn delete_message(&self, channel: u64, message: &str) -> Result<(), TransportError> {
        let path = format!("/channels/{channel}/messages/{message}");
        let route = format!("DELETE {path}");
        self.wait_for_rate_limit(&route).await;
        let resp = self
            .client
            .delete(format!("{}{}", self.base, path))
            .send()
            .await
            .map_err(|e| TransportError::Net(trim_net_error(e)))?;
        self.observe_rate_limit(&route, resp.headers());
        let status = resp.status();
        self.classify(status, resp).await.map(|_| ())
    }

    /// Respond to an application command/component with a V2 message.
    pub async fn interaction_v2(
        &self,
        id: &str,
        token: &str,
        components: &[Value],
        public: bool,
    ) -> Result<(), TransportError> {
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        let flags = crate::channels::discord::render::IS_COMPONENTS_V2
            | if public {
                0
            } else {
                crate::channels::discord::render::EPHEMERAL
            };
        let data = json!({
            "flags": flags,
            "components": components,
            "allowed_mentions": {"parse": []},
        });
        self.interaction_callback(id, token, &json!({"type": 4, "data": data}))
            .await
    }

    /// Acknowledge a potentially slow interaction without legacy content.
    /// Visibility is fixed here; the later V2 edit inherits it.
    pub async fn defer_v2(
        &self,
        id: &str,
        token: &str,
        public: bool,
    ) -> Result<(), TransportError> {
        let data = if public {
            json!({})
        } else {
            json!({"flags": crate::channels::discord::render::EPHEMERAL})
        };
        self.interaction_callback(id, token, &json!({"type": 5, "data": data}))
            .await
    }

    /// Edit the original response of a deferred interaction. This is the
    /// required V2 path; posting a followup would create a second answer.
    pub async fn edit_original_v2(
        &self,
        app_id: &str,
        token: &str,
        components: &[Value],
        public: bool,
    ) -> Result<MessageId, TransportError> {
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        let flags = crate::channels::discord::render::IS_COMPONENTS_V2
            | if public {
                0
            } else {
                crate::channels::discord::render::EPHEMERAL
            };
        let body = json!({
            "flags": flags,
            "components": components,
            "allowed_mentions": {"parse": []},
            "content": Value::Null,
            "embeds": [],
            "sticker_ids": [],
        });
        let v = self
            .patch(
                &format!("/webhooks/{app_id}/{token}/messages/@original"),
                &body,
            )
            .await?;
        v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| TransportError::Http(200, "bad response".into()))
    }

    /// Post a V2 webhook followup. Used for later chunks of a deferred answer
    /// after the first chunk edited `@original`.
    pub async fn followup_v2(
        &self,
        app_id: &str,
        token: &str,
        text: &str,
    ) -> Result<Vec<String>, TransportError> {
        check_bounds(
            text,
            200000,
            "Completed answer must contain 1–200000 characters",
        )?;
        let chunks = crate::channels::discord::text::split_message(text, MAX_CONTENT_LEN)
            .map_err(TransportError::Invalid)?;
        let mut ids = Vec::new();
        for chunk in chunks {
            let components = crate::channels::discord::render::text_message(&chunk)
                .map_err(TransportError::Invalid)?;
            let v = self
                .post(
                    &format!("/webhooks/{app_id}/{token}"),
                    &json!({
                        "flags": crate::channels::discord::render::IS_COMPONENTS_V2,
                        "components": components,
                        "allowed_mentions": {"parse": []},
                    }),
                )
                .await?;
            ids.push(
                v.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
        Ok(ids)
    }

    /// Update a component's original message in-place (callback type 7).
    pub async fn interaction_update_v2(
        &self,
        id: &str,
        token: &str,
        components: &[Value],
    ) -> Result<(), TransportError> {
        crate::channels::discord::render::validate_components(components)
            .map_err(TransportError::Invalid)?;
        self.interaction_callback(
            id,
            token,
            &json!({
                "type": 7,
                "data": {
                    "flags": crate::channels::discord::render::IS_COMPONENTS_V2,
                    "components": components,
                    "allowed_mentions": {"parse": []},
                }
            }),
        )
        .await
    }

    /// Typing indicator. Best-effort: never fails a turn.
    /// Open (or re-open) the DM channel with a user: POST
    /// /users/@me/channels. Used by setup to make the owner's DM the home
    /// channel without asking them to hunt an ID.
    pub async fn create_dm(&self, user_id: u64) -> Result<u64, TransportError> {
        self.post(
            "/users/@me/channels",
            &serde_json::json!({ "recipient_id": user_id.to_string() }),
        )
        .await
        .and_then(|v| {
            v.get("id")
                .and_then(Value::as_str)
                .and_then(|s| s.parse::<u64>().ok())
                .ok_or_else(|| {
                    TransportError::Invalid("Discord did not return a DM channel".into())
                })
        })
    }

    /// One message carrying an embed (the pairing reply). Discord caps an
    /// embed at this size; titles and fields that outgrow it are refused
    /// here rather than 400'd by the API.
    pub async fn send_embed(
        &self,
        channel: u64,
        content: &str,
        embed: &Value,
    ) -> Result<MessageId, TransportError> {
        check_bounds(content, 20000, "Reply must contain 1-20000 characters")?;
        let compact = serde_json::to_string(embed).unwrap_or_default();
        check_bounds(&compact, 6000, "Embed is too large")?;
        let mut body = json!({
            "content": content,
            "allowed_mentions": {"parse": []},
            "embeds": [embed],
        });
        let _ = body["content"].take();
        let v = self
            .post(&format!("/channels/{channel}/messages"), &body)
            .await?;
        Ok(v.get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_default())
    }

    pub async fn typing(&self, channel: u64) {
        let _ = self
            .post(&format!("/channels/{channel}/typing"), &json!({}))
            .await;
    }

    /// Add/remove reactions. Best-effort: never fail a turn.
    pub async fn add_reaction(&self, channel: u64, message: &str, emoji: &str) {
        let encoded = percent_encode(emoji);
        let url = format!(
            "{}/channels/{channel}/messages/{message}/reactions/{encoded}/@me",
            self.base
        );
        let route = format!("PUT /channels/{channel}/messages/{message}/reactions/{encoded}/@me");
        self.wait_for_rate_limit(&route).await;
        if let Ok(resp) = self.client.put(url).send().await {
            self.observe_rate_limit(&route, resp.headers());
        }
    }

    pub async fn remove_reaction(&self, channel: u64, message: &str, emoji: &str) {
        let encoded = percent_encode(emoji);
        let url = format!(
            "{}/channels/{channel}/messages/{message}/reactions/{encoded}/@me",
            self.base
        );
        let route =
            format!("DELETE /channels/{channel}/messages/{message}/reactions/{encoded}/@me");
        self.wait_for_rate_limit(&route).await;
        if let Ok(resp) = self.client.delete(url).send().await {
            self.observe_rate_limit(&route, resp.headers());
        }
    }

    /// Bulk-overwrite global slash commands for `app_id`.
    pub async fn register_commands(
        &self,
        app_id: &str,
        cmds: &Value,
    ) -> Result<(), TransportError> {
        let path = format!("/applications/{app_id}/commands");
        let route = format!("PUT {path}");
        self.wait_for_rate_limit(&route).await;
        let url = format!("{}{}", self.base, path);
        let resp = self
            .client
            .put(url)
            .json(cmds)
            .send()
            .await
            .map_err(|e| TransportError::Net(trim_net_error(e)))?;
        self.observe_rate_limit(&route, resp.headers());
        let status = resp.status();
        self.classify(status, resp).await.map(|_| ())
    }

    /// Slash interaction ack. The gateway passes the deferred payload
    /// (`{"type": 5}`: DEFERRED_CHANNEL_MESSAGE_WITH_SOURCE).
    pub async fn interaction_callback(
        &self,
        id: &str,
        token: &str,
        data: &Value,
    ) -> Result<(), TransportError> {
        let path = format!("/interactions/{id}/{token}/callback");
        let route = format!("POST {path}");
        if self
            .wait_for_rate_limit_bounded(&route, Some(Duration::from_millis(750)), false)
            .await
            .is_err()
        {
            return Err(TransportError::RateLimited(Some(0.75)));
        }
        let url = format!("{}{}", self.base, path);
        let resp = tokio::time::timeout(
            Duration::from_millis(1500),
            self.client.post(url).json(data).send(),
        )
        .await
        .map_err(|_| TransportError::Net("interaction timeout".to_string()))?
        .map_err(|e| TransportError::Net(trim_net_error(e)))?;
        self.observe_rate_limit(&route, resp.headers());
        let status = resp.status();
        self.classify(status, resp).await.map(|_| ())
    }

    /// Slash followup chunks (webhook path). Returns message ids.
    pub async fn followup(
        &self,
        app_id: &str,
        token: &str,
        text: &str,
    ) -> Result<Vec<String>, TransportError> {
        check_bounds(
            text,
            200000,
            "Completed answer must contain 1–200000 characters",
        )?;
        let chunks = crate::channels::discord::text::split_message(text, MAX_CONTENT_LEN)
            .map_err(TransportError::Invalid)?;
        let mut ids = Vec::new();
        for chunk in &chunks {
            let body = json!({"content": chunk, "allowed_mentions": {"parse": []}});
            let v = self
                .post(&format!("/webhooks/{app_id}/{token}"), &body)
                .await?;
            ids.push(
                v.get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            );
        }
        Ok(ids)
    }

    /// The embed sibling of `followup`. A deferred slash-command answer is a
    /// webhook post, not a channel post, so it cannot reuse `send_embed`;
    /// the bounds are the same, and an oversized embed is refused here rather
    /// than 400'd by the API.
    pub async fn followup_embed(
        &self,
        app_id: &str,
        token: &str,
        embed: &Value,
    ) -> Result<String, TransportError> {
        let compact = serde_json::to_string(embed).unwrap_or_default();
        check_bounds(&compact, 6000, "Reply is too large")?;
        let body = json!({
            "allowed_mentions": {"parse": []},
            "embeds": [embed],
        });
        let v = self
            .post(&format!("/webhooks/{app_id}/{token}"), &body)
            .await?;
        Ok(v.get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string())
    }

    /// Login → channel → send. REST only.
    pub async fn rest_send(
        &self,
        channel_id: &str,
        text: &str,
    ) -> Result<MessageId, TransportError> {
        check_bounds(text, 20000, "Content must contain 1–20000 characters")?;
        self.login().await?;
        let id: u64 = channel_id
            .parse()
            .map_err(|_| TransportError::Invalid("channel_id must be a Discord ID".into()))?;
        self.fetch_channel(id).await?;
        self.send_text_v2(id, text, None).await
    }
}

fn role_permissions(guild: &Value, guild_id: &str, worn: &[&str]) -> u64 {
    let roles = guild
        .get("roles")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut perms = 0;
    for role in roles {
        let id = role.get("id").and_then(Value::as_str).unwrap_or("");
        let granted = role
            .get("permissions")
            .and_then(|v| {
                v.as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| v.as_u64())
            })
            .unwrap_or(0);
        if id == guild_id || worn.contains(&id) {
            perms |= granted;
        }
    }
    perms
}

fn apply_overwrites(
    perms: &mut u64,
    overwrites: &[Value],
    guild_id: &str,
    user_id: &str,
    worn: &[&str],
) {
    let value = |v: &Value, name: &str| {
        v.get(name)
            .and_then(|value| {
                value
                    .as_str()
                    .and_then(|s| s.parse::<u64>().ok())
                    .or_else(|| value.as_u64())
            })
            .unwrap_or(0)
    };
    // The order mirrors Discord's documented permission-overwrite algorithm.
    for overwrite in overwrites {
        if overwrite.get("id").and_then(Value::as_str) == Some(guild_id) {
            *perms &= !value(overwrite, "deny");
        }
    }
    for overwrite in overwrites {
        if overwrite.get("id").and_then(Value::as_str) == Some(guild_id) {
            *perms |= value(overwrite, "allow");
        }
    }
    for overwrite in overwrites {
        let id = overwrite.get("id").and_then(Value::as_str).unwrap_or("");
        if worn.contains(&id) {
            *perms &= !value(overwrite, "deny");
        }
    }
    for overwrite in overwrites {
        let id = overwrite.get("id").and_then(Value::as_str).unwrap_or("");
        if worn.contains(&id) {
            *perms |= value(overwrite, "allow");
        }
    }
    for overwrite in overwrites {
        if overwrite.get("id").and_then(Value::as_str) == Some(user_id) {
            *perms &= !value(overwrite, "deny");
        }
    }
    for overwrite in overwrites {
        if overwrite.get("id").and_then(Value::as_str) == Some(user_id) {
            *perms |= value(overwrite, "allow");
        }
    }
}

/// Flatten Discord's nested `errors` object into `path: message` lines.
pub fn discord_error_detail(code: u16, body: &Value) -> String {
    if code == 413 {
        return "the upload is larger than this server allows".to_string();
    }
    fn walk(value: &Value, path: &str, out: &mut Vec<String>) {
        let Some(map) = value.as_object() else {
            return;
        };
        if let Some(errors) = map.get("_errors").and_then(Value::as_array) {
            for error in errors {
                if let Some(message) = error.get("message").and_then(Value::as_str) {
                    out.push(format!("{path}: {message}"));
                }
            }
        }
        for (key, child) in map {
            if key == "_errors" {
                continue;
            }
            let next = if key.chars().all(|c| c.is_ascii_digit()) {
                format!("{path}[{key}]")
            } else if path.is_empty() {
                key.clone()
            } else {
                format!("{path}.{key}")
            };
            walk(child, &next, out);
        }
    }
    let mut lines = Vec::new();
    if let Some(errors) = body.get("errors") {
        walk(errors, "", &mut lines);
    }
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("request rejected");
    let mut detail = if lines.is_empty() {
        message.to_string()
    } else {
        format!("{message}: {}", lines.join("; "))
    };
    if detail.chars().count() > 900 {
        detail = detail.chars().take(900).collect::<String>() + "…";
    }
    detail
}

/// Trim reqwest error display to a category (may contain URLs, never tokens —
/// the token only travels in a header, but URLs still leak channel ids).
fn trim_net_error(e: reqwest::Error) -> String {
    if e.is_timeout() {
        return "timeout".to_string();
    }
    if e.is_connect() {
        return "connect failed".to_string();
    }
    "request failed".to_string()
}

fn percent_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod permission_tests {
    use super::*;

    #[tokio::test]
    async fn a_shared_discord_bucket_blocks_the_route_that_references_it() {
        let rest = Rest::new("http://127.0.0.1:1", "TESTTOKEN");
        {
            let mut state = rest.rate_limits.lock().unwrap();
            state
                .route_buckets
                .insert("POST /messages".to_string(), "shared-bucket".to_string());
            state.routes.insert(
                "shared-bucket".to_string(),
                RateBucket {
                    limit: 1,
                    remaining: 0,
                    reset_at: Instant::now() + Duration::from_millis(40),
                },
            );
        }
        let started = Instant::now();
        rest.wait_for_rate_limit("POST /messages").await;
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "route did not wait on the shared bucket"
        );
    }

    #[test]
    fn ordered_overwrites_deny_then_allow_and_member_wins() {
        let mut perms = PERM_VIEW_CHANNEL | PERM_SEND_MESSAGES;
        let overwrites = vec![
            serde_json::json!({"id": "guild", "deny": PERM_SEND_MESSAGES.to_string(), "allow": "0"}),
            serde_json::json!({"id": "role", "deny": "0", "allow": PERM_READ_MESSAGE_HISTORY.to_string()}),
            serde_json::json!({"id": "user", "deny": PERM_VIEW_CHANNEL.to_string(), "allow": PERM_SEND_MESSAGES.to_string()}),
        ];
        apply_overwrites(&mut perms, &overwrites, "guild", "user", &["role"]);
        assert_eq!(perms & PERM_VIEW_CHANNEL, 0);
        assert_ne!(perms & PERM_SEND_MESSAGES, 0);
        assert_ne!(perms & PERM_READ_MESSAGE_HISTORY, 0);
    }
}
