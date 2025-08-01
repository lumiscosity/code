use crate::ErrorKind;
use crate::util::fetch::REQWEST_CLIENT;
use base64::Engine;
use base64::prelude::{BASE64_STANDARD, BASE64_URL_SAFE_NO_PAD};
use chrono::{DateTime, Duration, TimeZone, Utc};
use dashmap::DashMap;
use futures::TryStreamExt;
use heck::ToTitleCase;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::pkcs8::{DecodePrivateKey, EncodePrivateKey, LineEnding};
use rand::Rng;
use rand::rngs::OsRng;
use reqwest::header::HeaderMap;
use reqwest::{Response, StatusCode};
use serde::de::DeserializeOwned;
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::json;
use sha2::Digest;
use std::borrow::Cow;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::Future;
use std::hash::{BuildHasherDefault, DefaultHasher};
use std::io;
use std::ops::Deref;
use std::sync::Arc;
use std::time::Instant;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::sync::Mutex;
use tokio::task;
use url::Url;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub enum MinecraftAuthStep {
    GetDeviceToken,
    SisuAuthenticate,
    GetOAuthToken,
    RefreshOAuthToken,
    SisuAuthorize,
    XstsAuthorize,
    MinecraftToken,
    MinecraftEntitlements,
    MinecraftProfile,
}

#[derive(thiserror::Error, Debug)]
pub enum MinecraftAuthenticationError {
    #[error("Error reading public key during generation")]
    ReadingPublicKey,
    #[error("Failed to serialize private key to PEM: {0}")]
    PEMSerialize(#[from] p256::pkcs8::Error),
    #[error("Failed to serialize body to JSON during step {step:?}: {source}")]
    SerializeBody {
        step: MinecraftAuthStep,
        #[source]
        source: serde_json::Error,
    },
    #[error(
        "Failed to deserialize response to JSON during step {step:?}: {source}. Status Code: {status_code} Body: {raw}"
    )]
    DeserializeResponse {
        step: MinecraftAuthStep,
        raw: String,
        #[source]
        source: serde_json::Error,
        status_code: StatusCode,
    },
    #[error("Request failed during step {step:?}: {source}")]
    Request {
        step: MinecraftAuthStep,
        #[source]
        source: reqwest::Error,
    },
    #[error("Error reading XBOX Session ID header")]
    NoSessionId,
    #[error("Error reading user hash")]
    NoUserHash,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct MinecraftLoginFlow {
    pub verifier: String,
    pub challenge: String,
    pub session_id: String,
    pub auth_request_uri: String,
}

#[tracing::instrument]
pub async fn login_begin(
    exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
) -> crate::Result<MinecraftLoginFlow> {
    let (pair, current_date) =
        DeviceTokenPair::refresh_and_get_device_token(Utc::now(), exec).await?;

    let verifier = generate_oauth_challenge();
    let result = sha2::Sha256::digest(&verifier);
    let challenge = BASE64_URL_SAFE_NO_PAD.encode(result);

    match sisu_authenticate(
        &pair.token.token,
        &challenge,
        &pair.key,
        current_date,
    )
    .await
    {
        Ok((session_id, redirect_uri)) => {
            return Ok(MinecraftLoginFlow {
                verifier,
                challenge,
                session_id,
                auth_request_uri: redirect_uri.value.msa_oauth_redirect,
            });
        }
        Err(err) => return Err(crate::ErrorKind::from(err).into()),
    }
}

#[tracing::instrument]
pub async fn login_finish(
    code: &str,
    flow: MinecraftLoginFlow,
    exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
) -> crate::Result<Credentials> {
    let (pair, _) =
        DeviceTokenPair::refresh_and_get_device_token(Utc::now(), exec).await?;

    let oauth_token = oauth_token(code, &flow.verifier).await?;
    let sisu_authorize = sisu_authorize(
        Some(&flow.session_id),
        &oauth_token.value.access_token,
        &pair.token.token,
        &pair.key,
        oauth_token.date,
    )
    .await?;

    let xbox_token = xsts_authorize(
        sisu_authorize.value,
        &pair.token.token,
        &pair.key,
        sisu_authorize.date,
    )
    .await?;
    let minecraft_token = minecraft_token(xbox_token.value).await?;

    minecraft_entitlements(&minecraft_token.access_token).await?;

    let mut credentials = Credentials {
        offline_profile: MinecraftProfile::default(),
        access_token: minecraft_token.access_token,
        refresh_token: oauth_token.value.refresh_token,
        expires: oauth_token.date
            + Duration::seconds(oauth_token.value.expires_in as i64),
        active: true,
    };

    // During login, we need to fetch the online profile at least once to get the
    // player UUID and name to use for the offline profile, in order for that offline
    // profile to make sense. It's also important to modify the returned credentials
    // object, as otherwise continued usage of it will skip the profile cache due to
    // the dummy UUID
    let online_profile = credentials
        .online_profile()
        .await
        .ok_or(io::Error::other("Failed to fetch player profile"))?;
    credentials.offline_profile = MinecraftProfile {
        id: online_profile.id,
        name: online_profile.name.clone(),
        ..credentials.offline_profile
    };

    credentials.upsert(exec).await?;

    Ok(credentials)
}

#[derive(Deserialize, Debug)]
pub struct Credentials {
    /// The offline profile of the user these credentials are for.
    ///
    /// Such a profile can only be relied upon to have a proper player UUID, which is
    /// never changed. A potentially stale username may be available, but no other data
    /// such as skins or capes is available.
    #[serde(rename = "profile")]
    pub offline_profile: MinecraftProfile,
    pub access_token: String,
    pub refresh_token: String,
    pub expires: DateTime<Utc>,
    pub active: bool,
}

