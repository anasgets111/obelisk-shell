mod capabilities;
mod cli;
mod compositor;
mod control_client;
mod generation;
mod memory;
mod pam_worker;
mod polkit;
mod process;
mod reload;
mod reload_link;
mod setup;
mod snapshot;
mod socket;
// LuaCATS stub generator for tests only. `setup` reports stale stubs to users.
#[cfg(test)]
mod stubs;
mod supervisor;
mod watcher;

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use capabilities::Capabilities;
use capabilities::lock::{self, LockController};
use capabilities::network::NetworkController;
use generation::renderer_binary_path;
use polkit::PolkitAgent;
use shared::{Capability, ReevaluateReport, ReevaluateRequest, RendererFrame, SupervisorFrame, Zeroize};
use socket::send_frame_logged;
use supervisor::Supervisor;

/// Watcher delay after the last relevant `shell.lua` change, coalescing multi-event saves
/// (ADR-0024).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// § 14.2 ready-signal and § 14.3 evidence-verification deadlines (`reload::PbaTimings`), scaled
/// above `reload.rs`'s test constants for a Candidate binding Wayland/EGL. Healthy Candidates fit;
/// wedged ones cannot hang a config edit.
const PBA_TIMINGS: reload::PbaTimings = reload::PbaTimings {
    ready_timeout: Duration::from_secs(2),
    evidence_timeout: Duration::from_secs(3),
    reap_grace: process::DEFAULT_REAP_GRACE,
};

/// The next frame for the main loop: a deferred one first, then the socket. A swap handshake
/// borrows the shared receiver and takes frames it is not the reader of off it (ADR-0025), so
/// `replay` is where they wait; draining it first is what keeps their order.
///
/// Cancel-safe, which the `select!` arm requires: the pop is synchronous, so the only await point
/// is `Receiver::recv`, and a cancelled call cannot have taken a frame from either source.
async fn next_inbound(
    replay: &mut std::collections::VecDeque<socket::InboundFrame>,
    inbound: &mut tokio::sync::mpsc::Receiver<socket::InboundFrame>,
) -> Option<socket::InboundFrame> {
    match replay.pop_front() {
        Some(frame) => Some(frame),
        None => inbound.recv().await,
    }
}

/// Whether an `Unchanged` report names the most recently sent `Reevaluate`; a mismatch is a
/// superseded evaluation and cannot authorize the reload (ADR-0024).
fn is_current_reload(report_sequence: u64, next_sequence: u64) -> bool {
    report_sequence == next_sequence
}

/// Bumps and sends one `Reevaluate` sequence (ADR-0024, ADR-0041 decision 4). Debounced file
/// changes and Renderer `RequestReload` after `wl_output` changes share this counter, so stale
/// reports from either trigger fail `is_current_reload`.
fn begin_reload(registry: &socket::GenerationRegistry, generation_id: u32, next_sequence: &mut u64) {
    *next_sequence += 1;
    send_frame_logged(
        registry,
        generation_id,
        &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: *next_sequence }),
    );
}

/// Shared malformed-arguments log line for capability `dispatch` adapters (ADR-0037).
pub(crate) fn log_malformed_command(params: &shared::CommandParams) {
    eprintln!(
        "malformed {}.{} command from generation {}: {:?}",
        params.capability, params.action, params.generation_id, params.arguments
    );
}

/// Log line for a `dispatch` adapter's unmatched-action fallback.
pub(crate) fn log_unknown_action(params: &shared::CommandParams) {
    eprintln!("{}: unknown action {:?} from generation {}", params.capability, params.action, params.generation_id);
}

/// Logs a command for a controller never built (ADR-0070). A config cannot reach this: reading
/// `oblisk.<name>` sends the start before its `invoke` on the same socket. This is a buggy Renderer
/// or hand-written frame, so name the capability instead of staying silent.
pub(crate) fn log_unstarted(envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    eprintln!(
        "generation {}'s oblisk.{}:invoke({:?}) arrived before anything started {}; dropping",
        params.generation_id, params.capability, params.action, params.capability
    );
}

