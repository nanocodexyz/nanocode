use crate::client::{Client, Limits};
use crate::error::Error;
use crate::kubeconfig::{Auth, Credentials};
use crate::transport::{Chunks, Request, Response, Transport, Verb};

#[derive(Clone, Debug)]
pub struct HttpTransport {
    http: nanocode_http::Client,
    server: String,
    token: Option<String>,
}

pub struct HttpBody(nanocode_http::Response);

fn transport_error(error: &nanocode_http::Error) -> Error {
    Error::Transport {
        detail: nanocode_http::error_detail(error),
        retryable: nanocode_http::retryable(error),
    }
}

impl HttpTransport {
    pub fn new(credentials: &Credentials) -> Result<Self, Error> {
        if !credentials.server.starts_with("https://") {
            return Err(Error::config("the API server must be https"));
        }
        let mut builder = nanocode_http::ClientBuilder::new()
            .user_agent("nanocode-kube")
            .no_proxy()
            .tls_built_in_root_certs(false)
            .add_root_certificates_pem(&credentials.ca_pem);
        let token = match &credentials.auth {
            Auth::Token(token) => Some(token.clone()),
            Auth::Certificate {
                certificate_pem,
                key_pem,
            } => {
                builder = builder.identity_pem(certificate_pem, key_pem);
                None
            }
        };
        let http = builder
            .build()
            .map_err(|error| Error::config(nanocode_http::error_detail(&error)))?;
        Ok(Self {
            http,
            server: credentials.server.clone(),
            token,
        })
    }
}

impl Chunks for HttpBody {
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, Error> {
        self.0
            .chunk()
            .await
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .map_err(|error| transport_error(&error))
    }
}

impl Transport for HttpTransport {
    type Body = HttpBody;

    async fn send(&self, request: Request) -> Result<Response<HttpBody>, Error> {
        let method = match request.verb {
            Verb::Get => nanocode_http::Method::GET,
            Verb::Patch => nanocode_http::Method::PATCH,
        };
        let url = format!("{}{}", self.server, request.target());
        let mut builder = self
            .http
            .request(method, url.as_str())
            .header("accept", "application/json");
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        if let Some(content_type) = request.content_type {
            builder = builder.header("content-type", content_type);
        }
        if request.verb == Verb::Patch {
            builder = builder.body(request.body);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| transport_error(&error))?;
        Ok(Response {
            status: response.status().as_u16(),
            body: HttpBody(response),
        })
    }
}

impl Client<HttpTransport> {
    pub fn in_cluster(limits: Limits) -> Result<Self, Error> {
        Ok(Self::new(
            HttpTransport::new(&Credentials::in_cluster()?)?,
            limits,
        ))
    }

    pub fn from_kubeconfig(
        path: &std::path::Path,
        context: Option<&str>,
        limits: Limits,
    ) -> Result<Self, Error> {
        let credentials = Credentials::from_kubeconfig_file(path, context)?;
        Ok(Self::new(HttpTransport::new(&credentials)?, limits))
    }
}