/// An entry in the player profile cache, keyed by player UUID.
pub(super) enum ProfileCacheEntry {
    /// A cached profile that is valid, even though it may be stale.
    Hit(Arc<MinecraftProfile>),
    /// A negative profile fetch result due to an authentication error,
    /// from which we're recovering by holding off from repeatedly
    /// attempting to fetch the profile until the token is refreshed
    /// or some time has passed.
    AuthErrorBackoff {
        likely_expired_token: String,
        last_attempt: Instant,
    },
}

/// A thread-safe cache of online profiles, used to avoid fetching the
/// same profile multiple times as long as they don't get too stale.
///
/// The cache has to be static because credential objects are short lived
/// and disposable, and in the future several threads may be interested in
/// profile data.
pub(super) static PROFILE_CACHE: Mutex<
    HashMap<Uuid, ProfileCacheEntry, BuildHasherDefault<DefaultHasher>>,
> = Mutex::const_new(HashMap::with_hasher(BuildHasherDefault::new()));

impl Credentials {
    /// Refreshes the authentication tokens for this user if they are expired, or
    /// very close to expiration.
    async fn refresh(
        &mut self,
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
    ) -> crate::Result<()> {
        // Use a margin of 5 minutes to give e.g. Minecraft and potentially
        // other operations that depend on a fresh token 5 minutes to complete
        // from now, and deal with some classes of clock skew
        if self.expires > Utc::now() + Duration::minutes(5) {
            return Ok(());
        }

        let oauth_token = oauth_refresh(&self.refresh_token).await?;
        let (pair, current_date) =
            DeviceTokenPair::refresh_and_get_device_token(
                oauth_token.date,
                exec,
            )
            .await?;

        let sisu_authorize = sisu_authorize(
            None,
            &oauth_token.value.access_token,
            &pair.token.token,
            &pair.key,
            current_date,
        )
        .await?;

        let xbox_token = xsts_authorize(
            sisu_authorize.value,
            &pair.token.token,
            &pair.key,
            sisu_authorize.date,
        )
        .await?;

        let minecraft_token = minecraft_token(xbox_token.value).await?;

        self.access_token = minecraft_token.access_token;
        self.refresh_token = oauth_token.value.refresh_token;
        self.expires = oauth_token.date
            + Duration::seconds(oauth_token.value.expires_in as i64);

        self.upsert(exec).await?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub async fn online_profile(&self) -> Option<Arc<MinecraftProfile>> {
        let mut profile_cache = PROFILE_CACHE.lock().await;

        loop {
            match profile_cache.entry(self.offline_profile.id) {
                Entry::Occupied(entry) => {
                    match entry.get() {
                        ProfileCacheEntry::Hit(profile)
                            if profile.is_fresh() =>
                        {
                            return Some(Arc::clone(profile));
                        }
                        ProfileCacheEntry::Hit(_) => {
                            // The profile is stale, so remove it and try again
                            entry.remove();
                            continue;
                        }
                        // Auth errors must be handled with a backoff strategy because it
                        // has been experimentally found that Mojang quickly rate limits
                        // the profile data endpoint on repeated attempts with bad auth
                        ProfileCacheEntry::AuthErrorBackoff {
                            likely_expired_token,
                            last_attempt,
                        } if &self.access_token != likely_expired_token
                            || Instant::now()
                                .saturating_duration_since(*last_attempt)
                                > std::time::Duration::from_secs(60) =>
                        {
                            entry.remove();
                            continue;
                        }
                        ProfileCacheEntry::AuthErrorBackoff { .. } => {
                            return None;
                        }
                    }
                }
                Entry::Vacant(entry) => {
                    match minecraft_profile(&self.access_token).await {
                        Ok(profile) => {
                            let profile = Arc::new(profile);
                            let cache_entry =
                                ProfileCacheEntry::Hit(Arc::clone(&profile));

                            // When fetching a profile for the first time, the player UUID may
                            // be unknown (i.e., set to a dummy value), so make sure we don't
                            // cache it in the wrong place
                            if entry.key() != &profile.id {
                                profile_cache.insert(profile.id, cache_entry);
                            } else {
                                entry.insert(cache_entry);
                            }

                            return Some(profile);
                        }
                        Err(
                            err @ MinecraftAuthenticationError::DeserializeResponse {
                                status_code: StatusCode::UNAUTHORIZED,
                                ..
                            },
                        ) => {
                            tracing::warn!(
                                "Failed to fetch online profile for UUID {} likely due to stale credentials, backing off: {err}",
                                self.offline_profile.id
                            );

                            // We have to assume the player UUID key we have is correct here, which
                            // should always be the case assuming a non-adversarial server. In any
                            // case, any cache poisoning is inconsequential due to the entry expiration
                            // and the fact that we use at most one single dummy UUID
                            entry.insert(ProfileCacheEntry::AuthErrorBackoff {
                                likely_expired_token: self.access_token.clone(),
                                last_attempt: Instant::now(),
                            });

                            return None;
                        }
                        Err(err) => {
                            tracing::warn!(
                                "Failed to fetch online profile for UUID {}: {err}",
                                self.offline_profile.id
                            );

                            return None;
                        }
                    }
                }
            }
        }
    }

    /// Attempts to fetch the online profile for this user if possible, and if that fails
    /// falls back to the known offline profile data.
    ///
    /// See also the [`online_profile`](Self::online_profile) method.
    pub async fn maybe_online_profile(
        &self,
    ) -> MaybeOnlineMinecraftProfile<'_> {
        let online_profile = self.online_profile().await;
        online_profile.map_or_else(
            || MaybeOnlineMinecraftProfile::Offline(&self.offline_profile),
            MaybeOnlineMinecraftProfile::Online,
        )
    }

