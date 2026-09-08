//! Connected outputs (`wl_output`), the `screens` payload, and presentation feedback.
//! `Screen`/`OutputFacts` feed both the Lua signal and `layout::instance` monitor matching
//! (ADR-0041 decision 2).

use super::*;
use crate::wayland::surface::TrackedRole;

/// One connected output, the source for the `screens` signal and `monitor` matching
/// (ADR-0041 decision 2).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Screen {
    name: String,
    width: i32,
    height: i32,
    scale: i32,
    /// Hz; `wl_output::mode` reports millihertz, so conversion happens once here.
    refresh: f64,
}
/// The `OutputInfo` fields [`screen_entry`] reads, copied by [`App::screens`] because SCTK's type
/// is `#[non_exhaustive]` and has no public constructor for unit tests.
struct OutputFacts {
    name: Option<String>,
    logical_size: Option<(i32, i32)>,
    /// The current `Mode`'s `(dimensions, refresh_rate)`, or `None`; the pair stays coherent.
    current_mode: Option<((i32, i32), i32)>,
    scale_factor: i32,
}
/// Floor under [`texture_budget`], and what that budget was before it was derived: enough for one
/// config's closed picker plus a 1920x1200 wallpaper (ADR-0123). It keeps a small display, or one
/// whose size no output has reported yet, from getting a tighter budget than the constant did.
const MIN_TEXTURE_BUDGET: usize = 16 << 20;

/// The `ImageCache` idle-texture budget for this machine's displays (ADR-0182): one screenful of
/// RGBA per output, floored at [`MIN_TEXTURE_BUDGET`].
///
/// A constant cannot be right for an engine other people's shells run on. 16 MB was measured
/// against one 1920x1200 laptop and one config's 54-file picker, and a single 4K wallpaper is 33 MB
/// on its own -- so that machine would sit permanently over budget while this one has room to
/// spare. Every texture this cache holds is sized by the box it is drawn into, so display geometry
/// is the one thing in reach that scales with the working set.
///
/// One screenful per output is the largest single image a shell draws, times the number of places
/// it can draw one, which is what a config changing every wallpaper at once asks for. Physical
/// pixels, not logical: that is what `ImageCache` keys on.
pub(super) fn texture_budget(screens: &[Screen]) -> usize {
    let screenful = |screen: &Screen| {
        let scale = screen.scale.max(1) as usize;
        let (width, height) = (screen.width.max(0) as usize, screen.height.max(0) as usize);
        width.saturating_mul(scale).saturating_mul(height.saturating_mul(scale)).saturating_mul(4)
    };
    screens.iter().map(screenful).sum::<usize>().max(MIN_TEXTURE_BUDGET)
}

/// One `screens` entry, or `None` if size is unknown. Prefer `logical_size` (`xdg_output`/
/// `wl_output` v4 compositor space), then the current `Mode` dimensions; never invent a size.
/// ponytail: below `wl_output` v4, nameless outputs use positional `"output-{index}"` ids, so
/// `monitor = "DP-1"` cannot match; no client-side upgrade exists without a compositor name.
fn screen_entry(index: usize, facts: &OutputFacts) -> Option<Screen> {
    let (width, height) = facts.logical_size.or_else(|| facts.current_mode.map(|(dimensions, _)| dimensions))?;
    Some(Screen {
        name: facts.name.clone().unwrap_or_else(|| format!("output-{index}")),
        width,
        height,
        scale: facts.scale_factor,
        // `Mode` allows zero when an output has no correct refresh rate, such as a virtual output.
        refresh: facts.current_mode.map_or(0.0, |(_, rate)| f64::from(rate) / 1000.0),
    })
}
/// § 2.9's per-output fields as a JSON array, pushed through the same `Loader::to_lua_value` as
/// capability `StateSnapshot`s (ADR-0041 decision 2).
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
/// The same names and sizes `layout::instance` uses for `monitor` matching and `available`.
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
    /// Every connected output as [`Screen`]. A departing output is excluded because SCTK calls
    /// `output_destroyed` before removing it from `OutputState`, so `outputs()` still lists it.
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

    /// Handles an output appearing, changing, or leaving: update `screens` only when its payload
    /// changed. `update_output` also fires for things `screens` does not carry; re-running the
    /// rest for one would ask the Supervisor for an unjustified reload. Reconcile `monitor = "All"`
    /// instances in place (ADR-0038 decision 3), then ask the Supervisor for a generation swap
    /// when a config's `screens` loop changes surface ids
    /// (ADR-0041 decisions 2-3). Candidates build their own set, so the two paths do not conflict.
    ///
    /// ponytail: hotplug inside a PBA Candidate's ready window is unsupported. A late surface would
    /// trip `PbaFailure::UnexpectedEvidence` after the one-shot `maybe_send_ready_signal`; then
    /// `RequestReload` is skipped while `SocketCandidateLink::recv_matching` drains
    /// `inbound_frames`. Window: `PBA_TIMINGS` seconds.
    /// Upgrade: defer like `apply_visibility` defers `visible`, when this is hit.
    fn handle_output_change(&mut self, qh: &QueueHandle<App>, departing: Option<&wl_output::WlOutput>) {
        let screens = self.screens(departing);
        // Before the early return: an output arriving during the startup burst changes what the
        // cache may hold even though nothing else here runs yet.
        self.image_cache.set_texture_budget(texture_budget(&screens));
        if !self.client.set_screens(screens_payload(&screens)) || !self.startup_complete {
            // Seed from the initial output burst; `startup_complete` gates the rest.
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
        // Percent resolves against the output's logical size, not the compositor-configured panel
        // size. Windows have no output size (§ 6).
        for instance in &fresh {
            if let Some(TrackedRole::Panel { output_size, .. }) =
                self.surfaces.iter_mut().find(|s| s.surface_id == instance.instance_id).map(|s| &mut s.role)
            {
                *output_size = instance.available;
            }
        }
        // Before `create_surfaces`, which reads the scene for a new surface's `visible`.
        self.client.set_instances(reconcile.instances);
        self.create_surfaces(qh, &specs, &reconcile.added);
        self.client.request_reload();
    }
}

