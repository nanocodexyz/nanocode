use crate::wire::{self, named};
use crate::{CLOUD_PLATFORM_SCOPE, CredentialKind, Error, Token};
use nanocode_http::Method;
use nanocode_http::header::HeaderName;
use nanocode_http::layer::{BoxFuture, Sender};
use nanocode_sys::base64::{STANDARD, URL_SAFE_NO_PAD};
use serde_json::{Value, json};
use std::time::Duration;

pub const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub const STS_TOKEN_URL: &str = "https://sts.googleapis.com/v1/token";
pub const TOKEN_LIFETIME: Duration = Duration::from_secs(60 * 60);
const JWT_BEARER: &str = "urn:ietf:params:oauth:grant-type:jwt-bearer";
const TOKEN_EXCHANGE: &str = "urn:ietf:params:oauth:grant-type:token-exchange";
const ACCESS_TOKEN_TYPE: &str = "urn:ietf:params:oauth:token-type:access_token";
const PRIVATE_KEY_BEGIN: &str = "-----BEGIN PRIVATE KEY-----";
const PRIVATE_KEY_END: &str = "-----END PRIVATE KEY-----";

pub enum Source {
    Static(String),
    Refresh(Refresh),
    ServiceAccount(ServiceAccount),
    Impersonated(Box<Impersonated>),
    External(Box<External>),
    Metadata(String),
    Gcloud,
}

impl Source {
    pub fn identity(&self) -> String {
        match self {
            Self::Static(_) => "access-token".to_owned(),
            Self::Refresh(refresh) => format!(
                "{}:{}",
                refresh.client_id,
                nanocode_digest::blake3(refresh.refresh_token.as_bytes()).to_hex()
            ),
            Self::ServiceAccount(account) => account.email.clone(),
            Self::Impersonated(impersonated) => impersonated.url.clone(),
            Self::External(external) => external.audience.clone(),
            Self::Metadata(base) => base.clone(),
            Self::Gcloud => "gcloud".to_owned(),
        }
    }
}

pub struct Refresh {
    token_url: String,
    refresh_token: String,
    client_id: String,
    client_secret: String,
    basic: bool,
}

pub struct ServiceAccount {
    email: String,
    token_url: String,
    key_id: Option<String>,
    key: Vec<u8>,
}

pub struct Impersonated {
    source: Source,
    url: String,
    delegates: Vec<String>,
    lifetime: Duration,
}

pub struct External {
    audience: String,
    subject_type: String,
    token_url: String,
    subject: Subject,
    client: Option<(String, String)>,
    user_project: Option<String>,
    impersonation: Option<(String, Duration)>,
}

pub enum Subject {
    File {
        path: String,
        field: Option<String>,
    },
    Url {
        url: String,
        headers: Vec<(HeaderName, String)>,
        field: Option<String>,
    },
}

pub struct Parsed {
    pub source: Source,
    pub kind: CredentialKind,
    pub project: Option<String>,
    pub quota_project: Option<String>,
}

fn text(value: &Value, name: &str) -> Option<String> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn required(value: &Value, name: &str, kind: &str) -> Result<String, Error> {
    text(value, name).ok_or_else(|| Error::new(format!("{kind} credentials have no {name}")))
}

fn lifetime(value: Option<&Value>) -> Duration {
    value
        .and_then(Value::as_u64)
        .map_or(TOKEN_LIFETIME, Duration::from_secs)
}

