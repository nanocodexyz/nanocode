use std::sync::Arc;

use nanocode::{ClaudeModel, HarnessFamily, Model, ModelSelection};

const KEY: &str = "OPENCODE_API_KEY";
const SAVED_KEY: &str = "opencode-api-key";
const MODELS_URL: &str = "https://opencode.ai/inference/go/v1/models";
const MESSAGES_URL: &str = "https://opencode.ai/inference/go/anthropic/v1/messages";
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Api {
    ChatCompletions,
    Responses,
    Messages,
}

impl Api {
    fn parse(value: &str) -> Option<Self> {
        match value {
            "chat-completions" => Some(Self::ChatCompletions),
            "responses" => Some(Self::Responses),
            "messages" => Some(Self::Messages),
            _ => None,
        }
    }
}

pub(crate) struct Offer {
    id: String,
    name: String,
    api: Api,
    window: Option<u64>,
    monthly: Option<u64>,
}

type Listing = Arc<nanocode_sync::SetOnce<Result<Vec<Offer>, String>>>;

#[derive(Clone)]
pub(crate) struct OpenCodeGo {
    key: Arc<str>,
    listing: Listing,
}

impl OpenCodeGo {
    pub(crate) const NAME: &str = "opencode-go";
    pub(crate) const PREFIX: &str = "opencode-go/";
    pub(crate) const ABOUT: &str = "models your OpenCode Go subscription serves";

    pub(crate) fn chat(&self) -> nanocode::codex::OpenAiBuilder {
        nanocode::OpenAi::builder(nanocode::codex::OpenAiAuth::api_key(Arc::clone(&self.key)))
            .chat_gateway(nanocode::codex::transport::ChatGateway::opencode_go())
    }

    pub(crate) fn responses(&self) -> nanocode::codex::OpenAiBuilder {
        nanocode::OpenAi::builder(nanocode::codex::OpenAiAuth::api_key(Arc::clone(&self.key)))
            .responses_gateway(nanocode::codex::transport::ResponsesGateway::opencode_go())
    }

    pub(crate) fn claude(&self) -> Result<nanocode::claude::ClaudeClient, String> {
        let mut headers = nanocode_http::types::header::HeaderMap::new();
        let mut value =
            nanocode_http::types::header::HeaderValue::from_str(&format!("Bearer {}", self.key))
                .map_err(|error| format!("{KEY}: {error}"))?;
        value.set_sensitive(true);
        headers.insert(nanocode_http::types::header::AUTHORIZATION, value);
        Ok(nanocode::claude::ClaudeClient::with_auth_headers(
            nanocode::claude::ClaudeClient::http_client(),
            MESSAGES_URL.to_owned(),
            headers,
        ))
    }

    pub(crate) async fn load(&self) -> Result<(), String> {
        self.listing
            .wait()
            .await
            .as_ref()
            .map(drop)
            .map_err(Clone::clone)
    }

    fn offers(&self) -> Result<&[Offer], String> {
        match self.listing.get() {
            None => Err(format!("listing OpenCode Go models from {MODELS_URL}…")),
            Some(Err(error)) => Err(error.clone()),
            Some(Ok(offers)) => Ok(offers),
        }
    }

    fn offer(&self, id: &str) -> Option<&Offer> {
        self.offers().ok()?.iter().find(|offer| offer.id == id)
    }

    pub(crate) fn api(&self, id: &str) -> Option<Api> {
        self.offer(id).map(|offer| offer.api)
    }

    pub(crate) fn window(&self, id: &str) -> Option<u64> {
        self.offer(id)?.window
    }