/// Deserializes `params.action`, logging and returning `None` for an unknown variant. Serde owns
/// the accepted spellings; there is no second action-name list, and `supervisor/src/stubs.rs`
/// generates Lua from the same enum.
pub(crate) fn parse_action<A: serde::de::DeserializeOwned>(params: &shared::CommandParams) -> Option<A> {
    use serde::de::IntoDeserializer;
    let action: Result<A, serde::de::value::Error> = A::deserialize(params.action.as_str().into_deserializer());
    match action {
        Ok(action) => Some(action),
        Err(_) => {
            log_unknown_action(params);
            None
        }
    }
}

/// Why `run_supervisor` returned and its process exit code (ADR-0059 decision 3). The service
/// restarts every exit except the code named by `RestartPreventExitStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shutdown {
    /// `SIGINT`, `SIGTERM`, or every channel closing; rerunning the shell is recovery.
    Requested,
    /// `generation::RestartBrake` refused a respawn; the same config would kill each fresh Renderer
    /// forever.
    RestartBrakeTripped,
}

impl Shutdown {
    /// Deliberately arbitrary except for what it is not: `3` is neither `0` (clean), `1` (`main`'s
    /// `?` failure), signal codes (128+), nor "not found" (127).
    fn exit_code(self) -> i32 {
        match self {
            Shutdown::Requested => 0,
            Shutdown::RestartBrakeTripped => 3,
        }
    }
}

