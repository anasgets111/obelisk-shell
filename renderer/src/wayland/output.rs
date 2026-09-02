//! Connected outputs (`wl_output`), the `screens` signal payload, and presentation feedback.
//! `Screen`/`OutputFacts` and their conversions are the single source both the `screens` Lua
//! signal and `layout::instance`'s monitor matching read (ADR-0041 decision 2).

use super::*;
use crate::wayland::surface::TrackedRole;

/// One connected output, exactly as `wl_output` reports it (ADR-0041 decision 2): the source both
/// the `screens` Lua signal and `layout::instance::expand_instances`'s `monitor` matching read.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Screen {
    name: String,
    width: i32,
    height: i32,
    scale: i32,
    /// Hz. `wl_output`'s `mode` event reports millihertz, which is not the unit anyone writes a
    /// config against, so the division happens once here rather than in every config.
    refresh: f64,
}
/// The `smithay_client_toolkit::output::OutputInfo` fields [`screen_entry`] reads, lifted off it by
/// [`App::screens`]. A separate struct since `OutputInfo` is `#[non_exhaustive]` with no public
/// constructor, so a function taking one could never be unit-tested, unlike this conversion.
struct OutputFacts {
    name: Option<String>,
    logical_size: Option<(i32, i32)>,
    /// The *current* `Mode`'s `(dimensions, refresh_rate)`, or `None` if none is advertised. Both
    /// fields travel together so they can't disagree about which mode they describe.
    current_mode: Option<((i32, i32), i32)>,
    scale_factor: i32,
}
/// One output's `screens` entry, or `None` for an output whose size cannot be known.
/// `logical_size` first (`xdg_output`/`wl_output` v4's compositor-space size, the space a layer
/// surface's own coordinates are in), falling back to the current `Mode`'s `dimensions`. Neither
/// present yields nothing rather than a made-up size; the caller logs the miss.
/// ponytail: a nameless output (below `wl_output` v4) takes a positional `"output-{index}"` id,
/// carried over from the deleted `wallpaper_surface_id`. `monitor = "DP-1"` can never match there;
/// no client-side upgrade path exists, since the name genuinely does not exist.
fn screen_entry(index: usize, facts: &OutputFacts) -> Option<Screen> {
    let (width, height) = facts.logical_size.or_else(|| facts.current_mode.map(|(dimensions, _)| dimensions))?;
    Some(Screen {
        name: facts.name.clone().unwrap_or_else(|| format!("output-{index}")),
        width,
        height,
        scale: facts.scale_factor,
        // `Mode`'s own docs already allow a zero refresh rate ("if an output has no correct
        // refresh rate, such as a virtual output"), so no current mode reads the same way.
        refresh: facts.current_mode.map_or(0.0, |(_, rate)| f64::from(rate) / 1000.0),
    })
}
/// The `screens` signal's payload: § 2.9's per-output fields as a JSON array, pushed into Lua
/// through the same `Loader::to_lua_value` every capability's `StateSnapshot` goes through
/// (ADR-0041 decision 2: Renderer-sourced, but not a second marshalling path).
pub(super) fn screens_payload(screens: &[Screen]) -> serde_json::Value {
    serde_json::Value::Array(
        screens
            .iter()
            .map(|screen| {
                serde_json::json!({
                    "name": screen.name,
                    "width": screen.width,
                    "height": screen.height,
                    "scale": screen.scale,
                    "refresh": screen.refresh,
                })
            })
            .collect(),
    )
}
/// The same screen list `layout::instance` needs: a name to match `monitor` against, a size to
/// seed `available` with.
pub(super) fn geometries_from(screens: &[Screen]) -> Vec<OutputGeometry> {
    screens
        .iter()
        .map(|screen| OutputGeometry {
            name: screen.name.clone(),
            size: layout::LogicalSize { width: screen.width as f32, height: screen.height as f32 },
        })
        .collect()
}

impl App {
    /// Every connected output as [`Screen`], skipping (with a log) any whose size `wl_output`
    /// cannot answer for. `departing` is the output an `output_destroyed` event is announcing,
    /// excluded by hand: SCTK's `remove_global` calls `OutputHandler::output_destroyed` before
    /// removing it from its own `OutputState`, so `outputs()` here still lists it. `None` else.
    pub(super) fn screens(&self, departing: Option<&wl_output::WlOutput>) -> Vec<Screen> {
        let mut screens = Vec::new();
        for (index, output) in self.output_state.outputs().enumerate() {
            if departing == Some(&output) {
                continue;
            }
            let Some(info) = self.output_state.info(&output) else {
                eprintln!("[oblisk-renderer] output {index} advertised no info yet; no surface created on it");
                continue;
            };
            let facts = OutputFacts {
                name: info.name.clone(),
                logical_size: info.logical_size,
                current_mode: info
                    .modes
                    .iter()
                    .find(|mode| mode.current)
                    .map(|mode| (mode.dimensions, mode.refresh_rate)),
                scale_factor: info.scale_factor,
            };
            match screen_entry(index, &facts) {
                Some(screen) => screens.push(screen),
                None => eprintln!(
                    "[oblisk-renderer] output {:?} reports neither a logical size nor a current mode; no surface created on it",
                    info.name.as_deref().unwrap_or("<unnamed>")
                ),
            }
        }
        screens
    }

