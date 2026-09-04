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
// The LuaCATS stub generator, a development tool with no place in the shipped binary. The reader
// that tells a user their stubs are stale lives in `setup`, which is the thing that acts on it.
#[cfg(test)]
mod stubs;
mod supervisor;
mod watcher;

use std::error::Error;
use std::path::PathBuf;
use std::time::Duration;

use capabilities::lock::{self, LockController};
use capabilities::network::NetworkController;
use capabilities::{Capabilities, Startable};
use generation::renderer_binary_path;
use polkit::PolkitAgent;
use shared::{Capability, ReevaluateReport, ReevaluateRequest, RendererFrame, SupervisorFrame, Zeroize};
use socket::send_frame_logged;
use supervisor::Supervisor;

/// How long the Watcher waits after the last relevant `shell.lua` change before dispatching a
/// reload -- coalesces a multi-event save into one round trip. Fixed (ADR-0024).
const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// § 15.2/15.3's ready-signal and evidence-verification deadlines (`reload::PbaTimings`), scaled
/// up from `reload.rs`'s own test constants for a real Candidate that has to bind Wayland/EGL.
/// Generous enough a healthy Candidate never trips them, tight enough a wedged one doesn't hang a
/// config edit.
const PBA_TIMINGS: reload::PbaTimings = reload::PbaTimings {
    ready_timeout: Duration::from_secs(2),
    evidence_timeout: Duration::from_secs(3),
    reap_grace: process::DEFAULT_REAP_GRACE,
};

/// Whether an `Unchanged` report's `sequence` still names the most recently sent `Reevaluate`.
/// A mismatch means a newer `Reevaluate` already went out for this generation, so the go-ahead
/// must not fire for a superseded evaluation (ADR-0024).
fn is_current_reload(report_sequence: u64, next_sequence: u64) -> bool {
    report_sequence == next_sequence
}

/// Starts one reload cycle: bumps the sequence and sends the `Reevaluate` carrying it
/// (ADR-0024, ADR-0041 decision 4). Reached by the watcher's debounced file change and by a
/// Renderer's `RequestReload` after a `wl_output` change -- both funnel through the same counter
/// so `is_current_reload` rejects a stale report from either trigger.
fn begin_reload(registry: &socket::GenerationRegistry, generation_id: u32, next_sequence: &mut u64) {
    *next_sequence += 1;
    send_frame_logged(
        registry,
        generation_id,
        &SupervisorFrame::Reevaluate(ReevaluateRequest { sequence: *next_sequence }),
    );
}

/// The malformed-arguments log line every capability's `dispatch` adapter shares (ADR-0037).
pub(crate) fn log_malformed_command(params: &shared::CommandParams) {
    eprintln!(
        "malformed {}.{} command from generation {}: {:?}",
        params.capability, params.action, params.generation_id, params.arguments
    );
}

/// [`log_malformed_command`]'s sibling for a `dispatch` adapter's unmatched-action fallback.
pub(crate) fn log_unknown_action(params: &shared::CommandParams) {
    eprintln!("{}: unknown action {:?} from generation {}", params.capability, params.action, params.generation_id);
}

/// Logs a command for a capability whose controller was never built (ADR-0070).
///
/// Not reachable from a config: reading `oblisk.<name>` is what hands out the object an `invoke`
/// is a method on, and that read sends the start on the same socket, in order, ahead of the
/// command. What reaches here is a Renderer that sent a command without the read -- a bug in this
/// codebase or a hand-written frame -- so it names the capability rather than being silent.
pub(crate) fn log_unstarted(envelope: &shared::CommandEnvelope) {
    let params = &envelope.params;
    eprintln!(
        "generation {}'s oblisk.{}:invoke({:?}) arrived before anything started {}; dropping",
        params.generation_id, params.capability, params.action, params.capability
    );
}

/// Reads `params.action` as a capability's action enum, logging and returning `None` when it names
/// no variant. Serde owns the string-to-variant mapping, so the spellings a capability accepts are
/// its enum's variants and nothing else -- there is no second list of action names anywhere, and
/// `supervisor/src/stubs.rs` generates the Lua side from the same enum.
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

