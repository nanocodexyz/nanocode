use super::{process, selection};
use nanocode_io::{AsyncRead, AsyncReadExt};
use std::{
    ffi::OsString,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

pub use nanocode_process_contracts::{ForegroundOutput, OutputSink};

pub(crate) const MARK_VARIABLE: &str = "NANOCODE_COMMAND_ID";
const SHELL_JOB: &str = "psh, running part of the command";
const SURVIVORS_NOTE: &str = "The command exited, and these processes it started are still running in the background. Nothing reports when they end:";

static COMMANDS: AtomicU64 = AtomicU64::new(0);

pub fn run_bash<'call>(
    workspace: &'call Path,
    command: &'call str,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
    session: Option<&'call crate::SessionEnvironment>,
    output: Option<&'call dyn OutputSink>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ForegroundOutput, String>> + Send + 'call>,
> {
    run_bash_with_environment(
        workspace,
        command,
        timeout,
        max_stdout,
        max_stderr,
        session,
        &[],
        output,
    )
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn run_bash_with_environment<'call>(
    workspace: &'call Path,
    command: &'call str,
    timeout: Duration,
    max_stdout: usize,
    max_stderr: usize,
    session: Option<&'call crate::SessionEnvironment>,
    environment: &'call [(OsString, OsString)],
    output: Option<&'call dyn OutputSink>,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<ForegroundOutput, String>> + Send + 'call>,
> {
    Box::pin(async move {
        let mark = format!(
            "{}-{}",
            std::process::id(),
            COMMANDS.fetch_add(1, Ordering::Relaxed)
        );
        let overrides: Vec<_> = environment
            .iter()
            .cloned()
            .chain(
                session
                    .map(crate::SessionEnvironment::os_variables)
                    .unwrap_or_default(),
            )
            .chain([(OsString::from(MARK_VARIABLE), OsString::from(&mark))])
            .collect();
        let (mut environment, secrets) = process::sanitized_environment(&overrides, &[]);
        let bash = OsString::from(selection::default_shell_path());
        if !environment.iter().any(|(name, _)| name == "SHELL") {
            environment.push((OsString::from("SHELL"), bash.clone()));
        }
        environment.push((OsString::from("PSH_NAME"), bash));
        let shell = selection::chosen().ok_or("this host has not made psh its shell")?;
        #[cfg(unix)]
        let warm = match super::embedded::spawn(
            command,
            workspace,
            &environment,
            super::embedded::Streams::Apart,
        ) {
            Some(spawned) => Some(spawned),
            None => super::warm::spawn(command, workspace, &shell, false, &environment).await,
        };
        #[cfg(not(unix))]
        let warm = None;
        let mut spawned = match warm {
            Some(spawned) => spawned,
            None => process::spawn(command, workspace, &shell, false, false, &environment)
                .map_err(|error| error.to_string())?,
        };
        let group = spawned.child.group();
        struct Cleanup(process::SharedProcessGroup);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.0.terminate_and_disarm();
            }
        }
        let cleanup = Cleanup(spawned.process_group);
        let outcome = nanocode_platform::timeout(timeout, async {
            match spawned.output {
                process::ProcessOutput::Pipes { stdout, stderr } => {
                    let captures = (
                        Capture::new(stdout, max_stdout),
                        Capture::new(stderr, max_stderr),
                    );
                    collect(&mut spawned.child, captures, output, &secrets).await
                }
                #[cfg(unix)]
                process::ProcessOutput::Streams { stdout, stderr } => {
                    let captures = (
                        Capture::new(Some(stdout), max_stdout),
                        Capture::new(Some(stderr), max_stderr),
                    );
                    collect(&mut spawned.child, captures, output, &secrets).await
                }
                _ => Err(std::io::Error::other("foreground execution requires pipes")),
            }
        })
        .await;
        drop(cleanup);
        match outcome {
            Ok(Ok(mut output)) => {
                let found = nanocode_platform::spawn_blocking(move || {
                    nanocode_executor::process::survivors(group, (MARK_VARIABLE, &mark))
                })
                .await;
                match found
                    .map_err(|error| error.to_string())
                    .and_then(|found| found.map_err(|error| error.to_string()))
                {
                    Ok(survivors) if survivors.is_empty() => {}
                    Ok(survivors) => noted(&mut output, &survivors, (shell.path(), max_stderr)),
                    Err(error) => {
                        nanocode_trace::debug!(%error, "the processes a command left running were not listed");
                    }
                }
                Ok(output)
            }
            Ok(Err(error)) => Err(error.to_string()),
            Err(_) => {
                let _ = spawned.child.wait().await;
                Err("Bash command timed out".into())
            }
        }
    })
}