/// Enters the PAM worker's tokio-free path (ADR-0028) before any D-Bus, runtime, or audio-thread
/// setup. The worker must not construct a tokio runtime.
fn main() -> Result<(), Box<dyn Error>> {
    if std::env::var_os("OBLISK_PAM_WORKER").is_some() {
        return pam_worker::run_worker();
    }

    let args = match cli::parse(std::env::args()) {
        Ok(args) => args,
        Err(message) => {
            eprintln!("oblisk: {message}\n\n{}", cli::HELP);
            std::process::exit(2);
        }
    };

    // Set before path resolution and in this process, not just in children. `shared::config_dir`
    // and every Renderer, including later swap generations, read it.
    if let Some(dir) = &args.config_dir {
        // SAFETY: no runtime or thread exists yet; the PAM worker branch above returns.
        unsafe { std::env::set_var(shared::CONFIG_DIR_ENV, dir) };
    }

    match args.command {
        cli::Command::Help => {
            print!("{}", cli::HELP);
            Ok(())
        }
        cli::Command::Version => {
            println!("oblisk {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        cli::Command::Init { force } => setup::run(&shared::config_dir()?, force),
        cli::Command::SetState(set) => control_client::send(set),
        cli::Command::Check => match setup::check(&shared::config_dir()?) {
            Ok(report) => {
                print!("{report}");
                Ok(())
            }
            Err(message) => {
                eprintln!("{message}");
                std::process::exit(1);
            }
        },
        cli::Command::Run => {
            // Exit explicitly: `Shutdown`'s code matters, while `main`'s `Result` only yields 0 or
            // 1 (ADR-0059 decision 3). `run_supervisor` has finished its teardown. Two workers,
            // not one per core (ADR-0124), cover socket/D-Bus/inotify/timer waits; the blocking
            // pool is separate. A twenty-core laptop otherwise used twenty bar-serving threads.
            let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build()?;
            let shutdown = runtime.block_on(run_supervisor())?;
            std::process::exit(shutdown.exit_code());
        }
    }
}
async fn run_supervisor() -> Result<Shutdown, Box<dyn Error>> {
    let connection = zbus::Connection::system().await?;

    let (tx, mut agent_requests) = tokio::sync::mpsc::unbounded_channel();
    let mut polkit_agent = PolkitAgent::new(tx);
    let (polkit_outcome_tx, mut polkit_outcomes) =
        tokio::sync::mpsc::unbounded_channel::<(String, shared::PamOutcome)>();

    // Notifications' sound player (ADR-0033): one `std::sync::mpsc` recv loop with no connection,
    // so it stays eager and config cannot gate it.
    let (sound_tx, sound_rx) = std::sync::mpsc::channel::<PathBuf>();
    std::thread::spawn(move || capabilities::notifications::run_sound_player(sound_rx));

    // Idle (ADR-0032): notify uses its own Wayland connection so idle authority survives Renderer
    // crash/reload (ADR-0010); inhibit uses the shared one. It starts on the first
    // `oblisk.idle` method call (ADR-0070), off the roster, so methods send start rather than
    // `__index` (`renderer/src/lua/idle.rs`). Events are not snapshots, so this receiver stays
    // separate from `Signals`.
    let (idle_signal_tx, mut idle_signals) = tokio::sync::mpsc::unbounded_channel::<shared::IdleEvent>();

    // Every capability's channel/controller (ADR-0076). `capabilities` owns running capabilities
    // and senders; `signals` is the receiver this loop awaits. An unread capability's idle sender
    // never wakes the loop.
    let (capabilities, mut signals) = Capabilities::new(connection.clone(), sound_tx, idle_signal_tx);

    let socket_path = shared::control_socket_path()?;
    let (registry, mut inbound_frames, mut connected) = socket::spawn_listener(&socket_path)?;

    // Lock (ADR-0042, ADR-0052): the Renderer holds and paints `ext_session_lock_v1`; this side
    // decides whether to take it. The controller must not cache the authoritative generation id,
    // because a swap reassigns it.
    let (lock_command_tx, mut lock_commands) = tokio::sync::mpsc::unbounded_channel::<shared::SetSessionLock>();
    let lock = LockController::new(lock_command_tx);
    // `loginctl lock-session` in, `LockedHint` out (ADR-0138). Subscribe here, before a key press,
    // rather than after config happens to read a member.
    let (logind_lock_tx, mut logind_lock_requests) = tokio::sync::mpsc::unbounded_channel::<()>();
    let session_bridge = lock::logind::SessionBridge::new(connection.clone(), logind_lock_tx).await;
    let (pam_outcome_tx, mut pam_outcomes) = tokio::sync::mpsc::unbounded_channel::<(u64, shared::PamOutcome)>();
    let (process_done_tx, mut process_done) = tokio::sync::mpsc::unbounded_channel::<(u32, u64)>();

    let config_dir = shared::config_dir()?;
    let mut reload_events = watcher::spawn_watcher(&config_dir, RELOAD_DEBOUNCE)?;

    // Generation 0 is boot-spawned by the Supervisor (ADR-0025); without it there is no shell, so
    // failure is fatal.
    let renderer_path = renderer_binary_path()?;
    let renderer_path_str = renderer_path.to_string_lossy().into_owned();
    let boot_child = process::spawn_group_leader(
        &renderer_path_str,
        &[],
        &[(shared::GENERATION_ID_ENV.to_string(), "0".to_string())],
    )?;
    // Immediately, and before the child can have finished starting: generation 0 belongs to this
    // pid, and the listener refuses any other process claiming it (`socket::GenerationRegistry`).
    if let Some(pid) = boot_child.id() {
        registry.expect_generation(0, pid);
    }

    let mut supervisor = Supervisor::new(
        registry,
        boot_child,
        renderer_path_str,
        capabilities,
        lock,
        lock::SessionLockedFlag::at(shared::session_locked_flag_path()?),
        session_bridge,
        pam_outcome_tx.clone(),
        polkit_outcome_tx,
        process_done_tx,
    );

    // Without handlers, Ctrl-C or SIGTERM kills the Supervisor and leaves a headless orphaned
    // Renderer.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut memory_sampler = memory::sampler_from_env();
    // Every break leaves this alone except the brake's exit, which a service manager must not
    // restart into (ADR-0059 decision 3).
    let mut shutdown = Shutdown::Requested;

    // Frames a swap handshake read off `inbound_frames` without being their reader (ADR-0156).
    // Drained ahead of the socket so they keep their arrival order relative to each other and to
    // everything that arrived after the swap.
    let mut replay: std::collections::VecDeque<socket::InboundFrame> = std::collections::VecDeque::new();

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                eprintln!("SIGINT received, shutting down");
                break;
            }
            _ = sigterm.recv() => {
                eprintln!("SIGTERM received, shutting down");
                break;
            }
            // ADR-0058 decision 1: dead and healthy-idle Renderers both send no frames; without
            // this arm `select!` cannot distinguish them.
            status = supervisor.authoritative.child.wait() => {
                if let Some(reason) = supervisor.replace_departed_renderer(status) {
                    shutdown = reason;
                    break;
                }
            }
            Some(_) = memory::tick_sampler(&mut memory_sampler) => {
                memory::log_sample("steady state", &[(supervisor.authoritative.generation_id, &supervisor.authoritative.child)]);
            }
            Some(request) = agent_requests.recv() => supervisor.handle_polkit_request(request),
            Some((cookie, outcome)) = polkit_outcomes.recv() => supervisor.record_polkit_outcome(cookie, outcome),
            Some(generation_id) = connected.recv() => supervisor.hydrate(generation_id),
            // One arm per snapshot capability (ADR-0076). `Signals::next` is the cancel-safe half
            // of the bare `recv()`s; `Capabilities::push` runs in the winning body, which
            // `select!` does not cancel. Thus the two capabilities that await while building state
            // cannot lose a signal to a busier branch.
            Some(signal) = signals.next() => supervisor.push_capability_signal(signal).await,
            Some(event) = idle_signals.recv() => {
                // Route to the generation whose `register_threshold` fired (`event.generation_id`),
                // not the authoritative one (ADR-0006). Send raw `IdleEvent`, not the
                // StateSnapshot/revision path: idle is event-shaped, not pollable state (ADR-0032).
                send_frame_logged(&supervisor.registry, event.generation_id, &SupervisorFrame::IdleEvent(event));
            }
            Some(command) = lock_commands.recv() => supervisor.send_lock_command(command),
            Some(()) = logind_lock_requests.recv() => supervisor.lock_requested_by_logind(),
            // Other half of the `secure_submit(lock, authenticate)` arm: the only path where
            // Success becomes an unlock order (ADR-0042).
            Some((acquisition, outcome)) = pam_outcomes.recv() => supervisor.record_pam_outcome(acquisition, outcome),
            Some(()) = reload_events.recv() => supervisor.begin_reload(),
            Some((generation_id, id)) = process_done.recv() => supervisor.reap_exited_process(generation_id, id),
            Some(inbound) = next_inbound(&mut replay, &mut inbound_frames) => match inbound.frame {
                RendererFrame::LockReport(report) if inbound.generation_id != supervisor.authoritative.generation_id => {
                    // A superseded, unreaped connection still sends frames. Either stale report
                    // corrupts the swap gate: Unlocked/Finished reaps the live lock holder, while
                    // Locked shuts the gate with no holder and no report reopens it.
                    eprintln!(
                        "generation {}'s lock report arrived from a non-authoritative generation (authoritative is {}); dropping: {report:?}",
                        inbound.generation_id, supervisor.authoritative.generation_id
                    );
                }
                RendererFrame::LockReport(report) => supervisor.record_lock_report(report),
                RendererFrame::Command(envelope) => match envelope.params.capability.as_str() {
                    // `process` is addressable but never started, so it is not a roster capability.
                    // `idle` joined the roster with ADR-0141 and uses the generic path.
                    "process" => supervisor.dispatch_process_command(&envelope).await,
                    name => match Capability::from_name(name) {
                        Some(capability) => supervisor.dispatch_capability_command(capability, &envelope).await,
                        None => eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope),
                    },
                },
                RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_) => {
                    // SocketCandidateLink reads these only mid-handshake from `inbound_frames` (see
                    // TopologyChanged, ADR-0025). Here they are stale or a wire-protocol desync.
                    eprintln!("generation {}'s handshake frame arrived outside any in-flight PBA handshake; dropping: {:?}", inbound.generation_id, inbound.frame);
                }
                RendererFrame::StartCapability { capability } => {
                    // ADR-0070: config read `oblisk.<capability>` or named it in `secure_submit`.
                    // Await inline (decision 4). Re-entrant because decision 3 makes each
                    // generation resend every name; each arm is a no-op after controller creation.
                    match Capability::from_name(&capability) {
                        // Built at boot; starting it registers the agent (ADR-0070 decision 5,
                        // ADR-0114).
                        Some(Capability::Polkit) => polkit_agent.register(&connection).await,
                        Some(capability) => supervisor.capabilities.start(capability).await,
                        None => eprintln!(
                            "generation {} asked to start {capability:?}, which is not a capability this Supervisor builds",
                            inbound.generation_id
                        ),
                    }
                }
                // ADR-0112: send `oblisk set`/`toggle` to the onscreen generation. The Renderer
                // applies or refuses it by name; only this process knows that generation.
                RendererFrame::SetState(set) => send_frame_logged(
                    &supervisor.registry,
                    supervisor.authoritative.generation_id,
                    &SupervisorFrame::SetState(set),
                ),
                // ADR-0041 decision 4: a `wl_output` appeared or disappeared.
                RendererFrame::RequestReload => supervisor.begin_reload(),
                RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence }) => {
                    supervisor.answer_unchanged_report(inbound.generation_id, sequence).await;
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) if supervisor.lock.defers_swap() => {
                    // ADR-0042: candidate N+1 cannot acquire generation N's lock, so PBA waits for
                    // release. `Unchanged` reloads are not gated. Use `defers_swap`, not
                    // `is_active`: an unreported lock order is also unswappable, and a swap then
                    // would reap the process owning the lock object.
                    supervisor.defer_swap(sequence);
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) => {
                    supervisor.swap_generation(sequence, &mut inbound_frames, &mut replay).await;
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                    eprintln!("generation {}'s shell.lua re-evaluation (sequence {sequence}) failed: {error}", inbound.generation_id);
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "polkit" && submit.action == "authenticate" => {
                    // ADR-0028, ADR-0114. Before the catch-all arm because matches are ordered.
                    // `mem::take` gives plaintext to the worker, which zeroizes every return path.
                    let secret = std::mem::take(&mut submit.secret);
                    supervisor.begin_polkit_authentication(secret, submit.generation_id);
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "network" && submit.action == "connect" => {
                    // ADR-0029: empty secret means open network; non-empty is the WPA-PSK password.
                    // Before the catch-all for the same ordering reason as polkit.
                    match supervisor.capabilities.network().and_then(NetworkController::take_connect_intent) {
                        Some(pending) => {
                            // `mem::take` gives plaintext to `NetworkController::connect`, which
                            // zeroizes every return path.
                            let secret = std::mem::take(&mut submit.secret);
                            let controller = supervisor
                                .capabilities
                                .network()
                                .cloned()
                                .expect("take_connect_intent above only answers from a live controller");
                            tokio::spawn(async move { controller.connect(pending, secret).await; });
                        }
                        None => {
                            eprintln!(
                                "generation {}'s secure_submit(network, connect) arrived with no pending network connect intent; dropping",
                                submit.generation_id
                            );
                            submit.secret.zeroize();
                        }
                    }
                }
                RendererFrame::SecureSubmit(mut submit)
                    if submit.capability == "lock"
                        && submit.action == "authenticate"
                        && inbound.generation_id != supervisor.authoritative.generation_id =>
                {
                    // Only the authoritative generation owns the lock screen where the password
                    // was typed. Polkit/network accept any generation because the Supervisor owns
                    // their challenges; a lock belongs to one generation (ADR-0042).
                    eprintln!(
                        "generation {}'s secure_submit(lock, authenticate) is stale -- {} is authoritative; dropping",
                        submit.generation_id, supervisor.authoritative.generation_id
                    );
                    submit.secret.zeroize();
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "lock" && submit.action == "authenticate" => {
                    // ADR-0042, ADR-0052: this arm and `pam_outcomes` are the only unlock path,
                    // so `unlock_and_destroy` follows successful authentication at one call site.
                    // It must precede the catch-all. Unlike polkit/network, no pending intent is
                    // needed; the lock submission is complete.
                    // `try_begin_authentication` rejects no-lock and concurrent submissions. Its
                    // acquisition travels through the worker so `pam_outcomes` matches this lock.
                    if let Some(acquisition) = supervisor.lock.try_begin_authentication() {
                        supervisor.push_lock_state();
                        // `mem::take` gives plaintext to `run_authentication`, which zeroizes on
                        // panic and shutdown cancellation too. Spawn instead of await: Enter is
                        // unbounded and common, while pam_unix's ~2s failure delay would stall
                        // LockReport, reload, and process reap. The Supervisor runs as the user.
                        let secret = std::mem::take(&mut submit.secret);
                        let outcome_tx = supervisor.pam_outcome_tx.clone();
                        tokio::spawn(pam_worker::run_authentication(
                            nix::unistd::Uid::current().as_raw(),
                            shared::Zeroizing::new(secret),
                            acquisition,
                            outcome_tx,
                        ));
                    } else {
                        eprintln!(
                            "generation {}'s secure_submit(lock, authenticate) arrived with no lock held, or with an attempt already in flight; dropping",
                            submit.generation_id
                        );
                        submit.secret.zeroize();
                    }
                }
                RendererFrame::SecureSubmit(mut submit) => {
                    // Channel-forward/log placeholder for other capability/actions (ADR-0015's
                    // textfield/IPC half), none implemented yet. Log only length, before zeroizing
                    // so the read precedes the clear.
                    eprintln!(
                        "generation {}'s secure_submit received: capability={:?} action={:?} secret_len={}",
                        submit.generation_id,
                        submit.capability,
                        submit.action,
                        submit.secret.len()
                    );
                    submit.secret.zeroize();
                }
            },
            else => break,
        }
    }

    supervisor.reap().await;
    // Best effort; `socket::bind` removes a missed stale file on the next boot.
    let _ = std::fs::remove_file(&socket_path);
    Ok(shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tripped_restart_brake_exits_with_a_code_a_service_manager_will_not_restart() {
        // ADR-0059 decision 3: systemd's 5-in-10s start limit misses this brake's give-up
        // (three deaths over 60s); the exit code carries the difference.
        assert_ne!(Shutdown::RestartBrakeTripped.exit_code(), Shutdown::Requested.exit_code());
        assert_ne!(Shutdown::RestartBrakeTripped.exit_code(), 0, "a give-up is not a clean exit");
    }

    #[test]
    fn a_requested_shutdown_exits_cleanly_so_a_restart_policy_treats_it_as_one() {
        // SIGTERM at session end must not look like failure. `RestartPreventExitStatus` names one
        // code, so every other exit means restarting is recovery.
        assert_eq!(Shutdown::Requested.exit_code(), 0);
    }

    /// Order is the whole point of holding the frames in a queue rather than pushing them back
    /// onto the socket channel: a deferred `StartCapability` must be handled before whatever
    /// arrived while the swap was finishing, not after it.
    #[tokio::test]
    async fn next_inbound_drains_every_deferred_frame_before_it_reads_the_socket_again() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        tx.send(socket::InboundFrame {
            generation_id: 9,
            frame: shared::RendererFrame::StartCapability { capability: "after".to_string() },
        })
        .await
        .unwrap();

        let mut replay: std::collections::VecDeque<socket::InboundFrame> = ["lock", "audio"]
            .into_iter()
            .map(|capability| socket::InboundFrame {
                generation_id: 9,
                frame: shared::RendererFrame::StartCapability { capability: capability.to_string() },
            })
            .collect();

        let mut seen = Vec::new();
        for _ in 0..3 {
            let inbound = next_inbound(&mut replay, &mut rx).await.expect("three frames are available");
            let shared::RendererFrame::StartCapability { capability } = inbound.frame else {
                panic!("only starts were queued");
            };
            seen.push(capability);
        }
        assert_eq!(seen, ["lock", "audio", "after"], "both held frames come first, in the order they were held");
    }

    #[test]
    fn is_current_reload_matches_only_the_most_recently_sent_sequence() {
        assert!(is_current_reload(3, 3));
        assert!(!is_current_reload(3, 4), "a report for an older sequence than the last-sent one must be stale");
    }

    #[test]
    fn begin_reload_bumps_the_supervisor_owned_sequence_and_sends_the_reevaluate_carrying_it() {
        let registry = socket::GenerationRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        registry.register(7, tx, std::sync::Arc::new(tokio::sync::Notify::new()));
        let mut next_sequence = 0;

        // Watcher file changes and `RequestReload` (ADR-0041 decision 4) share this call, so both
        // must produce distinct increasing sequences.
        begin_reload(&registry, 7, &mut next_sequence);
        begin_reload(&registry, 7, &mut next_sequence);

        assert_eq!(next_sequence, 2);
        let sent: Vec<SupervisorFrame> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|payload| serde_json::from_slice(&payload).unwrap())
            .collect();
        assert_eq!(
            sent,
            vec![
                SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 1 }),
                SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: 2 }),
            ]
        );
    }

    #[test]
    fn begin_reload_still_advances_the_sequence_when_the_generation_has_no_connection() {
        // `send_frame_logged` drops `NoConnection` after logging. Advance anyway, or a reconnecting
        // generation's report could collide with an accepted sequence.
        let registry = socket::GenerationRegistry::default();
        let mut next_sequence = 41;
        begin_reload(&registry, 7, &mut next_sequence);
        assert_eq!(next_sequence, 42);
    }
}