pub fn parse(value: &Value) -> Result<Parsed, Error> {
    let kind = text(value, "type").ok_or_else(|| Error::new("the credentials have no type"))?;
    let quota_project = text(value, "quota_project_id");
    let parsed = |source, kind, project| Parsed {
        source,
        kind,
        project,
        quota_project: quota_project.clone(),
    };
    match kind.as_str() {
        "authorized_user" => Ok(parsed(
            Source::Refresh(Refresh {
                token_url: text(value, "token_uri").unwrap_or_else(|| GOOGLE_TOKEN_URL.to_owned()),
                refresh_token: required(value, "refresh_token", &kind)?,
                client_id: required(value, "client_id", &kind)?,
                client_secret: required(value, "client_secret", &kind)?,
                basic: false,
            }),
            CredentialKind::AuthorizedUser,
            None,
        )),
        "external_account_authorized_user" => Ok(parsed(
            Source::Refresh(Refresh {
                token_url: required(value, "token_url", &kind)?,
                refresh_token: required(value, "refresh_token", &kind)?,
                client_id: required(value, "client_id", &kind)?,
                client_secret: required(value, "client_secret", &kind)?,
                basic: true,
            }),
            CredentialKind::ExternalAccountAuthorizedUser,
            None,
        )),
        "service_account" => Ok(parsed(
            Source::ServiceAccount(service_account(value, &kind)?),
            CredentialKind::ServiceAccount,
            text(value, "project_id"),
        )),
        "impersonated_service_account" => Ok(parsed(
            Source::Impersonated(Box::new(impersonated(value, &kind)?)),
            CredentialKind::ImpersonatedServiceAccount,
            None,
        )),
        "external_account" => Ok(parsed(
            Source::External(Box::new(external(value)?)),
            CredentialKind::ExternalAccount,
            None,
        )),
        other => Err(Error::new(format!(
            "credentials of type {other:?} are not supported (supported: authorized_user, service_account, impersonated_service_account, external_account, external_account_authorized_user)"
        ))),
    }
}

fn service_account(value: &Value, kind: &str) -> Result<ServiceAccount, Error> {
    let pem = required(value, "private_key", kind)?;
    let body = pem
        .trim()
        .strip_prefix(PRIVATE_KEY_BEGIN)
        .and_then(|rest| rest.trim_end().strip_suffix(PRIVATE_KEY_END))
        .ok_or_else(|| {
            Error::new(format!(
                "the service_account private_key is not a {PRIVATE_KEY_BEGIN} PEM block"
            ))
        })?
        .split_whitespace()
        .collect::<String>();
    let key = STANDARD.decode(body).map_err(|error| {
        Error::new(format!(
            "the service_account private_key is not base64: {error:?}"
        ))
    })?;
    crate::sign::check_key(&key)?;
    Ok(ServiceAccount {
        email: required(value, "client_email", kind)?,
        token_url: text(value, "token_uri").unwrap_or_else(|| GOOGLE_TOKEN_URL.to_owned()),
        key_id: text(value, "private_key_id"),
        key,
    })
}

fn impersonated(value: &Value, kind: &str) -> Result<Impersonated, Error> {
    let inner = value.get("source_credentials").ok_or_else(|| {
        Error::new("impersonated_service_account credentials have no source_credentials")
    })?;
    let inner = parse(inner).map_err(|error| Error::new(format!("source_credentials: {error}")))?;
    let delegates = value
        .get("delegates")
        .and_then(Value::as_array)
        .map(|delegates| {
            delegates
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Ok(Impersonated {
        source: inner.source,
        url: required(value, "service_account_impersonation_url", kind)?,
        delegates,
        lifetime: TOKEN_LIFETIME,
    })
}

fn external(value: &Value) -> Result<External, Error> {
    let kind = "external_account";
    let source = value
        .get("credential_source")
        .ok_or_else(|| Error::new("external_account credentials have no credential_source"))?;
    if source.get("executable").is_some() {
        return Err(Error::new(
            "executable-sourced external_account credentials are not supported; use a file- or URL-sourced credential_source, or run gcloud auth application-default login",
        ));
    }
    if text(source, "environment_id").is_some_and(|id| id.starts_with("aws")) {
        return Err(Error::new(
            "AWS-sourced external_account credentials are not supported; use a file- or URL-sourced credential_source, or pass a token in GOOGLE_OAUTH_ACCESS_TOKEN",
        ));
    }
    let subject = subject(source, subject_field(source)?)?;
    let client =
        text(value, "client_id").map(|id| (id, text(value, "client_secret").unwrap_or_default()));
    let impersonation = text(value, "service_account_impersonation_url").map(|url| {
        let seconds = value
            .get("service_account_impersonation")
            .and_then(|options| options.get("token_lifetime_seconds"));
        (url, lifetime(seconds))
    });
    Ok(External {
        audience: required(value, "audience", kind)?,
        subject_type: required(value, "subject_token_type", kind)?,
        token_url: text(value, "token_url").unwrap_or_else(|| STS_TOKEN_URL.to_owned()),
        subject,
        user_project: text(value, "workforce_pool_user_project").filter(|_| client.is_none()),
        client,
        impersonation,
    })
}

fn subject_field(source: &Value) -> Result<Option<String>, Error> {
    let format = source.get("format");
    let field = match format.and_then(|format| text(format, "type")).as_deref() {
        None | Some("text") => None,
        Some("json") => Some(
            format
                .and_then(|format| text(format, "subject_token_field_name"))
                .ok_or_else(|| {
                    Error::new("credential_source.format has no subject_token_field_name")
                })?,
        ),
        Some(other) => {
            return Err(Error::new(format!(
                "credential_source.format type {other:?} is not text or json"
            )));
        }
    };
    Ok(field)
}

fn subject(source: &Value, field: Option<String>) -> Result<Subject, Error> {
    Ok(match (text(source, "file"), text(source, "url")) {
        (Some(path), _) => Subject::File { path, field },
        (None, Some(url)) => {
            let headers = source
                .get("headers")
                .and_then(Value::as_object)
                .map(|headers| {
                    headers
                        .iter()
                        .map(|(name, value)| {
                            let name =
                                HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
                                    Error::new(format!(
                                        "credential_source header {name:?}: {error}"
                                    ))
                                })?;
                            Ok((name, value.as_str().unwrap_or_default().to_owned()))
                        })
                        .collect::<Result<Vec<_>, Error>>()
                })
                .transpose()?
                .unwrap_or_default();
            Subject::Url {
                url,
                headers,
                field,
            }
        }
        (None, None) => {
            return Err(Error::new(
                "external_account credential_source names no file or url",
            ));
        }
    })
}