    /// One `wl_output` appeared, changed, or went away. One event, three jobs.
    ///
    /// The `screens` signal (ADR-0041 decision 2), gated on that push reporting a real change:
    /// `update_output` also fires for things `screens` does not carry, and re-running the rest for
    /// one of those would ask the Supervisor for an unjustified reload. Then the instance set
    /// (ADR-0038 decision 3): a `monitor = "All"` declaration expands to one instance per output,
    /// so appearing or leaving adds or removes one in place, no generation swap, since plugging
    /// in a monitor is not a config edit. Finally
    /// [`crate::socket::RendererClient::request_reload`], the half this cannot do itself: a config
    /// looping over `screens` declares different surface ids before and after, a topology change
    /// and so a generation swap (ADR-0041 decision 3), decided only by the Supervisor; a candidate
    /// builds its own surface set from its own evaluation, so the two do not conflict.
    ///
    /// ponytail: a hotplug inside a PBA Candidate's own ready window is not handled.
    /// `maybe_send_ready_signal` announces surfaces once, so one added after would trip
    /// `PbaFailure::UnexpectedEvidence`, and a `RequestReload` while draining `inbound_frames` is
    /// skipped by `SocketCandidateLink::recv_matching`. Window: `PBA_TIMINGS`'s seconds. Fix: defer
    /// like `apply_visibility` defers `visible`; not built until this is actually hit.
    fn handle_output_change(&mut self, qh: &QueueHandle<App>, departing: Option<&wl_output::WlOutput>) {
        let screens = self.screens(departing);
        if !self.client.set_screens(screens_payload(&screens)) || !self.startup_complete {
            // Pushed either way: seeding it from the initial output burst is the point (see
            // `App::startup_complete`), but nothing below it applies yet.
            return;
        }
        eprintln!(
            "[oblisk-renderer] outputs changed: {:?}",
            screens.iter().map(|s| s.name.as_str()).collect::<Vec<_>>()
        );

        let specs = self.client.applied_surface_specs();
        let fresh = expand_instances(&specs, &geometries_from(&screens));
        let reconcile = reconcile_instances(self.client.instances(), &fresh);

        for instance_id in &reconcile.removed {
            self.destroy_surface_by_id(instance_id);
        }
        // A surviving panel's `output_size` is what `SizeMode::Percent` resolves against, so a
        // resize must move it: `fresh` has the output's current logical size, the instance set the
        // size the compositor configured (see `reconcile_instances`). `window` has none: § 6.2.
        for instance in &fresh {
            if let Some(TrackedRole::Panel { output_size, .. }) =
                self.surfaces.iter_mut().find(|s| s.surface_id == instance.instance_id).map(|s| &mut s.role)
            {
                *output_size = instance.available;
            }
        }
        // Before `create_surfaces`, which reads the scene by instance id for a new surface's
        // `visible`.
        self.client.set_instances(reconcile.instances);
        self.create_surfaces(qh, &specs, &reconcile.added);
        self.client.request_reload();
    }
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time
    }

    /// § 15.3 point 4: the compositor confirmed `surface`'s committed frame physically hit the
    /// screen. Queues a `shared::PresentationEvidence` frame for the socket thread to write.
    fn presented(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _feedback: &wp_presentation_feedback::WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
        _outputs: Vec<wl_output::WlOutput>,
        _time: PresentTime,
        _refresh: u32,
        _seq: u64,
        _flags: WEnum<wp_presentation_feedback::Kind>,
    ) {
        let Some(nonce) = self.active_nonce else {
            eprintln!("[oblisk-renderer] presented event arrived with no active ActivateDraw nonce; dropping");
            return;
        };
        let Some(surface_id) = self.surface_id_for(surface).map(str::to_string) else {
            eprintln!("[oblisk-renderer] presented event for an untracked surface; dropping");
            return;
        };
        if let Err(e) =
            self.outbound_tx.send(RendererFrame::PresentationEvidence(PresentationEvidence { nonce, surface_id }))
        {
            eprintln!("[oblisk-renderer] failed to queue PresentationEvidence for the socket thread: {e}");
        }
    }

    /// The content update was never displayed. Logged only: the Supervisor's `evidence_timeout`
    /// catches this surface never presenting (ADR-0025). Does **not** queue `PresentationEvidence`.
    fn discarded(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _feedback: &wp_presentation_feedback::WpPresentationFeedback,
        surface: &wl_surface::WlSurface,
    ) {
        let label = self.surface_id_for(surface).unwrap_or("<untracked surface>");
        eprintln!("[oblisk-renderer] presentation feedback discarded for {label}");
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface, _time: u32) {}

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    // All three: update the `screens` signal, then ask for a re-evaluation (ADR-0041 decisions 2
    // and 4). See [`App::handle_output_change`].
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.handle_output_change(qh, None);
    }

    // Not only mode/scale changes: SCTK also routes an output's *first* `xdg_output` arrival here,
    // not to `new_output`, when the `wl_output` was already known.
    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.handle_output_change(qh, None);
    }

    fn output_destroyed(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Passed through explicitly: SCTK's `remove_global` calls this before removing the output
        // from its own `OutputState`, so `outputs()` here still lists it (see [`App::screens`]).
        self.handle_output_change(qh, Some(&output));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(name: Option<&str>) -> OutputFacts {
        OutputFacts {
            name: name.map(str::to_string),
            logical_size: Some((1920, 1080)),
            current_mode: Some(((1920, 1080), 60_000)),
            scale_factor: 1,
        }
    }

    #[test]
    fn a_screens_entry_reports_the_logical_size_in_preference_to_the_current_modes_dimensions() {
        // A 3840x2160 panel driven at scale 2 is 1920x1080 of compositor space, which is the
        // coordinate system a layer surface's own geometry is in -- so the mode's raw dimensions
        // would put a config's own arithmetic on a different grid than the engine's.
        let mut facts = facts(Some("eDP-1"));
        facts.logical_size = Some((1920, 1080));
        facts.current_mode = Some(((3840, 2160), 60_000));
        facts.scale_factor = 2;

        let screen = screen_entry(0, &facts).expect("a logical size is enough on its own");
        assert_eq!((screen.width, screen.height), (1920, 1080));
        assert_eq!(screen.scale, 2);
    }

    #[test]
    fn a_screens_entry_falls_back_to_the_current_modes_dimensions_when_no_logical_size_is_reported() {
        // A compositor below wl_output v4, or one that has not sent an xdg_output yet.
        let mut facts = facts(Some("eDP-1"));
        facts.logical_size = None;
        facts.current_mode = Some(((1366, 768), 60_000));

        let screen = screen_entry(0, &facts).expect("the current mode is the documented fallback");
        assert_eq!((screen.width, screen.height), (1366, 768));
    }

    #[test]
    fn an_output_reporting_neither_a_logical_size_nor_a_current_mode_yields_no_screen_at_all() {
        // Not defaulted to some invented size: every surface on that monitor would then resolve
        // against a fiction, and the caller logs the miss instead.
        let mut facts = facts(Some("eDP-1"));
        facts.logical_size = None;
        facts.current_mode = None;

        assert!(screen_entry(0, &facts).is_none());
    }

    #[test]
    fn refresh_reaches_lua_in_hertz_although_wl_output_reports_millihertz() {
        assert_eq!(screen_entry(0, &facts(Some("eDP-1"))).unwrap().refresh, 60.0);

        // A real 144Hz panel's advertised rate is not a round number, so the division must keep
        // its fraction rather than truncating to an integer.
        let mut odd = facts(Some("DP-1"));
        odd.current_mode = Some(((2560, 1440), 143_868));
        assert_eq!(screen_entry(0, &odd).unwrap().refresh, 143.868);
    }

    #[test]
    fn a_screen_sized_from_its_logical_size_alone_reports_a_refresh_of_zero() {
        // `Mode`'s own docs allow a zero refresh rate for a virtual output, so zero is already this
        // field's "no real answer" value, and an output with no current mode reads the same way.
        let mut facts = facts(Some("HEADLESS-1"));
        facts.current_mode = None;
        assert_eq!(screen_entry(0, &facts).unwrap().refresh, 0.0);
    }

    #[test]
    fn an_unnamed_output_takes_its_positional_id_so_the_shell_still_works_below_wl_output_v4() {
        let screen = screen_entry(2, &facts(None)).unwrap();
        assert_eq!(screen.name, "output-2");
    }

    #[test]
    fn the_screens_payload_is_the_array_of_field_tables_a_config_loops_over() {
        let screens = [screen_entry(0, &facts(Some("eDP-1"))).unwrap(), screen_entry(1, &facts(Some("DP-1"))).unwrap()];

        assert_eq!(
            screens_payload(&screens),
            serde_json::json!([
                { "name": "eDP-1", "width": 1920, "height": 1080, "scale": 1, "refresh": 60.0 },
                { "name": "DP-1", "width": 1920, "height": 1080, "scale": 1, "refresh": 60.0 },
            ])
        );
    }

    #[test]
    fn instance_expansion_reads_the_same_screen_list_the_signal_does() {
        // One source, two consumers (ADR-0041 decision 2): a `monitor` match and a `screens`
        // entry must never be able to disagree about which monitors exist or how large they are.
        let screens = [screen_entry(0, &facts(Some("eDP-1"))).unwrap()];
        assert_eq!(
            geometries_from(&screens),
            [OutputGeometry { name: "eDP-1".to_string(), size: layout::LogicalSize { width: 1920.0, height: 1080.0 } }]
        );
    }
}