    /// Like [`get_active`](Self::get_active), but enforces credentials to be
    /// successfully refreshed unless the network is unreachable or times out.
    #[tracing::instrument]
    pub async fn get_default_credential(
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
    ) -> crate::Result<Option<Credentials>> {
        let credentials = Self::get_active(exec).await?;

        if let Some(mut creds) = credentials {
            let res = creds.refresh(exec).await;

            match res {
                Ok(_) => Ok(Some(creds)),
                Err(err) => {
                    if let ErrorKind::MinecraftAuthenticationError(
                        MinecraftAuthenticationError::Request {
                            ref source,
                            ..
                        },
                    ) = *err.raw
                    {
                        if source.is_connect() || source.is_timeout() {
                            return Ok(Some(creds));
                        }
                    }

                    Err(err)
                }
            }
        } else {
            Ok(None)
        }
    }

    /// Fetches the currently selected credentials from the database, attempting
    /// to refresh them if they are expired.
    pub async fn get_active(
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
    ) -> crate::Result<Option<Self>> {
        let res = sqlx::query!(
            "
            SELECT
                uuid, active, username, access_token, refresh_token, expires
            FROM minecraft_users
            WHERE active = TRUE
            "
        )
        .fetch_optional(exec)
        .await?;

        Ok(match res {
            Some(x) => {
                let mut credentials = Self {
                    offline_profile: MinecraftProfile {
                        id: Uuid::parse_str(&x.uuid).unwrap_or_default(),
                        name: x.username,
                        ..MinecraftProfile::default()
                    },
                    access_token: x.access_token,
                    refresh_token: x.refresh_token,
                    expires: Utc
                        .timestamp_opt(x.expires, 0)
                        .single()
                        .unwrap_or_else(Utc::now),
                    active: x.active == 1,
                };
                credentials.refresh(exec).await.ok();
                Some(credentials)
            }
            None => None,
        })
    }

    pub async fn get_all(
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
    ) -> crate::Result<DashMap<Uuid, Self>> {
        let res = sqlx::query!(
            "
            SELECT
                uuid, active, username, access_token, refresh_token, expires
            FROM minecraft_users
            "
        )
        .fetch(exec)
        .try_fold(DashMap::new(), |acc, x| {
            let uuid = Uuid::parse_str(&x.uuid).unwrap_or_default();
            let mut credentials = Self {
                offline_profile: MinecraftProfile {
                    id: uuid,
                    name: x.username,
                    ..MinecraftProfile::default()
                },
                access_token: x.access_token,
                refresh_token: x.refresh_token,
                expires: Utc
                    .timestamp_opt(x.expires, 0)
                    .single()
                    .unwrap_or_else(Utc::now),
                active: x.active == 1,
            };

            async move {
                credentials.refresh(exec).await.ok();
                acc.insert(uuid, credentials);

                Ok(acc)
            }
        })
        .await?;

        Ok(res)
    }

    pub async fn upsert(
        &self,
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
    ) -> crate::Result<()> {
        let profile = self.maybe_online_profile().await;
        let expires = self.expires.timestamp();
        let uuid = profile.id.as_hyphenated().to_string();

        if self.active {
            sqlx::query!(
                "
                UPDATE minecraft_users
                SET active = FALSE
                ",
            )
            .execute(exec)
            .await?;
        }

        sqlx::query!(
            "
            INSERT INTO minecraft_users (uuid, active, username, access_token, refresh_token, expires)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (uuid) DO UPDATE SET
                active = $2,
                username = $3,
                access_token = $4,
                refresh_token = $5,
                expires = $6
            ",
            uuid,
            self.active,
            profile.name,
            self.access_token,
            self.refresh_token,
            expires,
        )
            .execute(exec)
            .await?;

        Ok(())
    }

    pub async fn remove(
        uuid: Uuid,
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    ) -> crate::Result<()> {
        let uuid = uuid.as_hyphenated().to_string();

        sqlx::query!(
            "
            DELETE FROM minecraft_users WHERE uuid = $1
            ",
            uuid,
        )
        .execute(exec)
        .await?;

        Ok(())
    }
}

impl Serialize for Credentials {
    fn serialize<S: Serializer>(
        &self,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        // Opportunistically hydrate the profile with its online data if possible for frontend
        // consumption, transparently handling all the possible Tokio runtime states the current
        // thread may be in the most efficient way
        let profile = match Handle::try_current().ok() {
            Some(runtime)
                if runtime.runtime_flavor() == RuntimeFlavor::CurrentThread =>
            {
                runtime.block_on(self.maybe_online_profile())
            }
            Some(runtime) => task::block_in_place(|| {
                runtime.block_on(self.maybe_online_profile())
            }),
            None => tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_or_else(
                    |_| {
                        MaybeOnlineMinecraftProfile::Offline(
                            &self.offline_profile,
                        )
                    },
                    |runtime| runtime.block_on(self.maybe_online_profile()),
                ),
        };

        let mut ser = serializer.serialize_struct("Credentials", 5)?;
        ser.serialize_field("profile", &*profile)?;
        ser.serialize_field("access_token", &self.access_token)?;
        ser.serialize_field("refresh_token", &self.refresh_token)?;
        ser.serialize_field("expires", &self.expires)?;
        ser.serialize_field("active", &self.active)?;
        ser.end()
    }
}