impl Source {
    pub fn fetch<'a>(&'a self, sender: &'a dyn Sender) -> BoxFuture<'a, Result<Token, Error>> {
        Box::pin(async move {
            match self {
                Self::Static(token) => Ok(Token::lasting(token.clone(), None)),
                Self::Refresh(refresh) => refresh.fetch(sender).await,
                Self::ServiceAccount(account) => account.fetch(sender).await,
                Self::Impersonated(impersonated) => {
                    let source = impersonated.source.fetch(sender).await?;
                    impersonate(
                        sender,
                        &impersonated.url,
                        &source.value,
                        &impersonated.delegates,
                        impersonated.lifetime,
                    )
                    .await
                }
                Self::External(external) => external.fetch(sender).await,
                Self::Metadata(base) => metadata_token(sender, base).await,
                Self::Gcloud => crate::discover::gcloud_token().await,
            }
        })
    }
}

impl Refresh {
    async fn fetch(&self, sender: &dyn Sender) -> Result<Token, Error> {
        let mut pairs = vec![
            ("grant_type", "refresh_token"),
            ("refresh_token", self.refresh_token.as_str()),
        ];
        let basic = match self.basic {
            true => Some((self.client_id.as_str(), self.client_secret.as_str())),
            false => {
                pairs.push(("client_id", &self.client_id));
                pairs.push(("client_secret", &self.client_secret));
                None
            }
        };
        let reply = wire::form(sender, &self.token_url, &pairs, basic).await?;
        wire::oauth_token(&reply, &self.token_url)
    }
}

impl ServiceAccount {
    async fn fetch(&self, sender: &dyn Sender) -> Result<Token, Error> {
        let now = nanocode_platform::system_time()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| Error::new(format!("the clock is before 1970: {error}")))?
            .as_secs();
        let header = match &self.key_id {
            Some(id) => json!({"alg": "RS256", "typ": "JWT", "kid": id}),
            None => json!({"alg": "RS256", "typ": "JWT"}),
        };
        let claims = json!({
            "iss": self.email,
            "scope": CLOUD_PLATFORM_SCOPE,
            "aud": self.token_url,
            "iat": now,
            "exp": now + TOKEN_LIFETIME.as_secs(),
        });
        let signed = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(header.to_string()),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let signature = crate::sign::rs256(&self.key, signed.as_bytes())?;
        let assertion = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature));
        let reply = wire::form(
            sender,
            &self.token_url,
            &[("grant_type", JWT_BEARER), ("assertion", &assertion)],
            None,
        )
        .await?;
        wire::oauth_token(&reply, &self.token_url)
    }
}

