use std::collections::VecDeque;

use nanocode_agent_core::hedge::HedgeOutcome;
use nanocode_platform::Instant;

use crate::client::{
    AgentEventKind, EncodedRequest, OpenAiAuthMode, ResponsesError, ResponsesTransport,
    http::{HttpMetadata, ResponsesHttpStream},
    socket::ReceivedText,
    telemetry::{ApiEvent, ApiEventData, elapsed_ns},
    tower::{
        ResponsesAttempt, ResponsesAttemptKind, ResponsesOutput, ResponsesServiceError,
        ResponsesServiceResponse,
        service_error::FailurePhase,
        stream::{self, ResponseEventSource},
    },
    transport::error::Credentials,
};

use super::super::{AttemptGuard, ResponsesService, record_pipeline_stats, required_call_index};

pub(crate) async fn run(
    service: &ResponsesService,
    connection: &mut AttemptGuard<'_>,
    request: &ResponsesAttempt,
    started_at: Instant,
) -> Result<ResponsesServiceResponse, ResponsesServiceError> {
    if matches!(
        request.kind,
        ResponsesAttemptKind::Warmup | ResponsesAttemptKind::Connect
    ) {
        return Err(ResponsesServiceError::invalid_attempt_state(
            "HTTPS Responses transport does not perform a warmup or connect request",
            FailurePhase::Protocol,
            0,
        ));
    }
    let encode_started_at = Instant::now();
    let encoded = service.encode_request(
        connection.turn_state.as_deref(),
        connection.generation,
        request,
        ResponsesTransport::Https,
    )?;
    let encode_duration_ns = elapsed_ns(encode_started_at);
    let request_bytes = encoded.json().len();
    let transport = ResponsesTransport::Https.as_str();
    let span = nanocode_trace::Span::current();
    span.record("request.bytes", request_bytes);
    span.record("request.encode.duration_ns", encode_duration_ns);
    nanocode_trace::trace!(
        target: "nanocode_codex",
        direction = "outbound",
        transport,
        phase = request.kind.phase(),
        model.call_index = request.call_index,
        api.request = %encoded.json(),
        "OpenAI Responses API request"
    );
    request.trace_encoded_input(encoded.json());
    request.observer.emit(
        AgentEventKind::ApiEvent,
        ApiEvent {
            direction: "outbound",
            transport,
            phase: request.kind.phase(),
            model_call_index: request.call_index,
            event: ApiEventData::Encoded(encoded.json()),
        },
    )?;
    let send_started_at = Instant::now();
    let turn_state = connection.turn_state.clone();
    let (hedge, opened) = match (request.kind, service.config.hedging.as_deref()) {
        (ResponsesAttemptKind::Generation, Some(hedging)) => {
            let mut hedge_credentials = Credentials::new();
            let hedged = hedging
                .race(
                    request.model().as_str(),
                    open(
                        service,
                        request,
                        &encoded,
                        &mut connection.credentials,
                        turn_state.as_deref(),
                        true,
                    ),
                    || {
                        open(
                            service,
                            request,
                            &encoded,
                            &mut hedge_credentials,
                            turn_state.as_deref(),
                            true,
                        )
                    },
                )
                .await;
            connection.credentials.extend(&hedge_credentials);
            (hedged.outcome, hedged.result?)
        }
        _ => (
            HedgeOutcome::Unneeded,
            open(
                service,
                request,
                &encoded,
                &mut connection.credentials,
                turn_state.as_deref(),
                false,
            )
            .await?,
        ),
    };
    let Opened {
        buffered,
        metadata,
        response_headers_ns,
        mut response,
    } = opened;
    connection.observe_turn_state(metadata.turn_state.as_deref());
    let send_duration_ns = elapsed_ns(send_started_at);
    span.record("request.send.duration_ns", send_duration_ns);
    let output = match request.kind {
        ResponsesAttemptKind::Generation => {
            let mut output = stream::receive(
                &mut stream::Primed::new(buffered, &mut response),
                request.freeform_calls(service.config.responses_gateway.as_deref()),
                transport,
                &request.observer,
                &connection.credentials,
                required_call_index(request)?,
                started_at,
                request.model_call_started_at,
            )
            .await?;
            output.timing.encode_ns = Some(encode_duration_ns);
            output.timing.send_ns = Some(send_duration_ns);
            output.timing.response_headers_ns = response_headers_ns;
            output.hedge = hedge;
            ResponsesOutput::Generation(output)
        }
        ResponsesAttemptKind::Compaction => ResponsesOutput::Compaction(
            stream::receive_compaction(
                &mut stream::Primed::new(buffered, &mut response),
                request.is_local_compaction(),
                transport,
                &request.observer,
                &connection.credentials,
                required_call_index(request)?,
                started_at,
            )
            .await?,
        ),
        ResponsesAttemptKind::Warmup | ResponsesAttemptKind::Connect => {
            unreachable!("warmup and connect rejected above")
        }
    };
    let pipeline_stats = match &output {
        ResponsesOutput::Generation(result) => result.pipeline_stats,
        ResponsesOutput::Compaction(result) => result.pipeline_stats,
        ResponsesOutput::Warmup(_) | ResponsesOutput::Connected => {
            unreachable!("warmup and connect rejected above")
        }
    };
    record_pipeline_stats(
        &span,
        request_bytes,
        encode_duration_ns,
        send_duration_ns,
        pipeline_stats,
    );
    Ok(ResponsesServiceResponse {
        output,
        attempt: request.attempt,
        connection_generation: 0,
        server_reasoning_included: metadata.reasoning_included,
    })
}