pub struct DeviceTokenPair {
    pub token: DeviceToken,
    pub key: DeviceTokenKey,
}

impl DeviceTokenPair {
    #[tracing::instrument(skip(exec))]
    async fn refresh_and_get_device_token(
        current_date: DateTime<Utc>,
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite> + Copy,
    ) -> crate::Result<(Self, DateTime<Utc>)> {
        let pair = Self::get(exec).await?;

        if let Some(mut pair) = pair {
            if pair.token.not_after > current_date {
                Ok((pair, current_date))
            } else {
                let res = device_token(&pair.key, current_date).await?;

                pair.token = res.value;
                pair.upsert(exec).await?;

                Ok((pair, res.date))
            }
        } else {
            let key = generate_key()?;
            let res = device_token(&key, current_date).await?;

            let pair = Self {
                key,
                token: res.value,
            };

            pair.upsert(exec).await?;

            Ok((pair, res.date))
        }
    }

    async fn get(
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    ) -> crate::Result<Option<Self>> {
        let res = sqlx::query!(
            r#"
            SELECT
                uuid, private_key, x, y, issue_instant, not_after, token, json(display_claims) as "display_claims!: serde_json::Value"
            FROM minecraft_device_tokens
            "#
        )
            .fetch_optional(exec)
            .await?;

        if let Some(x) = res {
            if let Ok(uuid) = Uuid::parse_str(&x.uuid) {
                if let Ok(private_key) =
                    SigningKey::from_pkcs8_pem(&x.private_key)
                {
                    return Ok(Some(Self {
                        token: DeviceToken {
                            issue_instant: Utc
                                .timestamp_opt(x.issue_instant, 0)
                                .single()
                                .unwrap_or_else(Utc::now),
                            not_after: Utc
                                .timestamp_opt(x.not_after, 0)
                                .single()
                                .unwrap_or_else(Utc::now),
                            token: x.token,
                            display_claims: serde_json::from_value(
                                x.display_claims,
                            )
                            .unwrap_or_default(),
                        },
                        key: DeviceTokenKey {
                            id: uuid,
                            key: private_key,
                            x: x.x,
                            y: x.y,
                        },
                    }));
                }
            }
        }

        Ok(None)
    }

    pub async fn upsert(
        &self,
        exec: impl sqlx::Executor<'_, Database = sqlx::Sqlite>,
    ) -> crate::Result<()> {
        let uuid = self.key.id.as_hyphenated().to_string();
        let issue_instant = self.token.issue_instant.timestamp();
        let not_after = self.token.not_after.timestamp();
        let key = self
            .key
            .key
            .to_pkcs8_pem(LineEnding::default())
            .map_err(MinecraftAuthenticationError::PEMSerialize)?
            .to_string();
        let display_claims = serde_json::to_string(&self.token.display_claims)?;

        sqlx::query!(
            "
            INSERT INTO minecraft_device_tokens (id, uuid, private_key, x, y, issue_instant, not_after, token, display_claims)
            VALUES (0, $1, $2, $3, $4, $5, $6, $7, $8)
            ON CONFLICT (id) DO UPDATE SET
                uuid = $1,
                private_key = $2,
                x = $3,
                y = $4,
                issue_instant = $5,
                not_after = $6,
                token = $7,
                display_claims = jsonb($8)
            ",
            uuid,
            key,
            self.key.x,
            self.key.y,
            issue_instant,
            not_after,
            self.token.token,
            display_claims,
        )
            .execute(exec)
            .await?;

        Ok(())
    }
}

const MICROSOFT_CLIENT_ID: &str = "00000000402b5328";
const AUTH_REPLY_URL: &str = "https://login.live.com/oauth20_desktop.srf";
const REQUESTED_SCOPE: &str = "service::user.auth.xboxlive.com::MBI_SSL";

struct RequestWithDate<T> {
    pub date: DateTime<Utc>,
    pub value: T,
}

// flow steps
#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "PascalCase")]
pub struct DeviceToken {
    pub issue_instant: DateTime<Utc>,
    pub not_after: DateTime<Utc>,
    pub token: String,
    pub display_claims: HashMap<String, serde_json::Value>,
}