impl External {
    async fn subject(&self, sender: &dyn Sender) -> Result<String, Error> {
        let (raw, field, origin) = match &self.subject {
            Subject::File { path, field } => (
                crate::discover::read_file(path.clone())
                    .await
                    .map_err(|error| {
                        Error::new(format!("reading the subject token file {path}: {error}"))
                    })?,
                field,
                path,
            ),
            Subject::Url {
                url,
                headers,
                field,
            } => {
                let reply = wire::call(sender, Method::GET, url, headers, Vec::new()).await?;
                if !(200..300).contains(&reply.status) {
                    return Err(Error::new(format!(
                        "{url} returned {}: {}",
                        reply.status,
                        reply.text().trim()
                    )));
                }
                (reply.body, field, url)
            }
        };
        let token = match field {
            None => String::from_utf8_lossy(&raw).trim().to_owned(),
            Some(name) => {
                let value: Value = serde_json::from_slice(&raw).map_err(|error| {
                    Error::new(format!(
                        "the subject token from {origin} is not JSON: {error}"
                    ))
                })?;
                text(&value, name).ok_or_else(|| {
                    Error::new(format!("the subject token from {origin} has no {name}"))
                })?
            }
        };
        match token.is_empty() {
            true => Err(Error::new(format!(
                "the subject token from {origin} is empty"
            ))),
            false => Ok(token),
        }
    }

    async fn fetch(&self, sender: &dyn Sender) -> Result<Token, Error> {
        let subject = self.subject(sender).await?;
        let options = self
            .user_project
            .as_ref()
            .map(|project| json!({"userProject": project}).to_string());
        let mut pairs = vec![
            ("grant_type", TOKEN_EXCHANGE),
            ("audience", self.audience.as_str()),
            ("scope", CLOUD_PLATFORM_SCOPE),
            ("requested_token_type", ACCESS_TOKEN_TYPE),
            ("subject_token", subject.as_str()),
            ("subject_token_type", self.subject_type.as_str()),
        ];
        if let Some(options) = &options {
            pairs.push(("options", options));
        }
        let basic = self
            .client
            .as_ref()
            .map(|(id, secret)| (id.as_str(), secret.as_str()));
        let reply = wire::form(sender, &self.token_url, &pairs, basic).await?;
        let federated = wire::oauth_token(&reply, &self.token_url)?;
        match &self.impersonation {
            None => Ok(federated),
            Some((url, lifetime)) => {
                impersonate(sender, url, &federated.value, &[], *lifetime).await
            }
        }
    }
}

async fn impersonate(
    sender: &dyn Sender,
    url: &str,
    bearer: &str,
    delegates: &[String],
    lifetime: Duration,
) -> Result<Token, Error> {
    let mut body = json!({
        "scope": [CLOUD_PLATFORM_SCOPE],
        "lifetime": format!("{}s", lifetime.as_secs()),
    });
    if !delegates.is_empty() {
        body["delegates"] = json!(delegates);
    }
    let reply = wire::post_json(sender, url, bearer, &body).await?;
    wire::iam_token(&reply, url)
}

pub const METADATA_FLAVOR: (&str, &str) = ("metadata-flavor", "Google");

pub async fn metadata_get(sender: &dyn Sender, url: &str) -> Result<wire::Reply, Error> {
    wire::call(
        sender,
        Method::GET,
        url,
        &[(named(METADATA_FLAVOR.0), METADATA_FLAVOR.1.to_owned())],
        Vec::new(),
    )
    .await
}

async fn metadata_token(sender: &dyn Sender, base: &str) -> Result<Token, Error> {
    let url = format!("{base}/computeMetadata/v1/instance/service-accounts/default/token");
    let reply = metadata_get(sender, &url).await?.json(&url)?;
    wire::oauth_token(&reply, &url)
}

pub async fn metadata_project(sender: &dyn Sender, base: &str) -> Result<String, Error> {
    let url = format!("{base}/computeMetadata/v1/project/project-id");
    let reply = metadata_get(sender, &url).await?;
    let project = reply.text().trim().to_owned();
    match (200..300).contains(&reply.status) && !project.is_empty() {
        true => Ok(project),
        false => Err(Error::new(format!(
            "{url} returned {}: {project}",
            reply.status
        ))),
    }
}