/// Why `run_supervisor` returned, and the process exit code it becomes (ADR-0059 decision 3).
/// `packaging/oblisk-shell.service` restarts this process on every exit but one, so that exit
/// needs a code of its own to be named in `RestartPreventExitStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shutdown {
    /// `SIGINT`, `SIGTERM`, or every channel closing. Rerunning the shell is recovery.
    Requested,
    /// `generation::RestartBrake` refused another respawn. Rerunning would hand the same config to a fresh
    /// Renderer, which dies the same way, forever.
    RestartBrakeTripped,
}

impl Shutdown {
    /// `3` is arbitrary except for what it isn't: not `0` (a clean exit), not `1` (`main`'s own
    /// `?` failure), and not a code shell conventions reserve for signals (128+) or "not found" (127).
    fn exit_code(self) -> i32 {
        match self {
            Shutdown::Requested => 0,
            Shutdown::RestartBrakeTripped => 3,
        }
    }
}

/// Branches into the PAM worker's own tokio-free path (ADR-0028) before the normal Supervisor.
/// Must run before any D-Bus/tokio-runtime/audio-thread setup -- the worker path must not
/// construct a tokio runtime at all.
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

    // Set before anything resolves a path, and set in this process rather than passed down.
    // `shared::config_dir` reads it, and so does every Renderer spawned from here, including the
    // ones a later generation swap spawns (see that function's own note).
    if let Some(dir) = &args.config_dir {
        // SAFETY: single-threaded. No tokio runtime exists yet and no thread has been spawned;
        // the PAM worker re-exec above is the only earlier branch and it returns.
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
            // std::process::exit, not return: the exit code is the point of `Shutdown`, and
            // `main`'s `Result` can only produce 0 or 1 (ADR-0059 decision 3). Every teardown
            // `run_supervisor` owns has already run by the time it returns.
            // Two workers, not one per core (ADR-0124): every task here waits on a socket, a
            // D-Bus signal, an inotify event or a timer, and the blocking pool is separate. On a
            // twenty-core laptop the default was twenty threads to serve a bar.
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

    // Notifications' sound player (ADR-0033). The thread is one `std::sync::mpsc` recv loop with
    // no connection behind it, so it stays eager -- there is nothing for a config to gate.
    let (sound_tx, sound_rx) = std::sync::mpsc::channel::<PathBuf>();
    std::thread::spawn(move || capabilities::notifications::run_sound_player(sound_rx));

    // Idle capability (ADR-0032): notify rides its own Wayland connection (idle authority must
    // survive a Renderer crash or reload, ADR-0010); inhibit rides the shared connection. Built
    // when the config first calls a method on `oblisk.idle` like every other capability
    // (ADR-0070) -- off the roster, so its own methods send the start rather than an
    // `__index` (`renderer/src/lua/idle.rs`). Its events are not snapshots, so its receiver stays
    // here rather than joining `Signals`.
    let (idle_signal_tx, mut idle_signals) = tokio::sync::mpsc::unbounded_channel::<shared::IdleEvent>();

    // Every capability's channel and controller (ADR-0076). `capabilities` owns what is
    // running and every sender; `signals` is the receiving half this loop awaits. A capability the
    // config never reads keeps a sender nobody sends on, so it simply never wakes the loop.
    let (capabilities, mut signals) = Capabilities::new(connection.clone(), sound_tx, idle_signal_tx);

    let socket_path = shared::control_socket_path()?;
    let (registry, mut inbound_frames, mut connected) = socket::spawn_listener(&socket_path)?;

    // lock capability (ADR-0042, ADR-0052): the Renderer holds ext_session_lock_v1 and
    // paints it; this side owns the decision to take it. The channel exists because the
    // controller must not cache the authoritative generation id -- a swap reassigns it.
    let (lock_command_tx, mut lock_commands) = tokio::sync::mpsc::unbounded_channel::<shared::SetSessionLock>();
    let lock = LockController::new(lock_command_tx);
    // `loginctl lock-session` in, `LockedHint` out (ADR-0138). Built here rather than lazily
    // because the signal has to be subscribed before anyone presses the key, not after a config
    // happens to read a member.
    let (logind_lock_tx, mut logind_lock_requests) = tokio::sync::mpsc::unbounded_channel::<()>();
    let session_bridge = lock::logind::SessionBridge::new(connection.clone(), logind_lock_tx).await;
    let (pam_outcome_tx, mut pam_outcomes) = tokio::sync::mpsc::unbounded_channel::<(u64, shared::PamOutcome)>();
    let (process_done_tx, mut process_done) = tokio::sync::mpsc::unbounded_channel::<(u32, u64)>();

    let config_dir = shared::config_dir()?;
    let mut reload_events = watcher::spawn_watcher(&config_dir, RELOAD_DEBOUNCE)?;

    // Generation 0 is boot-spawned by the Supervisor itself (ADR-0025) -- there is no
    // shell without it, so a spawn failure here is fatal to main.
    let renderer_path = renderer_binary_path()?;
    let renderer_path_str = renderer_path.to_string_lossy().into_owned();
    let boot_child = process::spawn_group_leader(
        &renderer_path_str,
        &[],
        &[(shared::GENERATION_ID_ENV.to_string(), "0".to_string())],
    )?;

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

    // With no handler, Ctrl-C killed the Supervisor on the spot, leaving the Renderer orphaned
    // and running headless. SIGTERM gets the same treatment.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut memory_sampler = memory::sampler_from_env();
    // Every break below leaves this alone except the brake's, the one exit a service manager must
    // not restart into (ADR-0059 decision 3).
    let mut shutdown = Shutdown::Requested;

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
            // ADR-0058 decision 1: a dead Renderer sends no frames, and a healthy idle one
            // sends none either -- without this arm the difference never reaches select! at all.
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
            // One arm for every snapshot capability (ADR-0076). `Signals::next` is the
            // cancel-safe half -- bare `recv()`s -- and `Capabilities::push` runs here in the
            // winning arm's body, which `select!` never cancels, so the two capabilities that
            // `await` while building their state cannot lose a signal to a busier branch.
            Some(signal) = signals.next() => supervisor.push_capability_signal(signal).await,
            Some(event) = idle_signals.recv() => {
                // Routed to whichever generation made the register_threshold call now firing
                // (event.generation_id), not the authoritative one -- ADR-0006. Sent as a raw
                // IdleEvent, not through the StateSnapshot/revision path: idle is event-shaped,
                // not pollable state (ADR-0032).
                send_frame_logged(&supervisor.registry, event.generation_id, &SupervisorFrame::IdleEvent(event));
            }
            Some(command) = lock_commands.recv() => supervisor.send_lock_command(command),
            Some(()) = logind_lock_requests.recv() => supervisor.lock_requested_by_logind(),
            // The other half of the secure_submit(lock, authenticate) arm below -- the one place a
            // Success becomes an unlock order (ADR-0042).
            Some((acquisition, outcome)) = pam_outcomes.recv() => supervisor.record_pam_outcome(acquisition, outcome),
            Some(()) = reload_events.recv() => supervisor.begin_reload(),
            Some((generation_id, id)) = process_done.recv() => supervisor.reap_exited_process(generation_id, id),
            Some(inbound) = inbound_frames.recv() => match inbound.frame {
                RendererFrame::LockReport(report) if inbound.generation_id != supervisor.authoritative.generation_id => {
                    // A superseded-but-not-yet-reaped connection is a real frame source, and
                    // either direction of a stale report corrupts the swap gate: a stale
                    // Unlocked/Finished reopens the gate into a swap that reaps the live lock
                    // holder; a stale Locked shuts the gate with no holder, and no report ever
                    // reopens it.
                    eprintln!(
                        "generation {}'s lock report arrived from a non-authoritative generation (authoritative is {}); dropping: {report:?}",
                        inbound.generation_id, supervisor.authoritative.generation_id
                    );
                }
                RendererFrame::LockReport(report) => supervisor.record_lock_report(report),
                RendererFrame::Command(envelope) => match envelope.params.capability.as_str() {
                    // Three names reach dispatch that are not roster capabilities: `process`,
                    // which is addressable but never started, `idle`, which is event-shaped
                    // (ADR-0032), and anything else, which is a Renderer bug or a hand-written
                    // frame. Everything else is one exhaustive match inside `Capabilities`.
                    "process" => supervisor.dispatch_process_command(&envelope).await,
                    "idle" => supervisor.capabilities.dispatch_idle(&envelope),
                    name => match Capability::from_name(name) {
                        Some(capability) => supervisor.dispatch_capability_command(capability, &envelope).await,
                        None => eprintln!("inbound command from generation {}: {:?}", inbound.generation_id, envelope),
                    },
                },
                RendererFrame::ReadySignal(_) | RendererFrame::PresentationEvidence(_) => {
                    // Both only matter mid-handshake, where SocketCandidateLink reads them
                    // directly off inbound_frames (see TopologyChanged below, ADR-0025).
                    // Reaching here means it arrived outside any in-flight handshake -- stale, or
                    // a wire-protocol desync.
                    eprintln!("generation {}'s handshake frame arrived outside any in-flight PBA handshake; dropping: {:?}", inbound.generation_id, inbound.frame);
                }
                RendererFrame::StartCapability { capability } => {
                    // ADR-0070: the config read `oblisk.<capability>` (or declared a
                    // `secure_submit` naming it), and this is the first time anything in this
                    // process has. Awaited inline rather than spawned -- decision 4 says why.
                    // Re-entrant: decision 3 has every generation re-send every name it read, and
                    // each arm below is a no-op once its controller exists.
                    match Startable::from_name(&capability) {
                        // Its controller is built at boot; starting it is registering the agent
                        // (ADR-0070 decision 5, ADR-0114).
                        Some(Startable::Capability(Capability::Polkit)) => polkit_agent.register(&connection).await,
                        Some(Startable::Capability(capability)) => supervisor.capabilities.start(capability).await,
                        Some(Startable::Idle) => supervisor.capabilities.start_idle().await,
                        None => eprintln!(
                            "generation {} asked to start {capability:?}, which is not a capability this Supervisor builds",
                            inbound.generation_id
                        ),
                    }
                }
                // ADR-0112: `oblisk set`/`oblisk toggle`, to whichever generation is on screen.
                // The Renderer applies it or refuses it by name; this process only knows which
                // generation is authoritative, which is the one thing the client cannot know.
                RendererFrame::SetState(set) => send_frame_logged(
                    &supervisor.registry,
                    supervisor.authoritative.generation_id,
                    &SupervisorFrame::SetState(set),
                ),
                // ADR-0041 decision 4: a wl_output appeared or disappeared.
                RendererFrame::RequestReload => supervisor.begin_reload(),
                RendererFrame::ReevaluateReport(ReevaluateReport::Unchanged { sequence }) => {
                    supervisor.answer_unchanged_report(inbound.generation_id, sequence).await;
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) if supervisor.lock.defers_swap() => {
                    // ADR-0042: candidate N+1 cannot acquire the lock generation N holds, so
                    // PBA's handoff waits until the lock clears. In-place reloads (Unchanged
                    // above) are not gated. Checked as defers_swap, not is_active: a lock order
                    // that's out but not yet reported is just as unswappable, and that window can
                    // be long -- a swap inside it would reap the process owning the lock object.
                    supervisor.defer_swap(sequence);
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::TopologyChanged { sequence }) => {
                    supervisor.swap_generation(sequence, &mut inbound_frames).await;
                }
                RendererFrame::ReevaluateReport(ReevaluateReport::Failed { sequence, error }) => {
                    eprintln!("generation {}'s shell.lua re-evaluation (sequence {sequence}) failed: {error}", inbound.generation_id);
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "polkit" && submit.action == "authenticate" => {
                    // ADR-0028, ADR-0114. Must come before the catch-all SecureSubmit arm below --
                    // match arms are tried in order. mem::take moves the plaintext out for the
                    // worker to own and zeroize on every return path.
                    let secret = std::mem::take(&mut submit.secret);
                    supervisor.begin_polkit_authentication(secret, submit.generation_id);
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "network" && submit.action == "connect" => {
                    // ADR-0029: an empty secret means an open network, a non-empty one becomes
                    // the wpa-psk password. Must come before the catch-all arm below, same
                    // ordering reason as the polkit arm above.
                    match supervisor.capabilities.network().and_then(NetworkController::take_connect_intent) {
                        Some(pending) => {
                            // mem::take moves the plaintext out for NetworkController::connect to
                            // own and zeroize on every return path.
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
                    // The same stale-frame guard the LockReport arm above takes: only the
                    // authoritative generation paints the lock screen a password can have been
                    // typed into. Unlike polkit/network, which take any generation's submission
                    // (they answer a challenge the Supervisor itself holds), a lock belongs to
                    // exactly one generation (ADR-0042).
                    eprintln!(
                        "generation {}'s secure_submit(lock, authenticate) is stale -- {} is authoritative; dropping",
                        submit.generation_id, supervisor.authoritative.generation_id
                    );
                    submit.secret.zeroize();
                }
                RendererFrame::SecureSubmit(mut submit) if submit.capability == "lock" && submit.action == "authenticate" => {
                    // ADR-0042, ADR-0052: this arm and the pam_outcomes arm above are
                    // the only path to an unlock, making "never unlock_and_destroy except on a
                    // successful authentication" a property of one call site. Must come before the
                    // catch-all arm below. No pending-intent lookup, unlike polkit/network: the
                    // lock screen's submission carries everything needed.
                    //
                    // try_begin_authentication refuses a submit with no lock held and a second
                    // submit while one is in flight. The acquisition
                    // it returns is carried through the worker so pam_outcomes above can tell
                    // this lock's answer from the one before it.
                    if let Some(acquisition) = supervisor.lock.try_begin_authentication() {
                        supervisor.push_lock_state();
                        // mem::take moves the plaintext out for run_authentication to own and
                        // zeroize on every exit path, including a panic or shutdown cancellation.
                        // Spawned, not awaited inline: a lock screen's Enter key is neither
                        // bounded nor rare, and pam_unix's ~2s failure delay would stall every
                        // LockReport, reload, and process reap behind it. The user is this
                        // process's own owner, since the Supervisor runs as the session user.
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
                    // A channel-forward-and-log placeholder for every capability/action other
                    // than the guarded arms above (ADR-0015's textfield/IPC half), none of
                    // which exist yet.
                    //
                    // Never log the secret -- only its length -- and log before zeroizing, so the
                    // length read happens before the clear.
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
    // Best-effort: socket::bind's own stale-file removal covers a missed unlink on the next boot.
    let _ = std::fs::remove_file(&socket_path);
    Ok(shutdown)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tripped_restart_brake_exits_with_a_code_a_service_manager_will_not_restart() {
        // ADR-0059 decision 3: systemd's default start limit (5 in 10s) is too fast to catch
        // this brake's give-up (three deaths over 60s). The code is what carries the difference.
        assert_ne!(Shutdown::RestartBrakeTripped.exit_code(), Shutdown::Requested.exit_code());
        assert_ne!(Shutdown::RestartBrakeTripped.exit_code(), 0, "a give-up is not a clean exit");
    }

    #[test]
    fn a_requested_shutdown_exits_cleanly_so_a_restart_policy_treats_it_as_one() {
        // SIGTERM at session end must not look like a failure -- `RestartPreventExitStatus` names
        // one code, so every other exit has to mean "restarting me is recovery".
        assert_eq!(Shutdown::Requested.exit_code(), 0);
    }

    #[test]
    fn is_current_reload_matches_only_the_most_recently_sent_sequence() {
        assert!(is_current_reload(3, 3));
        assert!(!is_current_reload(3, 4), "a report for an older sequence than the last-sent one must be stale");
    }

    #[test]
    fn begin_reload_bumps_the_supervisor_owned_sequence_and_sends_the_reevaluate_carrying_it() {
        let registry = socket::GenerationRegistry::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        registry.register(7, tx);
        let mut next_sequence = 0;

        // Both triggers (the watcher's file change and RequestReload, ADR-0041 decision 4)
        // reach this same call, so two of them must produce two distinct, increasing sequences.
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
        // send_frame_logged logs and drops a NoConnection failure. The sequence must advance
        // anyway, or a reconnecting generation's later report could collide with an already-
        // accepted sequence.
        let registry = socket::GenerationRegistry::default();
        let mut next_sequence = 41;
        begin_reload(&registry, 7, &mut next_sequence);
        assert_eq!(next_sequence, 42);
    }
}