#[tracing::instrument(skip(key))]
pub async fn device_token(
    key: &DeviceTokenKey,
    current_date: DateTime<Utc>,
) -> Result<RequestWithDate<DeviceToken>, MinecraftAuthenticationError> {
    let res = send_signed_request(
        None,
        "https://device.auth.xboxlive.com/device/authenticate",
        "/device/authenticate",
        json!({
            "Properties": {
                "AuthMethod": "ProofOfPossession",
                "Id": format!("{{{}}}", key.id.to_string().to_uppercase()),
                "DeviceType": "Win32",
                "Version": "10.16.0",
                "ProofKey": {
                    "kty": "EC",
                    "x": key.x,
                    "y": key.y,
                    "crv": "P-256",
                    "alg": "ES256",
                    "use": "sig"
                }
            },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"

        }),
        key,
        MinecraftAuthStep::GetDeviceToken,
        current_date,
    )
    .await?;

    Ok(RequestWithDate {
        date: res.current_date,
        value: res.body,
    })
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RedirectUri {
    pub msa_oauth_redirect: String,
}

#[tracing::instrument(skip(key))]
async fn sisu_authenticate(
    token: &str,
    challenge: &str,
    key: &DeviceTokenKey,
    current_date: DateTime<Utc>,
) -> Result<(String, RequestWithDate<RedirectUri>), MinecraftAuthenticationError>
{
    let res = send_signed_request::<RedirectUri>(
        None,
        "https://sisu.xboxlive.com/authenticate",
        "/authenticate",
        json!({
          "AppId": MICROSOFT_CLIENT_ID,
          "DeviceToken": token,
          "Offers": [
            REQUESTED_SCOPE
          ],
          "Query": {
            "code_challenge": challenge,
            "code_challenge_method": "S256",
            "state": generate_oauth_challenge(),
            "prompt": "select_account"
          },
          "RedirectUri": AUTH_REPLY_URL,
          "Sandbox": "RETAIL",
          "TokenType": "code",
          "TitleId": "1794566092",
        }),
        key,
        MinecraftAuthStep::SisuAuthenticate,
        current_date,
    )
    .await?;

    let session_id = res
        .headers
        .get("X-SessionId")
        .and_then(|x| x.to_str().ok())
        .ok_or_else(|| MinecraftAuthenticationError::NoSessionId)?
        .to_string();

    Ok((
        session_id,
        RequestWithDate {
            date: res.current_date,
            value: res.body,
        },
    ))
}

#[derive(Deserialize)]
struct OAuthToken {
    // pub token_type: String,
    pub expires_in: u64,
    // pub scope: String,
    pub access_token: String,
    pub refresh_token: String,
    // pub user_id: String,
    // pub foci: String,
}

#[tracing::instrument]
async fn oauth_token(
    code: &str,
    verifier: &str,
) -> Result<RequestWithDate<OAuthToken>, MinecraftAuthenticationError> {
    let mut query = HashMap::new();
    query.insert("client_id", MICROSOFT_CLIENT_ID);
    query.insert("code", code);
    query.insert("code_verifier", verifier);
    query.insert("grant_type", "authorization_code");
    query.insert("redirect_uri", AUTH_REPLY_URL);
    query.insert("scope", REQUESTED_SCOPE);

    let res = auth_retry(|| {
        REQWEST_CLIENT
            .post("https://login.live.com/oauth20_token.srf")
            .header("Accept", "application/json")
            .form(&query)
            .send()
    })
    .await
    .map_err(|source| MinecraftAuthenticationError::Request {
        source,
        step: MinecraftAuthStep::GetOAuthToken,
    })?;

    let status = res.status();
    let current_date = get_date_header(res.headers());
    let text = res.text().await.map_err(|source| {
        MinecraftAuthenticationError::Request {
            source,
            step: MinecraftAuthStep::GetOAuthToken,
        }
    })?;

    let body = serde_json::from_str(&text).map_err(|source| {
        MinecraftAuthenticationError::DeserializeResponse {
            source,
            raw: text,
            step: MinecraftAuthStep::GetOAuthToken,
            status_code: status,
        }
    })?;

    Ok(RequestWithDate {
        date: current_date,
        value: body,
    })
}

#[tracing::instrument]
async fn oauth_refresh(
    refresh_token: &str,
) -> Result<RequestWithDate<OAuthToken>, MinecraftAuthenticationError> {
    let mut query = HashMap::new();
    query.insert("client_id", MICROSOFT_CLIENT_ID);
    query.insert("refresh_token", refresh_token);
    query.insert("grant_type", "refresh_token");
    query.insert("redirect_uri", AUTH_REPLY_URL);
    query.insert("scope", REQUESTED_SCOPE);

    let res = auth_retry(|| {
        REQWEST_CLIENT
            .post("https://login.live.com/oauth20_token.srf")
            .header("Accept", "application/json")
            .form(&query)
            .send()
    })
    .await
    .map_err(|source| MinecraftAuthenticationError::Request {
        source,
        step: MinecraftAuthStep::RefreshOAuthToken,
    })?;

    let status = res.status();
    let current_date = get_date_header(res.headers());
    let text = res.text().await.map_err(|source| {
        MinecraftAuthenticationError::Request {
            source,
            step: MinecraftAuthStep::RefreshOAuthToken,
        }
    })?;

    let body = serde_json::from_str(&text).map_err(|source| {
        MinecraftAuthenticationError::DeserializeResponse {
            source,
            raw: text,
            step: MinecraftAuthStep::RefreshOAuthToken,
            status_code: status,
        }
    })?;

    Ok(RequestWithDate {
        date: current_date,
        value: body,
    })
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct SisuAuthorize {
    // pub authorization_token: DeviceToken,
    // pub device_token: String,
    // pub sandbox: String,
    pub title_token: DeviceToken,
    pub user_token: DeviceToken,
    // pub web_page: String,
}

#[tracing::instrument(skip(key))]
async fn sisu_authorize(
    session_id: Option<&str>,
    access_token: &str,
    device_token: &str,
    key: &DeviceTokenKey,
    current_date: DateTime<Utc>,
) -> Result<RequestWithDate<SisuAuthorize>, MinecraftAuthenticationError> {
    let res = send_signed_request(
        None,
        "https://sisu.xboxlive.com/authorize",
        "/authorize",
        json!({
            "AccessToken": format!("t={access_token}"),
            "AppId": MICROSOFT_CLIENT_ID,
            "DeviceToken": device_token,
            "ProofKey": {
                "kty": "EC",
                "x": key.x,
                "y": key.y,
                "crv": "P-256",
                "alg": "ES256",
                "use": "sig"
            },
            "Sandbox": "RETAIL",
            "SessionId": session_id,
            "SiteName": "user.auth.xboxlive.com",
            "RelyingParty": "http://xboxlive.com",
            "UseModernGamertag": true
        }),
        key,
        MinecraftAuthStep::SisuAuthorize,
        current_date,
    )
    .await?;

    Ok(RequestWithDate {
        date: res.current_date,
        value: res.body,
    })
}

#[tracing::instrument(skip(key))]
async fn xsts_authorize(
    authorize: SisuAuthorize,
    device_token: &str,
    key: &DeviceTokenKey,
    current_date: DateTime<Utc>,
) -> Result<RequestWithDate<DeviceToken>, MinecraftAuthenticationError> {
    let res = send_signed_request(
        None,
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        "/xsts/authorize",
        json!({
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT",
            "Properties": {
                "SandboxId": "RETAIL",
                "UserTokens": [authorize.user_token.token],
                "DeviceToken": device_token,
                "TitleToken": authorize.title_token.token,
            },
        }),
        key,
        MinecraftAuthStep::XstsAuthorize,
        current_date,
    )
    .await?;

    Ok(RequestWithDate {
        date: res.current_date,
        value: res.body,
    })
}

#[derive(Deserialize)]
struct MinecraftToken {
    // pub username: String,
    pub access_token: String,
    // pub token_type: String,
    // pub expires_in: u64,
}

#[tracing::instrument]
async fn minecraft_token(
    token: DeviceToken,
) -> Result<MinecraftToken, MinecraftAuthenticationError> {
    let uhs = token
        .display_claims
        .get("xui")
        .and_then(|x| x.get(0))
        .and_then(|x| x.get("uhs"))
        .and_then(|x| x.as_str().map(String::from))
        .ok_or_else(|| MinecraftAuthenticationError::NoUserHash)?;

    let token = token.token;

    let res = auth_retry(|| {
        REQWEST_CLIENT
            .post("https://api.minecraftservices.com/launcher/login")
            .header("Accept", "application/json")
            .json(&json!({
                "platform": "PC_LAUNCHER",
                "xtoken": format!("XBL3.0 x={uhs};{token}"),
            }))
            .send()
    })
    .await
    .map_err(|source| MinecraftAuthenticationError::Request {
        source,
        step: MinecraftAuthStep::MinecraftToken,
    })?;

    let status = res.status();
    let text = res.text().await.map_err(|source| {
        MinecraftAuthenticationError::Request {
            source,
            step: MinecraftAuthStep::MinecraftToken,
        }
    })?;

    serde_json::from_str(&text).map_err(|source| {
        MinecraftAuthenticationError::DeserializeResponse {
            source,
            raw: text,
            step: MinecraftAuthStep::MinecraftToken,
            status_code: status,
        }
    })
}

#[derive(
    sqlx::Type, Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq,
)]
#[serde(rename_all = "UPPERCASE")]
#[sqlx(rename_all = "UPPERCASE")]
pub enum MinecraftSkinVariant {
    /// The classic player model, with arms that are 4 pixels wide.
    Classic,
    /// The slim player model, with arms that are 3 pixels wide.
    Slim,
    /// The player model is unknown.
    #[serde(other)]
    Unknown, // Defensive handling of unexpected Mojang API return values to
             // prevent breaking the entire profile parsing
}

