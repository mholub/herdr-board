//! Managed protocol-17 launch: `agent.start` on a board-owned pane, with the
//! name/busy retry taxonomy and the wait for the agent to become interactive.

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context};
use board_herdr::{
    AgentInfo, AgentPromptParams, AgentStartParams, AgentStarted, HerdrClient, HerdrError,
};

use super::super::placement::{RetryablePlacementRace, ERR_PANE_NOT_FOUND};
use super::super::{
    HerdrLaunchPlan, AGENT_START_BUSY_BACKOFF, AGENT_START_BUSY_RETRIES, AGENT_START_TIMEOUT_MS,
    IMMEDIATE_READINESS_PROBES, READINESS_BACKOFF, READINESS_TIMEOUT,
};

const ERR_AGENT_NAME_TAKEN: &str = "agent_name_taken";
const ERR_AGENT_PANE_BUSY: &str = "agent_pane_busy";

pub(crate) type DelayFn = dyn Fn(Duration) + Send + Sync;

/// The production `agent.start` busy-retry delay, for launch callers outside
/// `HerdrSpawner` (the run-pane rescue) that have no injected clock of their own.
pub(crate) const DEFAULT_AGENT_START_DELAY: &DelayFn = &(thread::sleep as fn(Duration));

pub(crate) fn launch_managed(
    client: &mut HerdrClient,
    req: &HerdrLaunchPlan,
    kind: &str,
    pane_id: &str,
    delay: &DelayFn,
) -> anyhow::Result<Option<String>> {
    let prompt_transport = match kind {
        "pi" => Some("--append-system-prompt"),
        "claude" => Some("--append-system-prompt-file"),
        "codex" => None,
        other => bail!("unsupported managed harness kind: {other}"),
    };
    let system_prompt = req
        .system_prompt
        .as_deref()
        .ok_or_else(|| anyhow!("managed {kind} invocation is missing system_prompt metadata"))?;
    let (_, startup_tail) = req
        .argv
        .split_first()
        .ok_or_else(|| anyhow!("managed {kind} invocation has empty startup argv"))?;

    let mut prompt_file = tempfile::Builder::new()
        .prefix("herdr-board-system-")
        .tempfile()
        .context("creating managed system-prompt file")?;
    fs::set_permissions(prompt_file.path(), fs::Permissions::from_mode(0o600))
        .context("setting managed system-prompt file mode to 0600")?;
    prompt_file
        .write_all(system_prompt.as_bytes())
        .context("writing managed system-prompt file")?;
    prompt_file
        .flush()
        .context("flushing managed system-prompt file")?;
    let prompt_path = prompt_file
        .path()
        .to_str()
        .ok_or_else(|| anyhow!("managed system-prompt path is not valid UTF-8"))?
        .to_string();

    let mut args = startup_tail.to_vec();
    if let Some(flag) = prompt_transport {
        args.extend([flag.to_string(), prompt_path]);
    } else {
        let encoded = serde_json::to_string(system_prompt)
            .context("encoding Codex developer instructions")?;
        let system_args = [
            "--config".to_string(),
            format!("developer_instructions={encoded}"),
        ];
        if matches!(args.first().map(String::as_str), Some("resume" | "fork")) {
            let session_id = args
                .pop()
                .ok_or_else(|| anyhow!("managed Codex resume/fork is missing its session id"))?;
            args.extend(system_args);
            args.push(session_id);
        } else {
            args.extend(system_args);
        }
    }
    let params = AgentStartParams {
        name: req.name.clone(),
        kind: kind.to_string(),
        pane_id: pane_id.to_string(),
        args,
        timeout_ms: Some(AGENT_START_TIMEOUT_MS),
    };

    let operation = (|| -> anyhow::Result<Option<String>> {
        let started = agent_start_retry_name(client, &params, req.name_fallback.as_deref(), delay)
            .map_err(|error| {
                let message = error.to_string();
                let typed = if matches!(
                    &error,
                    HerdrError::Protocol { code, .. } if code == ERR_PANE_NOT_FOUND
                ) {
                    anyhow::Error::new(RetryablePlacementRace(error))
                } else {
                    anyhow::Error::new(error)
                };
                typed.context(format!("herdr agent.start for {}: {message}", req.name))
            })?;
        let ready = await_interactive_ready(client, &started)?;
        if let Some(text) = &req.initial_prompt {
            client
                .agent_prompt(&AgentPromptParams {
                    target: pane_id.to_string(),
                    text: text.clone(),
                    wait: None,
                })
                .with_context(|| format!("herdr agent.prompt for {}", req.name))?;
        }
        if kind == "codex" {
            return Ok(Some(await_agent_session_id(client, pane_id, &ready)?));
        }
        Ok(ready.agent_session.map(|session| session.value))
    })();

    let remove_result = prompt_file
        .close()
        .context("removing managed system-prompt file");
    match (operation, remove_result) {
        (Ok(session_id), Ok(())) => Ok(session_id),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(remove_error)) => Err(remove_error),
        (Err(error), Err(remove_error)) => Err(error.context(format!(
            "additionally failed to remove system-prompt file: {remove_error:#}"
        ))),
    }
}