fn noted(
    output: &mut ForegroundOutput,
    survivors: &[(u32, String)],
    (shell, limit): (&Path, usize),
) {
    let shell = shell.to_string_lossy();
    let mut note = SURVIVORS_NOTE.to_owned();
    let tail = format!("\nand {} more", survivors.len()).len();
    let mut listed = 0;
    for (id, command) in survivors {
        let command = match command.strip_prefix(shell.as_ref()) {
            Some(rest) if rest.is_empty() || rest.starts_with(' ') => SHELL_JOB,
            _ => command,
        };
        let reserved = if listed + 1 == survivors.len() {
            0
        } else {
            tail
        };
        let Some(line) = [format!("\nPID {id} ({command})"), format!("\nPID {id}")]
            .into_iter()
            .find(|line| note.len() + line.len() + reserved <= limit)
        else {
            break;
        };
        note.push_str(&line);
        listed += 1;
    }
    if listed < survivors.len() {
        note.push_str(&format!("\nand {} more", survivors.len() - listed));
    }
    let room = limit.saturating_sub(note.len() + 1);
    if output.stderr.len() > room {
        output
            .stderr
            .truncate(output.stderr.floor_char_boundary(room));
        output.truncated = true;
    }
    if !output.stderr.is_empty() && !output.stderr.ends_with('\n') {
        output.stderr.push('\n');
    }
    output.stderr.push_str(&note);
}

async fn collect<O: Pipe, E: Pipe>(
    child: &mut process::ProcessChild,
    (mut stdout, mut stderr): (Capture<O>, Capture<E>),
    output: Option<&dyn OutputSink>,
    secrets: &nanocode_process_contracts::Redactor,
) -> std::io::Result<ForegroundOutput> {
    let exit_code = {
        let read = async {
            nanocode_sync::try_join!(stdout.read(output, secrets), stderr.read(output, secrets))
        };
        let mut read = std::pin::pin!(read);
        nanocode_sync::select! {
            status = child.wait() => status?,
            ended = &mut read => {
                ended?;
                child.wait().await?
            }
        }
    };
    stdout.rest(output, secrets)?;
    stderr.rest(output, secrets)?;
    Ok(ForegroundOutput {
        truncated: stdout.truncated || stderr.truncated,
        stdout: stdout.text(secrets),
        stderr: stderr.text(secrets),
        exit_code,
    })
}

trait Pipe: AsyncRead + process::Ready + Unpin + Send + 'static {}

impl<P: AsyncRead + process::Ready + Unpin + Send + 'static> Pipe for P {}

struct Capture<P> {
    pipe: Option<P>,
    bytes: Vec<u8>,
    limit: usize,
    truncated: bool,
    _open: crate::output::Open,
}

impl<P: Pipe> Capture<P> {
    fn new(pipe: Option<P>, limit: usize) -> Self {
        Self {
            _open: pipe
                .as_ref()
                .map_or_else(crate::output::Open::none, process::Ready::open),
            pipe,
            bytes: Vec::new(),
            limit,
            truncated: false,
        }
    }

    fn keep(
        &mut self,
        chunk: &[u8],
        output: Option<&dyn OutputSink>,
        secrets: &nanocode_process_contracts::Redactor,
    ) {
        if let Some(output) = output {
            output.write(&crate::redact(chunk, secrets));
        }
        let keep = chunk.len().min(self.limit.saturating_sub(self.bytes.len()));
        self.bytes.extend_from_slice(&chunk[..keep]);
        self.truncated |= keep < chunk.len();
    }

    async fn read(
        &mut self,
        output: Option<&dyn OutputSink>,
        secrets: &nanocode_process_contracts::Redactor,
    ) -> std::io::Result<()> {
        let mut buffer = vec![0_u8; crate::output::READ_BUFFER_LENGTH];
        while let Some(pipe) = self.pipe.as_mut() {
            match pipe.read(&mut buffer).await? {
                0 => self.pipe = None,
                read => self.keep(&buffer[..read], output, secrets),
            }
        }
        Ok(())
    }

    fn rest(
        &mut self,
        output: Option<&dyn OutputSink>,
        secrets: &nanocode_process_contracts::Redactor,
    ) -> std::io::Result<()> {
        let mut buffer = vec![0_u8; crate::output::READ_BUFFER_LENGTH];
        while let Some(mut pipe) = self.pipe.take() {
            match pipe.ready(&mut buffer)? {
                None => {
                    let open = pipe.open();
                    drop(nanocode_platform::spawn_task(crate::output::discard(
                        pipe, open,
                    )));
                }
                Some(0) => {}
                Some(read) => {
                    self.keep(&buffer[..read], output, secrets);
                    self.pipe = Some(pipe);
                }
            }
        }
        Ok(())
    }

    fn text(&self, secrets: &nanocode_process_contracts::Redactor) -> String {
        String::from_utf8_lossy(&crate::redact(&self.bytes, secrets)).into_owned()
    }
}