#[derive(Deserialize, Serialize, Debug, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum MinecraftCharacterExpressionState {
    /// This expression is selected for being displayed ingame.
    ///
    /// At the moment, at most one expression can be selected at a time.
    Active,
    /// This expression is not selected for being displayed ingame.
    Inactive,
    /// The expression selection status is unknown.
    #[serde(other)]
    Unknown, // Defensive handling of unexpected Mojang API return values to
             // prevent breaking the entire profile parsing
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct MinecraftSkin {
    /// The UUID of this skin object.
    ///
    /// As of 2025-04-08, in the production Mojang profile endpoint this UUID
    /// changes every time the player changes their skin, even if the skin
    /// texture is the same as before.
    pub id: Uuid,
    /// The selection state of the skin.
    ///
    /// As of 2025-04-08, in the production Mojang profile endpoint this
    /// is always `ACTIVE`, as only a single skin representing the current
    /// skin is returned.
    pub state: MinecraftCharacterExpressionState,
    /// The URL to the skin texture.
    ///
    /// As of 2025-04-08, in the production Mojang profile endpoint the file
    /// name for this URL is a hash of the skin texture, so that different
    /// players using the same skin texture will share a texture URL.
    pub url: Arc<Url>,
    /// A hash of the skin texture.
    ///
    /// As of 2025-04-08, in the production Mojang profile endpoint this
    /// is always set and the same as the file name of the skin texture URL.
    #[serde(
        default, // Defensive handling of unexpected Mojang API return values to
                 // prevent breaking the entire profile parsing
        rename = "textureKey"
    )]
    pub texture_key: Option<Arc<str>>,
    /// The player model variant this skin is for.
    pub variant: MinecraftSkinVariant,
    /// User-friendly name for the skin.
    ///
    /// As of 2025-04-08, in the production Mojang profile endpoint this is
    /// only set if the player has not set a custom skin, and this skin object
    /// is therefore the default skin for the player's UUID.
    #[serde(
        default,
        rename = "alias",
        deserialize_with = "normalize_skin_alias_case"
    )]
    pub name: Option<String>,
}

impl MinecraftSkin {
    /// Robustly computes the texture key for this skin, falling back to its
    /// URL file name and finally to the skin UUID when necessary.
    pub fn texture_key(&self) -> Arc<str> {
        self.texture_key.as_ref().cloned().unwrap_or_else(|| {
            self.url
                .path_segments()
                .and_then(|mut path_segments| {
                    path_segments.next_back().map(String::from)
                })
                .unwrap_or_else(|| self.id.as_simple().to_string())
                .into()
        })
    }
}