impl PresentationTimeHandler for App {
    fn presentation_time_state(&mut self) -> &mut PresentationTimeState {
        &mut self.presentation_time
    }

    /// § 14.2 evidence: the compositor confirmed `surface`'s committed frame reached the screen;
    /// queue `shared::PresentationEvidence` for the socket thread.
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

    /// The update was never displayed. Log only; `evidence_timeout` catches it (ADR-0025), so do
    /// not queue `PresentationEvidence`.
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

    /// A frame callback `App::paint_surface` requested because that surface's tree was mid-tween
    /// (ADR-0145). One flag for every surface: the poll loop ticks the whole scene, and a second
    /// output's callback in the same turn is absorbed by it.
    fn frame(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _surface: &wl_surface::WlSurface, _time: u32) {
        self.animation_frame_due = true;
    }

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

    // Update `screens`, then ask for a re-evaluation (ADR-0041 decisions 2 and 4).
    fn new_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.handle_output_change(qh, None);
    }

    // SCTK also routes an output's first `xdg_output` here, not to `new_output`.
    fn update_output(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: wl_output::WlOutput) {
        self.handle_output_change(qh, None);
    }

    fn output_destroyed(&mut self, _: &Connection, qh: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // `remove_global` calls this before removing the output from SCTK's `OutputState`.
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

    /// ADR-0182. The budget is a property of the displays, so a bigger or a second screen gets a
    /// bigger one, and the floor keeps a small or not-yet-reported display from getting less than
    /// the constant this replaced.
    #[test]
    fn the_texture_budget_follows_the_displays_rather_than_a_constant() {
        let screen = |width, height, scale| Screen { name: "TEST".to_string(), width, height, scale, refresh: 60.0 };
        assert_eq!(texture_budget(&[]), MIN_TEXTURE_BUDGET, "no output yet is the floor, not zero");
        assert_eq!(
            texture_budget(&[screen(1920, 1200, 1)]),
            MIN_TEXTURE_BUDGET,
            "one 1920x1200 screenful is 9.2 MB, under the floor the old constant set"
        );

        // 3840x2160 is 33.2 MB of RGBA, which the constant could not hold even once.
        let uhd = texture_budget(&[screen(3840, 2160, 1)]);
        assert_eq!(uhd, 3840 * 2160 * 4);
        assert!(uhd > MIN_TEXTURE_BUDGET);

        // Physical pixels, which is what `ImageCache` keys on: the same panel driven at scale 2
        // reports half the logical size and must come out the same.
        assert_eq!(texture_budget(&[screen(1920, 1080, 2)]), 3840 * 2160 * 4);

        // A config changing every wallpaper at once needs room on every output it draws to.
        assert_eq!(texture_budget(&[screen(3840, 2160, 1), screen(3840, 2160, 1)]), 2 * 3840 * 2160 * 4);
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
