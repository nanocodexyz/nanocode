# nanocode-backend-antigravity

A nanocode harness that reproduces the engine of Google Antigravity CLI (`agy` 1.5.13): its system prompt,
its engine tools, its per-turn reminders, its compaction and its Gemini request shape. Subagents, Code Mode and MCP
stay nanocode's own, as in the other harnesses.

## Quick start

With Vertex AI and your Google account:

```
gcloud auth application-default login && nanocode setup google
nanocode -m gemini-4-neon
```

`nanocode setup google` signs you in with `gcloud auth application-default login` when Application Default Credentials are
missing or expired, finds a Google Cloud project where `gemini-4-neon` answers in `global`, and saves it in
`~/.nanocode/antigravity.json` (per credential) so later runs start without probing. When no project works it lists each one with
why (Vertex AI disabled, no permission, no billing) and offers to enable `aiplatform.googleapis.com` in one you pick, after a y/N
confirmation, then waits until three calls in a row succeed. It never enables anything or changes billing without that confirmation.
`nanocode setup google --check` prints the resolved auth method, project, location and model and probes them with one
`countTokens` call.

Or with a Gemini API key (`gemini-4-neon` needs a paid-tier key):

```
GEMINI_API_KEY=... nanocode -m gemini-4-neon
```

Vertex AI resolves the project in this order: `--google-project`, `GOOGLE_CLOUD_PROJECT`, the project `setup` or an
earlier run saved for these credentials, the credentials' `quota_project_id`, `GOOGLE_CLOUD_QUOTA_PROJECT`, a service account's
`project_id`, the metadata server's project, then the gcloud configuration's project (`CLOUDSDK_CORE_PROJECT`, else `[core] project`
of the active configuration). The first three are used as given. A project found any other way is probed once with `countTokens`
and saved when it answers; when it answers 403 (Vertex AI disabled, no permission, no billing), or when none is found, nanocode
lists your active projects (Cloud Resource Manager), probes each in parallel, uses the first that answers, saves it, and prints
one line on stderr naming it and how to override it.

Errors name the fix: Vertex AI disabled in a project points at `nanocode setup google` and the `gcloud services enable` command (a
just-enabled project answers 403 `SERVICE_DISABLED` on and off for a few minutes, so those are retried after 5 s and 15 s); an expired
sign-in points at `gcloud auth application-default login`; a model missing in a regional location suggests `--google-location global`;
a free-tier API key says the model needs a paid-tier key; 429s are retried with backoff before saying so.

Each model call's usage (`prompt_tokens`, `cache_read_tokens` from `cachedContentTokenCount`, output and thought tokens) goes to the
turn summary like other harnesses' and to `usage.jsonl` beside the transcript in the session's
`brain/<session>/.system_generated/logs/`, so Gemini's implicit caching can be checked: the system instruction, tools and earlier
turns are byte-identical across turns, and per-turn metadata rides in the new user turn.

## Selecting it

```
nanocode --harness antigravity                 # gemini-4-neon, thinking High
nanocode --harness antigravity -m gemini-3-pro
nanocode -m gemini-4-neon                      # a gemini-* model picks this harness
```

Credentials: pick a method with `--google-auth auto|api-key|vertex|vertex-express|gateway|antigravity|google-ai-plan` (or `NANOCODE_GOOGLE_AUTH`).