fn normalize_skin_alias_case<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    // Skin aliases have been spotted to be returned in all caps, so make sure
    // they are normalized to a prettier title case
    Ok(<Option<Cow<'_, str>>>::deserialize(deserializer)?
        .map(|alias| alias.to_title_case()))
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct MinecraftCape {
    /// The UUID of the cape.
    pub id: Uuid,
    /// The selection state of the cape.
    pub state: MinecraftCharacterExpressionState,
    /// The URL to the cape texture.
    pub url: Arc<Url>,
    /// The user-friendly name for the cape.
    #[serde(rename = "alias")]
    pub name: Arc<str>,
}

#[derive(Deserialize, Serialize, Debug, Default, Clone)]
pub struct MinecraftProfile {
    /// The UUID of the player.
    #[serde(default)]
    pub id: Uuid,
    /// The username of the player.
    pub name: String,
    /// The skins the player is known to have.
    ///
    /// As of 2025-04-08, in the production Mojang profile endpoint every
    /// player has a single skin.
    pub skins: Vec<MinecraftSkin>,
    /// The capes the player is known to have.
    pub capes: Vec<MinecraftCape>,
    /// The instant when the profile was fetched. See also [Self::is_fresh].
    #[serde(skip)]
    pub fetch_time: Option<Instant>,
}

impl MinecraftProfile {
    /// Checks whether the profile data is fresh (i.e., highly likely to be
    /// up-to-date because it was fetched recently) or stale. If it is not
    /// known when this profile data has been fetched from Mojang servers (i.e.,
    /// `fetch_time` is `None`), the profile is considered stale.
    ///
    /// This can be used to determine if the profile data should be fetched again
    /// from the Mojang API: the vanilla launcher was seen refreshing profile
    /// data every 60 seconds when re-entering the skin selection screen, and
    /// external applications may change this data at any time.
    fn is_fresh(&self) -> bool {
        self.fetch_time.is_some_and(|last_profile_fetch_time| {
            Instant::now().saturating_duration_since(last_profile_fetch_time)
                < std::time::Duration::from_secs(60)
        })
    }

    /// Returns the currently selected skin for this profile.
    pub fn current_skin(&self) -> crate::Result<&MinecraftSkin> {
        Ok(self
            .skins
            .iter()
            .find(|skin| {
                skin.state == MinecraftCharacterExpressionState::Active
            })
            // There should always be one active skin, even when the player uses their default skin
            .ok_or_else(|| {
                ErrorKind::OtherError("No active skin found".into())
            })?)
    }

    /// Returns the currently selected cape for this profile.
    pub fn current_cape(&self) -> Option<&MinecraftCape> {
        self.capes.iter().find(|cape| {
            cape.state == MinecraftCharacterExpressionState::Active
        })
    }
}

pub enum MaybeOnlineMinecraftProfile<'profile> {
    /// An online profile, fetched from the Mojang API.
    Online(Arc<MinecraftProfile>),
    /// An offline profile, which has not been fetched from the Mojang API.
    Offline(&'profile MinecraftProfile),
}

impl Deref for MaybeOnlineMinecraftProfile<'_> {
    type Target = MinecraftProfile;

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Online(profile) => profile,
            Self::Offline(profile) => profile,
        }
    }
}

