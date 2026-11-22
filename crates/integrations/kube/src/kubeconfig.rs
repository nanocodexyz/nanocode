use std::fmt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::error::Error;

const SERVICE_ACCOUNT: &str = "/var/run/secrets/kubernetes.io/serviceaccount";

#[derive(Clone, PartialEq, Eq)]
pub enum Auth {
    Token(String),
    Certificate {
        certificate_pem: Vec<u8>,
        key_pem: Vec<u8>,
    },
}

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub server: String,
    pub ca_pem: Vec<u8>,
    pub auth: Auth,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let auth = match self.auth {
            Auth::Token(_) => "token",
            Auth::Certificate { .. } => "certificate",
        };
        formatter
            .debug_struct("Credentials")
            .field("server", &self.server)
            .field("auth", &auth)
            .finish_non_exhaustive()
    }
}

fn read(path: &Path) -> Result<Vec<u8>, Error> {
    std::fs::read(path)
        .map_err(|error| Error::config(format!("cannot read '{}': {error}", path.display())))
}

impl Credentials {
    pub fn in_cluster() -> Result<Self, Error> {
        let variable = |name: &str| {
            std::env::var(name).map_err(|_| Error::config(format!("{name} is not set")))
        };
        let host = variable("KUBERNETES_SERVICE_HOST")?;
        let port = variable("KUBERNETES_SERVICE_PORT")?;
        let host = if host.contains(':') {
            format!("[{host}]")
        } else {
            host
        };
        let directory = Path::new(SERVICE_ACCOUNT);
        let token = String::from_utf8(read(&directory.join("token"))?)
            .map_err(|_| Error::config("service account token is not UTF-8"))?;
        let token = token.trim().to_owned();
        if token.is_empty() {
            return Err(Error::config("service account token is empty"));
        }
        Ok(Self {
            server: format!("https://{host}:{port}"),
            ca_pem: read(&directory.join("ca.crt"))?,
            auth: Auth::Token(token),
        })
    }

    pub fn from_kubeconfig_file(path: &Path, context: Option<&str>) -> Result<Self, Error> {
        let text =
            String::from_utf8(read(path)?).map_err(|_| Error::config("kubeconfig is not UTF-8"))?;
        let directory = path.parent().map_or_else(PathBuf::new, Path::to_path_buf);
        Self::from_kubeconfig(&text, &directory, context)
    }

    pub fn from_kubeconfig(
        text: &str,
        directory: &Path,
        context: Option<&str>,
    ) -> Result<Self, Error> {
        let config = parse_yaml(text)?;
        let current = config.get("current-context").and_then(Value::as_str);
        let context_name = context
            .or(current)
            .ok_or_else(|| Error::config("kubeconfig names no context"))?;
        let named = |list: &str, name: &str, field: &str| -> Result<Value, Error> {
            config
                .get(list)
                .and_then(Value::as_array)
                .and_then(|entries| {
                    entries
                        .iter()
                        .find(|entry| entry.get("name").and_then(Value::as_str) == Some(name))
                })
                .and_then(|entry| entry.get(field))
                .cloned()
                .ok_or_else(|| Error::config(format!("kubeconfig has no {list} entry '{name}'")))
        };
        let context = named("contexts", context_name, "context")?;
        let text_field =
            |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
        let cluster_name = text_field(&context, "cluster")
            .ok_or_else(|| Error::config("context names no cluster"))?;
        let user_name =
            text_field(&context, "user").ok_or_else(|| Error::config("context names no user"))?;
        let cluster = named("clusters", &cluster_name, "cluster")?;
        let user = named("users", &user_name, "user")?;
        if cluster
            .get("insecure-skip-tls-verify")
            .and_then(Value::as_bool)
            == Some(true)
        {
            return Err(Error::config("insecure-skip-tls-verify is refused"));
        }
        let server =
            text_field(&cluster, "server").ok_or_else(|| Error::config("cluster has no server"))?;
        let bytes = |value: &Value, data: &str, file: &str| -> Result<Option<Vec<u8>>, Error> {
            if let Some(encoded) = text_field(value, data) {
                return base64_decode(&encoded).map(Some);
            }
            match text_field(value, file) {
                Some(path) => read(&directory.join(path)).map(Some),
                None => Ok(None),
            }
        };
        let ca_pem = bytes(
            &cluster,
            "certificate-authority-data",
            "certificate-authority",
        )?
        .ok_or_else(|| Error::config("cluster has no certificate authority"))?;
        let certificate = bytes(&user, "client-certificate-data", "client-certificate")?;
        let key = bytes(&user, "client-key-data", "client-key")?;
        let token = match (text_field(&user, "token"), text_field(&user, "tokenFile")) {
            (Some(token), _) => Some(token),
            (None, Some(path)) => Some(
                String::from_utf8(read(&directory.join(path))?)
                    .map_err(|_| Error::config("token file is not UTF-8"))?
                    .trim()
                    .to_owned(),
            ),
            (None, None) => None,
        };
        let auth = match (certificate, key, token) {
            (Some(certificate_pem), Some(key_pem), None) => Auth::Certificate {
                certificate_pem,
                key_pem,
            },
            (None, None, Some(token)) if !token.is_empty() => Auth::Token(token),
            _ => {
                return Err(Error::config(
                    "user needs exactly one of a client certificate and key, or a token",
                ));
            }
        };
        Ok(Self {
            server: server.trim_end_matches('/').to_owned(),
            ca_pem,
            auth,
        })
    }
}

