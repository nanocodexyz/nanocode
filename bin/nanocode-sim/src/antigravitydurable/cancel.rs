use super::*;

const CANCELLED: &str = "antigravity-cancelled-turn";
const NEXT: &str = "antigravity-next-turn";

#[derive(Default)]
struct CancelWire {
    calls: AtomicU64,
    holding: Signal,
}

struct CancelSender(Arc<CancelWire>);

impl nanocode_http::layer::Sender for CancelSender {
    fn send(
        &self,
        _request: nanocode_http::layer::Request,
    ) -> nanocode_http::layer::BoxFuture<
        '_,
        Result<nanocode_http::layer::Response, nanocode_http::layer::Error>,
    > {
        Box::pin(async move {
            let parts = match self.0.calls.fetch_add(1, Ordering::AcqRel) {
                0 => vec![json!({"functionCall": {"name": HOLD, "args": {}}})],
                _ => vec![json!({"text": DONE})],
            };
            Ok(model_response(parts, true))
        })
    }
}

struct Hang(Arc<CancelWire>);

impl nanocode::FunctionHandler for Hang {
    fn call<'a>(
        &'a self,
        _name: &'a str,
        _input: Value,
        _context: nanocode::tools::ToolContext<'a>,
    ) -> nanocode::HostFuture<'a, Result<Value, String>> {
        Box::pin(async move {
            self.0.holding.fire();
            loop {
                nanocode_platform::sleep(POLL).await;
            }
        })
    }
}

async fn run(wire: Arc<CancelWire>, media: &Media, store: Store) -> Result<(), String> {
    let (session, parts) = media.open(store).await?;
    let builder = Tools::builder()
        .without_defaults()
        .tool(nanocode::FunctionTool::new(
            HOLD,
            "Run a simulated step",
            json!({"type": "object", "properties": {}}),
            Arc::new(Hang(Arc::clone(&wire))),
        ))
        .exposure(nanocode_tool_provider::ToolExposure::DirectOnly);
    let tools = match &parts {
        Some(parts) => builder.effect_gate(Arc::clone(&parts.gate)),
        None => builder,
    }
    .build()
    .map_err(|error| error.to_string())?;
    let runtime = runtime(Arc::new(CancelSender(Arc::clone(&wire))), session, tools)?;
    let (agent, events) = runtime
        .spawn()
        .session_id(SESSION)
        .build()
        .await
        .map_err(|error| format!("open: {error}"))?;
    drop(events);
    let turn = agent
        .prompt(PromptRequest::new(PROMPT).request_id(CANCELLED))
        .await
        .map_err(|error| format!("admit: {error}"))?;
    wire.holding.wait().await;
    if let Some(parts) = &parts {
        let settled = parts
            .log
            .settle_intents(CANCELLED)
            .await
            .map_err(|error| format!("settle intents: {error}"))?;
        if settled.is_empty() {
            return Err("the running tool call left no intent to settle".to_owned());
        }
    }
    turn.cancel()
        .await
        .map_err(|error| format!("cancel: {error}"))?;
    match turn.result().await {
        Err(nanocode::NanocodeError::TurnCancelled) => {}
        Err(error) => {
            return Err(format!(
                "antigravity_cancel_mid_tool: the turn ended with {error:?}, not cancelled"
            ));
        }
        Ok(done) => {
            return Err(format!(
                "antigravity_cancel_mid_tool: the cancelled turn finished with {:?}",
                done.final_message
            ));
        }
    }
    let next = agent
        .prompt(PromptRequest::new(PROMPT).request_id(NEXT))
        .await
        .map_err(|error| format!("antigravity_cancel_mid_tool: the next turn: {error}"))?
        .result()
        .await
        .map_err(|error| format!("antigravity_cancel_mid_tool: the next turn: {error}"))?;
    if next.final_message != DONE {
        return Err(format!(
            "antigravity_cancel_mid_tool: the next turn ended with {:?}",
            next.final_message
        ));
    }
    Ok(())
}

pub(super) fn simulate(seed: u64) -> (u64, Vec<String>) {
    let mut calls = 0;
    let mut wrong = Vec::new();
    for store in [Store::Kv, Store::Log] {
        let clock = Clock::new();
        crate::host::enter(seed, &clock);
        let media = media(seed, &clock);
        let wire = Arc::new(CancelWire::default());
        let (ended, result) = epoch(
            seed,
            &clock,
            &format!("antigravity cancel {store:?}"),
            run(Arc::clone(&wire), &media, store),
        );
        match (ended, result) {
            (Ended::Finished, Some(Ok(()))) => {}
            (Ended::Finished, Some(Err(error))) => wrong.push(format!("{store:?}: {error}")),
            (ended, _) => wrong.push(format!(
                "{store:?}: the cancel ended {ended:?} without a result"
            )),
        }
        calls += wire.calls.load(Ordering::Acquire);
    }
    (calls, wrong)
}