| Method | Endpoint | Auth | Configured by |
| --- | --- | --- | --- |
| `gateway` | `$AGY_LLM_GATEWAY_URL/v1beta/models/{m}:streamGenerateContent` | `Bearer $AGY_LLM_GATEWAY_API_KEY`, agy's gateway headers | `AGY_LLM_GATEWAY_URL`, `AGY_LLM_GATEWAY_API_KEY`. `claude-*` models on the Gemini wire run only here |
| `api-key` | `generativelanguage.googleapis.com/v1beta/models/{m}:streamGenerateContent` | `x-goog-api-key` | `GEMINI_API_KEY` or `GOOGLE_API_KEY`; `GEMINI_BASE_URL` for another base URL |
| `vertex-express` | `aiplatform.googleapis.com/v1/publishers/google/models/{m}:streamGenerateContent` | `x-goog-api-key` (an express mode key) | `GOOGLE_API_KEY` |
| `vertex` | `{location}-aiplatform.googleapis.com/v1/projects/{p}/locations/{location}/publishers/google/models/{m}:streamGenerateContent` (`aiplatform.googleapis.com` for `global`) | OAuth bearer from Application Default Credentials; `x-goog-user-project` for user credentials | `--google-project`, `--google-location`, the variables below |
| `antigravity` | `daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent` | OAuth bearer from an Antigravity subscription login | `nanocode login google`; `--google-auth antigravity` overrides other configured routes |
| `google-ai-plan` | `generativelanguage.googleapis.com/v1beta/interactions:createPerUserQuota` (Interactions API, stateless) | OAuth bearer from a Google AI plan sign-in | `nanocode login google-ai-plan` with a Google-approved OAuth client in `NANOCODE_GOOGLE_AI_PLAN_CLIENT_ID` and `NANOCODE_GOOGLE_AI_PLAN_CLIENT_SECRET` |

`auto` (the default) takes the first that applies:

1. `AGY_LLM_GATEWAY_URL` is set: `gateway`.
2. `GOOGLE_GENAI_USE_VERTEXAI=true`: `vertex` when a project is given (`--google-project` or `GOOGLE_CLOUD_PROJECT`), else `vertex-express` when `GOOGLE_API_KEY` is set, else `vertex`.
3. `GEMINI_API_KEY` or `GOOGLE_API_KEY`: `api-key`.
4. In the nanocode CLI, a saved Google AI plan sign-in selects `google-ai-plan`, else a saved subscription login selects `antigravity`, unless Google Cloud credentials, a project or a location are explicitly configured.
5. Otherwise `vertex` with Application Default Credentials. When none are found the error lists everything tried.

SDK callers select subscription auth explicitly with `GoogleConfig.method`; the CLI's saved-login selection does not change SDK credential discovery.

Vertex AI (`vertex`) finds credentials in this order, through `nanocode-google-auth`:

1. `GOOGLE_OAUTH_ACCESS_TOKEN`: a ready access token.
2. `GOOGLE_APPLICATION_CREDENTIALS`: a credentials file.
3. gcloud's `application_default_credentials.json` (`$CLOUDSDK_CONFIG`, else `~/.config/gcloud`, `%APPDATA%\gcloud` on Windows), from `gcloud auth application-default login`.
4. The GCE/GKE metadata server (`GCE_METADATA_HOST`, else 169.254.169.254; a 3 s probe; skipped with `NO_GCE_CHECK=true`).
5. `gcloud auth print-access-token`, as a last resort.

Credential files may be `authorized_user` (refresh token grant), `service_account` (an RS256 JWT grant for the `cloud-platform` scope),
`impersonated_service_account` (IAM Credentials `generateAccessToken`), `external_account` with a file or URL subject token
(STS token exchange, then optional service account impersonation) or `external_account_authorized_user`. Tokens are cached
and refreshed five minutes before they expire; concurrent requests share one refresh, and a 401 refreshes once and retries.

| Variable | Use |
| --- | --- |
| `GOOGLE_CLOUD_PROJECT` | The project, after `--google-project`; then the order in Quick start |
| `CLOUDSDK_CORE_PROJECT`, `CLOUDSDK_ACTIVE_CONFIG_NAME` | The gcloud configuration project, as gcloud reads it |
| `CLOUDSDK_API_ENDPOINT_OVERRIDES_CLOUDRESOURCEMANAGER`, `CLOUDSDK_API_ENDPOINT_OVERRIDES_SERVICEUSAGE` | Other base URLs for listing projects and enabling Vertex AI |
| `GOOGLE_CLOUD_LOCATION` | The location, after `--google-location`; `global` otherwise |
| `GOOGLE_CLOUD_QUOTA_PROJECT` | The `x-goog-user-project` billing project when the credentials name none |
| `GOOGLE_VERTEX_BASE_URL` | Another base URL for `vertex` and `vertex-express` |
| `NANOCODE_ANTIGRAVITY_EXPLICIT_CACHE=1` | Turns on explicit context caching (off by default; not available in express or subscription mode) |

