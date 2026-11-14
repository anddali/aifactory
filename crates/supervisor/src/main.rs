//! The runner supervisor: the container's entry point. It authenticates to
//! the plane with the run token, restores the workspace, runs turns through
//! the harness adapter, streams events, sends heartbeats, enforces the
//! per-turn wall-clock limit, and checkpoints on request, after each turn
//! and before exit.

mod deny;
mod harness;
mod toolchain;
mod workspace;

use anyhow::{Context, Result};
use client::PlaneClient;
use harness::{TurnCtx, TurnInput};
use proto::*;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch};
use workspace::Workspace;

enum Pump {
    Flush(oneshot::Sender<()>),
}

/// Batches harness events and posts them to the plane.
fn spawn_pump(client: PlaneClient, generation: i64) -> (mpsc::UnboundedSender<HarnessEvent>, mpsc::UnboundedSender<Pump>) {
    let (tx, mut rx) = mpsc::unbounded_channel::<HarnessEvent>();
    let (ftx, mut frx) = mpsc::unbounded_channel::<Pump>();
    tokio::spawn(async move {
        loop {
            let mut batch = vec![];
            let mut flush = None;
            tokio::select! {
                ev = rx.recv() => match ev { Some(e) => batch.push(e), None => break },
                f = frx.recv() => match f { Some(Pump::Flush(s)) => flush = Some(s), None => break },
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            while let Ok(e) = rx.try_recv() {
                batch.push(e);
                if batch.len() >= 200 {
                    break;
                }
            }
            if flush.is_some() {
                while let Ok(e) = rx.try_recv() {
                    batch.push(e);
                }
            }
            if !batch.is_empty() {
                for chunk in batch.chunks(200) {
                    if let Err(e) = client.events(&EventBatch { generation, events: chunk.to_vec() }).await {
                        tracing::warn!("posting events failed: {e:#}");
                    }
                }
            }
            if let Some(s) = flush {
                let _ = s.send(());
            }
        }
    });
    (tx, ftx)
}

async fn flush(f: &mpsc::UnboundedSender<Pump>) {
    let (s, r) = oneshot::channel();
    if f.send(Pump::Flush(s)).is_ok() {
        let _ = tokio::time::timeout(Duration::from_secs(60), r).await;
    }
}

struct Sup {
    client: PlaneClient,
    generation: i64,
    ws: Workspace,
    harness: Box<dyn harness::Harness>,
    events: mpsc::UnboundedSender<HarnessEvent>,
    pump: mpsc::UnboundedSender<Pump>,
    deny: deny::DenyList,
    busy: Arc<AtomicBool>,
    handoff: Option<String>,
}

impl Sup {
    async fn checkpoint(&self, reason: &str) -> Result<()> {
        let commit = match self.ws.checkpoint_git().await {
            Ok(c) => c,
            Err(e) => {
                let _ = self.events.send(HarnessEvent::Error { message: format!("checkpoint push failed: {e:#}") });
                None
            }
        };
        let session = self.ws.export_session(&self.harness.session_files())?;
        self.client
            .upload_checkpoint(&CheckpointMeta { generation: self.generation, commit, reason: reason.into() }, session)
            .await
    }

    async fn exit(&self, reason: &str, checkpoint: bool) -> ! {
        if checkpoint {
            if let Err(e) = self.checkpoint(reason).await {
                tracing::error!("final checkpoint failed: {e:#}");
            }
        }
        flush(&self.pump).await;
        let _ = self.client.exit(&ExitNotice { generation: self.generation, reason: reason.into() }).await;
        tracing::info!("supervisor exiting: {reason}");
        std::process::exit(0)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt().json().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let client = PlaneClient::from_env()?;
    let generation: i64 = std::env::var("FACTORY_GENERATION").ok().and_then(|g| g.parse().ok()).unwrap_or(1);
    let harness_name = std::env::var("FACTORY_HARNESS").unwrap_or_else(|_| "scripted".into());
    let root = PathBuf::from(std::env::var("FACTORY_WORKDIR").unwrap_or_else(|_| "/workspace".into()));
    std::fs::create_dir_all(&root)?;
    let hello = client
        .hello(&HelloRequest { generation, harness: harness_name.clone(), image: std::env::var("FACTORY_IMAGE").ok() })
        .await
        .context("hello")?;
    if !hello.accepted {
        tracing::info!("plane did not accept this runner generation; exiting");
        return Ok(());
    }
    let spec = hello.spec;
    let (events, pump) = spawn_pump(client.clone(), generation);
    let ws = Workspace::new(root, client.clone(), spec.clone());

    // Restore the harness session from the last checkpoint.
    if spec.has_session {
        match client.download_session().await {
            Ok(Some(b)) => ws.import_session(&b).context("restoring session")?,
            Ok(None) => {}
            Err(e) => tracing::warn!("session download failed: {e:#}"),
        }
    }
    match ws.setup().await {
        Ok(m) => {
            let _ = events.send(HarnessEvent::Log { level: "info".into(), message: format!("workspace ready: {m}") });
        }
        Err(e) => {
            let _ = events.send(HarnessEvent::Error { message: format!("workspace setup failed: {e:#}") });
            flush(&pump).await;
            let _ = client.exit(&ExitNotice { generation, reason: "setup failed".into() }).await;
            std::process::exit(1);
        }
    }
    let deny = deny::DenyList::new(&spec.deny_patterns);
    toolchain::ensure(&ws.repo, &events).await;
    let rc = spec.runner_config.clone().or_else(|| toolchain::runner_config(&ws.repo));
    if let Some(rc) = &rc {
        for cmd in &rc.setup {
            if let Some(p) = deny.check(cmd) {
                let _ = events.send(HarnessEvent::Error { message: format!("setup command refused by deny-list ({p}): {cmd}") });
                continue;
            }
            let sh = if cfg!(windows) { ("cmd", "/C") } else { ("sh", "-c") };
            let o = tokio::process::Command::new(sh.0).args([sh.1, cmd]).current_dir(&ws.repo).output().await;
            let ok = o.as_ref().map(|o| o.status.success()).unwrap_or(false);
            let _ = events.send(HarnessEvent::Log { level: if ok { "info" } else { "warn" }.into(), message: format!("setup `{cmd}`: {}", if ok { "ok" } else { "failed" }) });
        }
    }
    let mut h = harness::make(&harness_name)?;
    h.prepare(&ws).await.context("preparing harness")?;
    // Resume fallback: a relaunched run whose harness cannot resume (or whose
    // session is gone) starts fresh, seeded with a handoff summary.
    let relaunch = spec.checkpoint_commit.is_some() || spec.has_session;
    let handoff = if relaunch && (!h.capabilities().resume || !h.has_session(&ws)) {
        let s = client.handoff_summary().await.ok();
        let _ = events.send(HarnessEvent::Log { level: "info".into(), message: "session not resumable; seeding a fresh session with a handoff summary".into() });
        s
    } else {
        None
    };

    let busy = Arc::new(AtomicBool::new(false));
    // Heartbeats.
    {
        let client = client.clone();
        let busy = busy.clone();
        let every = Duration::from_secs(spec.heartbeat_secs.max(1));
        tokio::spawn(async move {
            loop {
                match client.heartbeat(&Heartbeat { generation, busy: busy.load(Ordering::Relaxed), current_input: None }).await {
                    Ok(true) => {}
                    Ok(false) => {
                        tracing::warn!("plane rejected heartbeat (stale generation); exiting");
                        std::process::exit(0);
                    }
                    Err(e) => tracing::warn!("heartbeat failed: {e:#}"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }
    // Instruction poller.
    let (itx, mut irx) = mpsc::unbounded_channel::<InstructionEnvelope>();
    {
        let client = client.clone();
        tokio::spawn(async move {
            let mut seen = HashSet::new();
            loop {
                match client.instructions(generation, 25).await {
                    Ok(list) => {
                        for i in list {
                            if seen.insert(i.id) && itx.send(i).is_err() {
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("instruction poll failed: {e:#}");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                    }
                }
            }
        });
    }

    let mut sup = Sup { client: client.clone(), generation, ws, harness: h, events, pump, deny, busy, handoff };
    let grace = Duration::from_secs(spec.idle_grace_secs);
    let mut queue: std::collections::VecDeque<InstructionEnvelope> = Default::default();
    loop {
        let next = match queue.pop_front() {
            Some(i) => Some(i),
            None => match tokio::time::timeout(grace, irx.recv()).await {
                Ok(Some(i)) => Some(i),
                Ok(None) => None,
                Err(_) => None,
            },
        };
        let Some(env) = next else {
            sup.exit("idle grace period over", true).await;
        };
        match env.instruction.clone() {
            Instruction::Turn { input_id, kind, text, data } => {
                client.ack(generation, env.id).await?;
                let outcome = run_turn(&mut sup, &mut irx, &mut queue, env.id, TurnInput { instruction_id: env.id, input_id, kind, text, data, handoff: None }).await;
                flush(&sup.pump).await;
                if let Err(e) = sup.checkpoint("turn end").await {
                    tracing::warn!("checkpoint after turn failed: {e:#}");
                }
                client.turn_complete(&TurnComplete { generation, instruction_id: env.id, input_id, outcome, summary: None }).await?;
                if queue.iter().any(|i| matches!(i.instruction, Instruction::Teardown)) {
                    sup.exit("teardown", false).await;
                }
            }
            Instruction::Interrupt => {
                client.ack(generation, env.id).await?;
            }
            Instruction::Checkpoint => {
                client.ack(generation, env.id).await?;
                sup.checkpoint("requested").await?;
            }
            Instruction::Stop { checkpoint } => {
                client.ack(generation, env.id).await?;
                sup.exit("stop requested", checkpoint).await;
            }
            Instruction::Teardown => {
                let _ = client.ack(generation, env.id).await;
                sup.exit("teardown", false).await;
            }
        }
    }
}

/// Run one turn, watching for interrupts and the wall-clock limit.
async fn run_turn(
    sup: &mut Sup,
    irx: &mut mpsc::UnboundedReceiver<InstructionEnvelope>,
    queue: &mut std::collections::VecDeque<InstructionEnvelope>,
    _id: i64,
    mut input: TurnInput,
) -> TurnOutcome {
    sup.busy.store(true, Ordering::Relaxed);
    input.handoff = sup.handoff.take();
    let _ = sup.events.send(HarnessEvent::TurnStarted { input_id: input.input_id });
    let (itx, irx_flag) = watch::channel(false);
    let limit = Duration::from_secs(sup.ws.spec.limits.turn_timeout_secs.max(1));
    let input_id = input.input_id;
    let client = sup.client.clone();
    let generation = sup.generation;
    let ctx = TurnCtx { ws: &sup.ws, events: sup.events.clone(), interrupt: irx_flag, deny: &sup.deny };
    let fut = sup.harness.run_turn(ctx, input);
    tokio::pin!(fut);
    let deadline = tokio::time::sleep(limit);
    tokio::pin!(deadline);
    let mut timed_out = false;
    let mut interrupted = false;
    let outcome = loop {
        tokio::select! {
            r = &mut fut => break r,
            _ = &mut deadline => {
                if timed_out {
                    break Ok(TurnOutcome::TimedOut);
                }
                timed_out = true;
                let _ = itx.send(true);
                // Give the harness a moment to stop cleanly.
                deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(15));
            }
            i = irx.recv() => match i {
                Some(env) => match env.instruction {
                    Instruction::Interrupt => {
                        interrupted = true;
                        let _ = client.ack(generation, env.id).await;
                        let _ = itx.send(true);
                    }
                    Instruction::Checkpoint => {
                        let _ = client.ack(generation, env.id).await;
                        queue.push_back(InstructionEnvelope { id: env.id, instruction: Instruction::Checkpoint });
                    }
                    Instruction::Teardown | Instruction::Stop { .. } => {
                        let _ = itx.send(true);
                        queue.push_front(env);
                    }
                    Instruction::Turn { .. } => queue.push_back(env),
                },
                None => {}
            },
        }
    };
    sup.busy.store(false, Ordering::Relaxed);
    let outcome = match outcome {
        Ok(o) if timed_out => {
            let _ = o;
            TurnOutcome::TimedOut
        }
        Ok(TurnOutcome::Completed) if interrupted => TurnOutcome::Interrupted,
        Ok(o) => o,
        Err(e) => {
            let _ = sup.events.send(HarnessEvent::Error { message: format!("{e:#}") });
            TurnOutcome::Failed
        }
    };
    let _ = sup.events.send(HarnessEvent::TurnEnded { input_id, outcome });
    outcome
}