pub fn base64_decode(text: &str) -> Result<Vec<u8>, Error> {
    let value = |byte: u8| -> Result<u32, Error> {
        match byte {
            b'A'..=b'Z' => Ok(u32::from(byte - b'A')),
            b'a'..=b'z' => Ok(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Ok(u32::from(byte - b'0') + 52),
            b'+' => Ok(62),
            b'/' => Ok(63),
            _ => Err(Error::config("invalid base64")),
        }
    };
    let symbols: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    let trimmed = symbols
        .strip_suffix(b"==")
        .or_else(|| symbols.strip_suffix(b"="))
        .unwrap_or(&symbols);
    if !symbols.len().is_multiple_of(4) || trimmed.contains(&b'=') {
        return Err(Error::config("invalid base64 length or padding"));
    }
    let mut bytes = Vec::with_capacity(trimmed.len() * 3 / 4);
    for group in trimmed.chunks(4) {
        let mut accumulator = 0u32;
        for byte in group {
            accumulator = (accumulator << 6) | value(*byte)?;
        }
        match group.len() {
            4 => bytes.extend_from_slice(&accumulator.to_be_bytes()[1..]),
            3 => bytes.extend_from_slice(&(accumulator << 6).to_be_bytes()[1..3]),
            2 => bytes.push((accumulator >> 4) as u8),
            _ => return Err(Error::config("invalid base64 length")),
        }
    }
    Ok(bytes)
}

struct Line<'a> {
    indent: usize,
    text: &'a str,
}

pub fn parse_yaml(text: &str) -> Result<Value, Error> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let content = raw.trim_end();
        let body = content.trim_start_matches(' ');
        if body.is_empty() || body.starts_with('#') || body == "---" {
            continue;
        }
        if body.starts_with('\t') {
            return Err(Error::config("YAML tab indentation is not supported"));
        }
        lines.push(Line {
            indent: content.len() - body.len(),
            text: body,
        });
    }
    if lines.is_empty() {
        return Err(Error::config("YAML is empty"));
    }
    let mut index = 0;
    let indent = lines[0].indent;
    let value = block(&mut lines, &mut index, indent)?;
    if index != lines.len() {
        return Err(Error::config("YAML has content outside its root block"));
    }
    Ok(value)
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

fn block(lines: &mut [Line<'_>], index: &mut usize, indent: usize) -> Result<Value, Error> {
    if is_item(lines[*index].text) {
        let mut items = Vec::new();
        while *index < lines.len() && lines[*index].indent == indent && is_item(lines[*index].text)
        {
            let rest = lines[*index].text[1..].trim_start_matches(' ');
            if rest.is_empty() {
                *index += 1;
                if *index < lines.len() && lines[*index].indent > indent {
                    let inner = lines[*index].indent;
                    items.push(block(lines, index, inner)?);
                } else {
                    items.push(Value::Null);
                }
            } else if mapping_key(rest).is_some() {
                let offset = lines[*index].text.len() - rest.len();
                lines[*index] = Line {
                    indent: indent + offset,
                    text: rest,
                };
                items.push(block(lines, index, indent + offset)?);
            } else {
                items.push(scalar(rest)?);
                *index += 1;
            }
        }
        return Ok(Value::Array(items));
    }
    let mut map = Map::new();
    while *index < lines.len() && lines[*index].indent == indent && !is_item(lines[*index].text) {
        let line = lines[*index].text;
        let (key, rest) = mapping_key(line)
            .ok_or_else(|| Error::config(format!("YAML line is not a mapping entry: {line}")))?;
        *index += 1;
        let value = if !rest.is_empty() {
            scalar(rest)?
        } else if *index < lines.len()
            && (lines[*index].indent > indent
                || (lines[*index].indent == indent && is_item(lines[*index].text)))
        {
            let inner = lines[*index].indent;
            block(lines, index, inner)?
        } else {
            Value::Null
        };
        if map.insert(key, value).is_some() {
            return Err(Error::config("YAML mapping repeats a key"));
        }
    }
    if *index < lines.len() && lines[*index].indent > indent {
        return Err(Error::config("YAML indentation is inconsistent"));
    }
    Ok(Value::Object(map))
}

fn mapping_key(line: &str) -> Option<(String, &str)> {
    if let Some(quoted) = line.strip_prefix('"') {
        let end = quoted.find('"')?;
        let rest = quoted[end + 1..].strip_prefix(':')?;
        return Some((quoted[..end].to_owned(), rest.trim()));
    }
    let end = line
        .find(": ")
        .or_else(|| line.ends_with(':').then(|| line.len() - 1))?;
    Some((line[..end].to_owned(), line[end + 1..].trim()))
}

fn scalar(text: &str) -> Result<Value, Error> {
    match text {
        "{}" => return Ok(Value::Object(Map::new())),
        "[]" => return Ok(Value::Array(Vec::new())),
        "null" | "~" => return Ok(Value::Null),
        "true" => return Ok(Value::Bool(true)),
        "false" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if let Some(inner) = text.strip_prefix('"') {
        let inner = inner
            .strip_suffix('"')
            .ok_or_else(|| Error::config("unterminated double-quoted YAML scalar"))?;
        return serde_json::from_str::<String>(&format!("\"{inner}\""))
            .map(Value::String)
            .map_err(|_| Error::config("unsupported escape in a double-quoted YAML scalar"));
    }
    if let Some(inner) = text.strip_prefix('\'') {
        let inner = inner
            .strip_suffix('\'')
            .ok_or_else(|| Error::config("unterminated single-quoted YAML scalar"))?;
        return Ok(Value::String(inner.replace("''", "'")));
    }
    if text.starts_with(['|', '>', '{', '[', '&', '*', '!']) {
        return Err(Error::config(format!("unsupported YAML scalar: {text}")));
    }
    let plain = text.split(" #").next().unwrap_or(text).trim_end();
    Ok(Value::String(plain.to_owned()))
}