Vertex requests carry the same Gemini body as the Gemini API, serialized the same cache-stable way, without agy's
`labels` (their values break Vertex AI's label rules). Claude models on Vertex AI are served by Anthropic's Model Garden
publisher (`streamRawPredict` with the Messages API), which this harness does not speak, so `claude-*` under `vertex`
fails and names the route that works: `--harness claude` with `CLAUDE_CODE_USE_VERTEX=1`, `ANTHROPIC_VERTEX_PROJECT_ID` (else
`GOOGLE_CLOUD_PROJECT`), `CLOUD_ML_REGION` (else `GOOGLE_CLOUD_LOCATION`, then `global`) and optionally `ANTHROPIC_VERTEX_BASE_URL`.
nanocode's Claude client then posts to `publishers/anthropic/models/{m}:streamRawPredict` with `anthropic_version: vertex-2023-10-16`
and a bearer from the same Application Default Credentials. Vertex names some Claude models with a version suffix
(`claude-…@20250929`); pass the name Vertex AI lists with `-m`.

### Antigravity subscription

This is an unofficial integration. [Google's terms](https://antigravity.google/terms) prohibit third-party access through Antigravity OAuth and allow suspension or termination of Antigravity or Gemini CLI accounts. API-key, Vertex AI and gateway auth remain available without this login.

```
nanocode login google
nanocode -m gemini-4-neon
nanocode setup google --google-auth antigravity --model gemini-4-neon --check
```

Run `nanocode login google`, or type `/login google` inside nanocode's interactive terminal UI, to sign in with your Google account's Antigravity subscription. These commands use that subscription when no other Google route is explicitly configured. `nanocode login antigravity` and `/login antigravity` select it even when an API key or Vertex route is configured. `nanocode setup google` and `/login vertex` retain the Google Cloud setup flow.

The subscription login uses the built-in public OAuth client, S256 PKCE and hosted callback from [agy 1.5.13](https://github.com/google-antigravity/antigravity-cli/releases/tag/1.5.13). It requests the same scopes: `cloud-platform`, `userinfo.email`, `userinfo.profile`, `cclog`, `experimentsandconfigs`, `aicode` and `openid` (the first six under `https://www.googleapis.com/auth/`). Finish in the browser at `https://antigravity.google/oauth-callback`. Google displays an authorization code there; paste it into nanocode, as in agy's normal CLI login, rather than waiting for a local callback listener. A full callback URL is also accepted and its state is checked. On a remote terminal, use `nanocode login google --no-browser` to print the sign-in link without opening a browser. No OAuth app, API key or billing project needs to be created.

Credentials are stored separately in `~/.nanocode/antigravity-subscription.json` (or the configured nanocode home), with private file permissions on Unix. Access tokens refresh automatically, and rotated refresh tokens are saved atomically. Later CLI sessions use this saved login unless another Google route is explicitly configured; `--google-auth antigravity` always selects it. A subscription failure never falls back to a billable API-key or Vertex route. Remove the credentials file to stop using the saved login.

Google's subscription eligibility check can return a managed project identifier. Nanocode includes it in subscription requests when present and omits it when absent; it never invents a project or asks you to configure a Vertex billing project. This route supports Gemini models; Claude subscription models are not supported. Model availability and quota depend on the account; replace the example model with an exact name it serves. SDK callers can inspect the account's catalog with `Client::available_models()`. Explicit context caching is unavailable on this route.

Embedders configure the same with `AntigravityHarness::new().google(GoogleConfig { .. }).endpoint(..).retry(..).explicit_cache(..)` and
`RuntimeBuilder::harness`.

## Crates

| Crate | Owns |
| --- | --- |
| `nanocode-backend-antigravity-client` | The Gemini wire format and its byte-exact encoder, SSE streaming, retries, explicit context caching, the Gemini API, Vertex AI, gateway and subscription routes |
| `nanocode-google-auth` | Google Cloud access tokens from Application Default Credentials (in `crates/core`, shared with the Claude on Vertex route) |
| `nanocode-backend-antigravity-tools` | The engine tools with agy's declarations, validation errors and output formats, background tasks and timers |
| `nanocode-backend-antigravity` | The driver: pinned prompts, request assembly, reminders, messages, compaction, model profiles |

## What it reproduces

- **Prompt.** `prompts/system.md` is agy's system instruction with `<messaging>` and `<subagents>`
  rewritten for `spawn_agent`, `send_agent_message`, `wait_agent`; the user information block takes the workspace,
  OS, app data directory and conversation ID, and `<skills>` lists nanocode's skills the way agy lists its own.
- **First user turn.** `<USER_REQUEST>`, `<ADDITIONAL_METADATA>` with the local time, and on the first turn
  `<USER_SETTINGS_CHANGE>` naming the model.
- **Tools.** `read_file`, `run_command`, `manage_task`, `schedule`, `write_file`, `edit_file`, `search_web`, `ask_question`
  with agy's declarations byte for byte, then nanocode's tools (`exec`/`wait`, subagent tools, MCP) as Gemini
  declarations. Results carry `Created At`/`Completed At`; outputs over 10,240 bytes are saved under
  `brain/<conversation>/.system_generated/steps/<step>/output.txt` and shown head and tail.
  `run_command` backgrounds itself after `WaitMsBeforeAsync`; background results, cancellations and timers come
  back as `<SYSTEM_MESSAGE>` messages, appended to the last tool result or sent as their own user turn when idle.
- **Reminders.** The efficiency reminder after two rounds in a row of at most one effective call, as agy counts them.
- **Requests.** `streamGenerateContent?alt=sse`, agy's labels, headers and generation config per model, its JSON
  spacing, sorted argument keys, echoed thought signatures and its placeholder signature on unsigned calls. The
  system instruction and tools never change during a session, so Gemini's implicit cache keeps the prefix.
- **Compaction.** At a reported prompt size of 512,000 tokens (256,000 for gemini-3 and claude), the same prefix plus
  agy's summary prompt with 32,768 output tokens, then a `<CONTEXT_SUMMARY>` turn with the last ten requests and the
  running tasks. The full transcript stays in `.system_generated/logs/transcript*.jsonl`. `/compact` runs it on demand.

`scripts/antigravity-request-parity.py` replays one model script against `agy` and nanocode through a mock gateway
and diffs their requests.

## Not ported

- agy's subagent, messaging, workflow and swarm tools and its MCP handling: nanocode's own replace them.
- The safety rater that agy runs before commands and writes: nanocode's approval policy decides instead.

## Google AI plan

`google-ai-plan` runs Gemini on a Google AI plan's own quota through the Interactions API (`POST /v1beta/interactions:createPerUserQuota`), the sanctioned way for third-party apps. Google approves each app's OAuth client separately and does not let apps share one, so nanocode ships none: register a Desktop app client in your Google Cloud project, have Google approve it for the plan, and set `NANOCODE_GOOGLE_AI_PLAN_CLIENT_ID` and `NANOCODE_GOOGLE_AI_PLAN_CLIENT_SECRET` before signing in. SDK callers pass their own `PlanClient` to `PlanLogin`.

`nanocode login google-ai-plan` (or `/login google-ai-plan`) signs in with a loopback redirect on `http://127.0.0.1:PORT/oauth2callback`, PKCE and a DPoP key (RFC 9449) made on this machine: Google binds the refresh token to that P-256 key and every token request carries a proof signed with it, retried once with Google's `DPoP-Nonce` and once with its clock for skew. If the browser runs on another machine, paste the URL it ends on with `/login google-ai-plan URL` or `--no-browser`. The sign-in lives in `~/.nanocode/google-ai-plan.json` with its client and key, apart from the Antigravity subscription and Google Cloud credentials. Model calls send the access token as a plain bearer with `User-Agent: nanocode/<version>`.

Each request is stateless (`store: false`): the conversation goes as typed steps (`user_input`, `thought`, `model_output`, `function_call`, `function_result`), thought signatures ride their own `thought` steps, and a call with none opens with Google's `skip_thought_signature_validator`. The stream's steps come back as the same parts the other routes give. A daily limit fails at once rather than retrying; `googleSearch` and explicit caches are refused on this route.
