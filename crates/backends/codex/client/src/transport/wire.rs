use std::{collections::HashMap, sync::Arc};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::value::RawValue;

use super::ResponsesError;

pub struct EncodedRequest {
    json: Arc<String>,
    responses_lite: bool,
}

#[cfg(not(target_family = "wasm"))]
struct Shared(Arc<String>);

#[cfg(not(target_family = "wasm"))]
impl AsRef<[u8]> for Shared {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl EncodedRequest {
    pub fn new<T: Serialize + ?Sized>(request: &T) -> Result<Self, ResponsesError> {
        serde_json::to_string(request)
            .map(|json| Self {
                json: Arc::new(json),
                responses_lite: false,
            })
            .map_err(ResponsesError::EncodeRequest)
    }

    pub(crate) fn create(
        request: &crate::client::responses::ResponseCreate<'_>,
    ) -> Result<Self, ResponsesError> {
        request
            .encode()
            .map(|json| Self {
                json: Arc::new(json),
                responses_lite: request.responses_lite(),
            })
            .map_err(ResponsesError::EncodeRequest)
    }

    #[must_use]
    pub fn json(&self) -> &str {
        &self.json
    }

    #[must_use]
    pub const fn responses_lite(&self) -> bool {
        self.responses_lite
    }

    #[cfg(not(target_family = "wasm"))]
    pub(crate) fn body(&self) -> nanocode_bytes::Bytes {
        nanocode_bytes::Bytes::from_owner(Shared(Arc::clone(&self.json)))
    }

    #[must_use]
    pub fn into_string(self) -> String {
        Arc::unwrap_or_clone(self.json)
    }
}

pub(crate) struct DecodedEvent<'a, T> {
    pub(crate) raw: &'a str,
    pub(crate) decoded: Result<T, ResponsesError>,
}

pub(crate) fn decode_event<T: DeserializeOwned>(
    text: &str,
) -> Result<DecodedEvent<'_, T>, ResponsesError> {
    let raw = text.trim_matches([' ', '\t', '\n', '\r']);
    let decoded = match serde_json::from_str(raw) {
        Ok(event) => Ok(event),
        Err(source) => {
            serde_json::from_str::<&RawValue>(text).map_err(ResponsesError::InvalidJson)?;
            Err(ResponsesError::InvalidPayload {
                source,
                event: raw.to_owned(),
            })
        }
    };
    Ok(DecodedEvent { raw, decoded })
}

pub(crate) fn turn_state_from_event(text: &str) -> Option<String> {
    if text.starts_with(r#"{"type":""#) && !text.starts_with(r#"{"type":"response.metadata""#) {
        return None;
    }
    let Ok(MetadataEvent::Metadata { headers }) = serde_json::from_str(text) else {
        return None;
    };
    headers.into_iter().find_map(|(name, value)| {
        name.eq_ignore_ascii_case(TURN_STATE_HEADER)
            .then_some(value)
    })
}

const TURN_STATE_HEADER: &str = "x-codex-turn-state";

#[derive(Deserialize)]
#[serde(tag = "type")]
enum MetadataEvent {
    #[serde(rename = "response.metadata")]
    Metadata {
        #[serde(default)]
        headers: HashMap<String, String>,
    },
    #[serde(other)]
    Other,
}