fn agent_start_retry_name(
    client: &mut HerdrClient,
    params: &AgentStartParams,
    fallback: Option<&str>,
    delay: &DelayFn,
) -> Result<AgentStarted, HerdrError> {
    // The fallback is part of the same interaction: busy retries already
    // spent on the primary name must not be available again for the fallback.
    let mut busy_budget = AgentStartBusyRetryBudget::new();
    match agent_start_retry_busy(client, params, delay, &mut busy_budget) {
        Err(HerdrError::Protocol { code, message }) if code == ERR_AGENT_NAME_TAKEN => {
            if let Some(name) = fallback {
                let mut retry = params.clone();
                retry.name = name.to_string();
                agent_start_retry_busy(client, &retry, delay, &mut busy_budget)
            } else {
                Err(HerdrError::Protocol { code, message })
            }
        }
        result => result,
    }
}

struct AgentStartBusyRetryBudget {
    retries_remaining: usize,
    backoff: Duration,
}

impl AgentStartBusyRetryBudget {
    fn new() -> Self {
        Self {
            retries_remaining: AGENT_START_BUSY_RETRIES,
            backoff: AGENT_START_BUSY_BACKOFF,
        }
    }

    fn take_retry(&mut self) -> Option<Duration> {
        let delay = self
            .retries_remaining
            .checked_sub(1)
            .map(|_| self.backoff)?;
        self.retries_remaining -= 1;
        self.backoff = self.backoff.saturating_mul(2);
        Some(delay)
    }
}

/// A newly allocated board-owned pane can briefly retain Herdr's previous
/// agent state. Retry the exact start request on that same pane before giving
/// up; the caller's owned-pane cleanup handles a persistent busy response.
fn agent_start_retry_busy(
    client: &mut HerdrClient,
    params: &AgentStartParams,
    delay: &DelayFn,
    busy_budget: &mut AgentStartBusyRetryBudget,
) -> Result<AgentStarted, HerdrError> {
    loop {
        match client.agent_start(params) {
            Err(error)
                if matches!(
                    &error,
                    HerdrError::Protocol { code, .. } if code == ERR_AGENT_PANE_BUSY
                ) =>
            {
                if let Some(backoff) = busy_budget.take_retry() {
                    delay(backoff);
                } else {
                    return Err(error);
                }
            }
            result => return result,
        }
    }
}

fn is_interactive(agent: &AgentInfo) -> bool {
    agent.interactive_ready && !agent.launch_pending
}

fn await_interactive_ready(
    client: &mut HerdrClient,
    started: &AgentStarted,
) -> anyhow::Result<AgentInfo> {
    if is_interactive(&started.agent) {
        return Ok(started.agent.clone());
    }

    let pane_id = started.pane_id();
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut probes = 0_usize;
    loop {
        // Probe immediately several times. Protocol/socket fixtures generally
        // transition synchronously, and this avoids wall sleeps in unit tests.
        let agent = client
            .agent_get(pane_id)
            .with_context(|| format!("herdr agent.get while waiting for {pane_id}"))?;
        if is_interactive(&agent) {
            return Ok(agent);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for managed agent in pane {pane_id} to become interactive");
        }
        probes += 1;
        if probes >= IMMEDIATE_READINESS_PROBES {
            thread::sleep(
                READINESS_BACKOFF.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}

fn await_agent_session_id(
    client: &mut HerdrClient,
    pane_id: &str,
    ready: &AgentInfo,
) -> anyhow::Result<String> {
    if let Some(session) = &ready.agent_session {
        return Ok(session.value.clone());
    }
    let deadline = Instant::now() + READINESS_TIMEOUT;
    let mut probes = 0_usize;
    loop {
        let agent = client
            .agent_get(pane_id)
            .with_context(|| format!("herdr agent.get while waiting for {pane_id} session id"))?;
        if let Some(session) = agent.agent_session {
            return Ok(session.value);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for managed Codex in pane {pane_id} to report its session id");
        }
        probes += 1;
        if probes >= IMMEDIATE_READINESS_PROBES {
            thread::sleep(
                READINESS_BACKOFF.min(deadline.saturating_duration_since(Instant::now())),
            );
        }
    }
}