impl ResponseEventSource for ResponsesHttpStream {
    async fn next_text(&mut self) -> Result<crate::client::socket::ReceivedText, ResponsesError> {
        Self::next_text(self).await
    }

    fn try_next_text(
        &mut self,
    ) -> Result<Option<crate::client::socket::ReceivedText>, ResponsesError> {
        Self::try_next_text(self)
    }
}

struct Opened {
    buffered: VecDeque<ReceivedText>,
    metadata: HttpMetadata,
    response_headers_ns: Option<u64>,
    response: ResponsesHttpStream,
}

async fn open(
    service: &ResponsesService,
    request: &ResponsesAttempt,
    encoded: &EncodedRequest,
    credentials: &mut Credentials,
    turn_state: Option<&str>,
    primed: bool,
) -> Result<Opened, ResponsesServiceError> {
    let (mut response, metadata) = send_with_auth_recovery(
        service,
        request.profile.session_id(),
        request.profile.thread_id(),
        encoded,
        credentials,
        turn_state,
    )
    .await
    .map_err(|error| ResponsesServiceError::responses(error, FailurePhase::Send, 0))?;
    let response_headers_ns = request.model_call_started_at.map(elapsed_ns);
    let buffered = if primed {
        stream::first_output(&mut response)
            .await
            .map_err(|error| ResponsesServiceError::responses(error, FailurePhase::Receive, 0))?
    } else {
        VecDeque::new()
    };
    Ok(Opened {
        buffered,
        metadata,
        response_headers_ns,
        response,
    })
}

async fn send_with_auth_recovery(
    service: &ResponsesService,
    session_id: &str,
    thread_id: &str,
    request: &EncodedRequest,
    credentials: &mut Credentials,
    turn_state: Option<&str>,
) -> Result<(ResponsesHttpStream, HttpMetadata), ResponsesError> {
    let auth = service.auth_snapshot().await?;
    credentials.insert(auth.shared_bearer());
    match service
        .platform
        .http()
        .send(
            &service.config.api_base_url,
            &auth,
            session_id,
            thread_id,
            turn_state,
            request,
        )
        .await
    {
        Err(ResponsesError::HttpRejected { status: 401, .. })
            if auth.mode() == OpenAiAuthMode::ChatGpt =>
        {
            service
                .config
                .auth
                .recover_unauthorized(&auth)
                .await
                .map_err(|error| ResponsesError::Authorization {
                    detail: error.to_string(),
                })?;
            let refreshed = service.auth_snapshot().await?;
            credentials.insert(refreshed.shared_bearer());
            service
                .platform
                .http()
                .send(
                    &service.config.api_base_url,
                    &refreshed,
                    session_id,
                    thread_id,
                    turn_state,
                    request,
                )
                .await
        }
        result => result,
    }
}