    pub(crate) fn model(&self, id: &str) -> Result<ModelSelection, String> {
        let offers = self.offers()?;
        let offer = offers.iter().find(|offer| offer.id == id).ok_or_else(|| {
            format!(
                "OpenCode Go serves no agent model {id:?}; it serves {}",
                offers
                    .iter()
                    .map(|offer| offer.id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
        Ok(Self::selection(offer))
    }

    fn selection(offer: &Offer) -> ModelSelection {
        match offer.api {
            Api::Messages => match offer.id.parse::<ClaudeModel>() {
                Ok(model) => ModelSelection::Claude(model),
                Err(_) => ModelSelection::new(HarnessFamily::Claude, &offer.id),
            },
            Api::ChatCompletions | Api::Responses => match Model::known(&offer.id)
                .filter(|model| Model::ALL.contains(model) && model.as_str() == offer.id)
            {
                Some(model) => ModelSelection::Codex(model),
                None => ModelSelection::Codex(Model::Custom(nanocode::CustomModel::new(&offer.id))),
            },
        }
    }

    pub(crate) fn group(&self) -> Result<Vec<(ModelSelection, Option<String>)>, String> {
        Ok(self
            .offers()?
            .iter()
            .map(|offer| {
                let api = match offer.api {
                    Api::ChatCompletions => "chat completions",
                    Api::Responses => "responses",
                    Api::Messages => "messages",
                };
                let about = [
                    (offer.name != offer.id).then(|| offer.name.clone()),
                    Some(api.to_owned()),
                    offer.monthly.map(|dollars| format!("{dollars} USD a month on Go")),
                ]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" · ");
                (Self::selection(offer), Some(about))
            })
            .collect())
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

pub fn apply(clients: crate::host::ModelClients) -> Result<crate::host::ModelClients, String> {
    let home = nanocode_home::AgentHome::from_env().map_err(|error| error.to_string())?;
    let path = home.nanocode_home().join(SAVED_KEY);
    let key = match env(KEY) {
        Some(key) => {
            if let Err(error) = crate::custom_endpoint::remember(&path, &key) {
                eprintln!(
                    "nanocode: could not save {KEY} to {}: {error}",
                    path.display()
                );
            }
            key
        }
        None => match crate::custom_endpoint::saved(&path)? {
            Some(key) => key,
            None => return Ok(clients),
        },
    };
    nanocode_http::types::header::HeaderValue::from_str(&format!("Bearer {key}"))
        .map_err(|error| format!("{KEY}: {error}"))?;
    nanocode::codex::transport::install_default_rustls_crypto_provider();
    let listing = Listing::default();
    let filled = Arc::clone(&listing);
    std::thread::Builder::new()
        .name("nanocode-opencode-go".to_owned())
        .spawn(move || drop(filled.set(list())))
        .map_err(|error| format!("failed to start listing OpenCode Go models: {error}"))?;
    Ok(clients.opencode_go(OpenCodeGo {
        key: Arc::from(key),
        listing,
    }))
}

fn list() -> Result<Vec<Offer>, String> {
    #[derive(serde::Deserialize)]
    struct List {
        data: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        id: String,
        name: Option<String>,
        recommended_api: Option<String>,
        limit: Option<Limit>,
        go: Option<Go>,
    }
    #[derive(serde::Deserialize)]
    struct Limit {
        context: Option<u64>,
    }
    #[derive(serde::Deserialize)]
    struct Go {
        monthly_usage: Option<Usage>,
    }
    #[derive(serde::Deserialize)]
    struct Usage {
        go: Option<u64>,
    }
    let runtime = nanocode_executor::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let list: List = runtime.block_on(async {
        let response = nanocode_http::client()
            .get(MODELS_URL)
            .timeout(LIST_TIMEOUT)
            .send()
            .await
            .map(nanocode_http::decode_flate_response)
            .map_err(|error| {
                format!(
                    "failed to list OpenCode Go models from {MODELS_URL}: {}",
                    nanocode_http::error_detail(&error)
                )
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(format!(
                "failed to list OpenCode Go models from {MODELS_URL}: HTTP {status}"
            ));
        }
        response
            .json()
            .await
            .map_err(|error| format!("{MODELS_URL} did not return a model list: {error}"))
    })?;
    let offers = list
        .data
        .into_iter()
        .filter_map(|entry| {
            let api = Api::parse(entry.recommended_api.as_deref()?)?;
            Some(Offer {
                name: entry.name.unwrap_or_else(|| entry.id.clone()),
                id: entry.id,
                api,
                window: entry.limit.and_then(|limit| limit.context),
                monthly: entry
                    .go
                    .and_then(|go| go.monthly_usage)
                    .and_then(|usage| usage.go),
            })
        })
        .collect::<Vec<_>>();
    if offers.is_empty() {
        return Err(format!("{MODELS_URL} lists no agent models"));
    }
    Ok(offers)
}