#[tracing::instrument(skip(token))]
async fn minecraft_profile(
    token: &str,
) -> Result<MinecraftProfile, MinecraftAuthenticationError> {
    let res = auth_retry(|| {
        REQWEST_CLIENT
            .get("https://api.minecraftservices.com/minecraft/profile")
            .header("Accept", "application/json")
            .bearer_auth(token)
            // Profiles may be refreshed periodically in response to user actions,
            // so we want each refresh to be fast
            .timeout(std::time::Duration::from_secs(10))
            .send()
    })
    .await
    .map_err(|source| MinecraftAuthenticationError::Request {
        source,
        step: MinecraftAuthStep::MinecraftProfile,
    })?;

    let status = res.status();
    let text = res.text().await.map_err(|source| {
        MinecraftAuthenticationError::Request {
            source,
            step: MinecraftAuthStep::MinecraftProfile,
        }
    })?;

    let mut profile =
        serde_json::from_str::<MinecraftProfile>(&text).map_err(|source| {
            MinecraftAuthenticationError::DeserializeResponse {
                source,
                raw: text,
                step: MinecraftAuthStep::MinecraftProfile,
                status_code: status,
            }
        })?;
    profile.fetch_time = Some(Instant::now());

    tracing::debug!(
        "Successfully fetched Minecraft profile for {}",
        profile.name
    );

    Ok(profile)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftEntitlements {}

#[tracing::instrument]
async fn minecraft_entitlements(
    token: &str,
) -> Result<MinecraftEntitlements, MinecraftAuthenticationError> {
    let res = auth_retry(|| {
        REQWEST_CLIENT
            .get(format!("https://api.minecraftservices.com/entitlements/license?requestId={}", Uuid::new_v4()))
            .header("Accept", "application/json")
            .bearer_auth(token)
            .send()
    })
    .await.map_err(|source| MinecraftAuthenticationError::Request { source, step: MinecraftAuthStep::MinecraftEntitlements })?;

    let status = res.status();
    let text = res.text().await.map_err(|source| {
        MinecraftAuthenticationError::Request {
            source,
            step: MinecraftAuthStep::MinecraftEntitlements,
        }
    })?;

    serde_json::from_str(&text).map_err(|source| {
        MinecraftAuthenticationError::DeserializeResponse {
            source,
            raw: text,
            step: MinecraftAuthStep::MinecraftEntitlements,
            status_code: status,
        }
    })
}

// auth utils
#[tracing::instrument(skip(reqwest_request))]
async fn auth_retry<F>(
    reqwest_request: impl Fn() -> F,
) -> Result<reqwest::Response, reqwest::Error>
where
    F: Future<Output = Result<Response, reqwest::Error>>,
{
    const RETRY_COUNT: usize = 5; // Does command 9 times
    const RETRY_WAIT: std::time::Duration =
        std::time::Duration::from_millis(250);

    let mut resp = reqwest_request().await;
    for i in 0..RETRY_COUNT {
        match &resp {
            Ok(_) => {
                break;
            }
            Err(err) => {
                if err.is_connect() || err.is_timeout() {
                    if i < RETRY_COUNT - 1 {
                        tracing::debug!(
                            "Request failed with connect error, retrying...",
                        );
                        tokio::time::sleep(RETRY_WAIT).await;
                        resp = reqwest_request().await;
                    } else {
                        break;
                    }
                }
            }
        }
    }

    resp
}

pub struct DeviceTokenKey {
    pub id: Uuid,
    pub key: SigningKey,
    pub x: String,
    pub y: String,
}

#[tracing::instrument]
fn generate_key() -> Result<DeviceTokenKey, MinecraftAuthenticationError> {
    let uuid = Uuid::new_v4();

    let signing_key = SigningKey::random(&mut OsRng);
    let public_key = VerifyingKey::from(&signing_key);

    let encoded_point = public_key.to_encoded_point(false);

    Ok(DeviceTokenKey {
        id: uuid,
        key: signing_key,
        x: BASE64_URL_SAFE_NO_PAD.encode(
            encoded_point.x().ok_or_else(|| {
                MinecraftAuthenticationError::ReadingPublicKey
            })?,
        ),
        y: BASE64_URL_SAFE_NO_PAD.encode(
            encoded_point.y().ok_or_else(|| {
                MinecraftAuthenticationError::ReadingPublicKey
            })?,
        ),
    })
}

struct SignedRequestResponse<T> {
    pub headers: HeaderMap,
    pub current_date: DateTime<Utc>,
    pub body: T,
}

#[tracing::instrument(skip(key))]
async fn send_signed_request<T: DeserializeOwned>(
    authorization: Option<&str>,
    url: &str,
    url_path: &str,
    raw_body: serde_json::Value,
    key: &DeviceTokenKey,
    step: MinecraftAuthStep,
    current_date: DateTime<Utc>,
) -> Result<SignedRequestResponse<T>, MinecraftAuthenticationError> {
    let auth = authorization.map_or(Vec::new(), |v| v.as_bytes().to_vec());

    let body = serde_json::to_vec(&raw_body).map_err(|source| {
        MinecraftAuthenticationError::SerializeBody { source, step }
    })?;
    let time: u128 =
        { ((current_date.timestamp() as u128) + 11644473600) * 10000000 };

    let mut buffer = Vec::new();
    buffer.extend_from_slice(&1_u32.to_be_bytes()[..]);
    buffer.push(0_u8);
    buffer.extend_from_slice(&(time as u64).to_be_bytes()[..]);
    buffer.push(0_u8);
    buffer.extend_from_slice("POST".as_bytes());
    buffer.push(0_u8);
    buffer.extend_from_slice(url_path.as_bytes());
    buffer.push(0_u8);
    buffer.extend_from_slice(&auth);
    buffer.push(0_u8);
    buffer.extend_from_slice(&body);
    buffer.push(0_u8);

    let ecdsa_sig: Signature = key.key.sign(&buffer);

    let mut sig_buffer = Vec::new();
    sig_buffer.extend_from_slice(&1_i32.to_be_bytes()[..]);
    sig_buffer.extend_from_slice(&(time as u64).to_be_bytes()[..]);
    sig_buffer.extend_from_slice(&ecdsa_sig.r().to_bytes());
    sig_buffer.extend_from_slice(&ecdsa_sig.s().to_bytes());

    let signature = BASE64_STANDARD.encode(&sig_buffer);

    let res = auth_retry(|| {
        let mut request = REQWEST_CLIENT
            .post(url)
            .header("Content-Type", "application/json; charset=utf-8")
            .header("Accept", "application/json")
            .header("Signature", &signature);

        if url != "https://sisu.xboxlive.com/authorize" {
            request = request.header("x-xbl-contract-version", "1");
        }

        if let Some(auth) = authorization {
            request = request.header("Authorization", auth);
        }

        request.body(body.clone()).send()
    })
    .await
    .map_err(|source| MinecraftAuthenticationError::Request { source, step })?;

    let status = res.status();
    let headers = res.headers().clone();

    let current_date = get_date_header(&headers);

    let body = res.text().await.map_err(|source| {
        MinecraftAuthenticationError::Request { source, step }
    })?;

    let body = serde_json::from_str(&body).map_err(|source| {
        MinecraftAuthenticationError::DeserializeResponse {
            source,
            raw: body,
            step,
            status_code: status,
        }
    })?;
    Ok(SignedRequestResponse {
        headers,
        current_date,
        body,
    })
}

#[tracing::instrument]
fn get_date_header(headers: &HeaderMap) -> DateTime<Utc> {
    headers
        .get(reqwest::header::DATE)
        .and_then(|x| x.to_str().ok())
        .and_then(|x| DateTime::parse_from_rfc2822(x).ok())
        .map_or(Utc::now(), |x| x.with_timezone(&Utc))
}

#[tracing::instrument]
fn generate_oauth_challenge() -> String {
    let mut rng = rand::thread_rng();

    let bytes: Vec<u8> = (0..64).map(|_| rng.r#gen::<u8>()).collect();
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
