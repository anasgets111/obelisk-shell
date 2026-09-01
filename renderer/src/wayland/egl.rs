use std::ffi::c_void;

use khronos_egl as egl;

/// The attributes of a candidate EGL frame buffer configuration, as reported by
/// `eglGetConfigAttrib`. Kept separate from `egl::Config` so `satisfies_requirements`
/// stays a pure function testable without a live EGL display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigAttribs {
    pub surface_type: egl::Int,
    pub renderable_type: egl::Int,
    pub red_size: egl::Int,
    pub green_size: egl::Int,
    pub blue_size: egl::Int,
    pub alpha_size: egl::Int,
}

/// A config must back an on-screen window surface, render GLES3, and give exactly
/// 8-bit-per-channel ARGB.
///
/// `eglChooseConfig` is supposed to only return matches, but its attribute lists are bitmask
/// supersets and driver behavior around exact-vs-minimum component sizes is inconsistent enough
/// to be worth re-validating directly rather than trusting the first candidate returned.
pub fn satisfies_requirements(attrs: ConfigAttribs) -> bool {
    attrs.surface_type & egl::WINDOW_BIT != 0
        && attrs.renderable_type & egl::OPENGL_ES3_BIT != 0
        && attrs.red_size == 8
        && attrs.green_size == 8
        && attrs.blue_size == 8
        && attrs.alpha_size == 8
}

/// Live EGL state shared by every static surface: one display connection, one config,
/// one GLES3 context. Surfaces are created per `wl_surface` against this shared context.
pub struct EglState {
    pub instance: egl::Instance<egl::Static>,
    pub display: egl::Display,
    pub config: egl::Config,
    pub context: egl::Context,
}

/// Initializes EGL against the Wayland display and picks the first config that
/// genuinely satisfies [`satisfies_requirements`], not just the first one
/// `eglChooseConfig` hands back.
pub fn init(wl_display_ptr: *mut c_void) -> Result<EglState, String> {
    let instance = egl::Instance::new(egl::Static);

    // SAFETY: wl_display_ptr comes from Connection::backend().display_ptr(), a live
    // wl_display for the whole lifetime of this process's Wayland connection.
    let display = unsafe { instance.get_display(wl_display_ptr) }
        .ok_or("eglGetDisplay returned no display for the Wayland connection")?;

    instance.initialize(display).map_err(|e| format!("eglInitialize failed: {e}"))?;

    instance.bind_api(egl::OPENGL_ES_API).map_err(|e| format!("eglBindAPI(EGL_OPENGL_ES_API) failed: {e}"))?;

    let query_attribs = [
        egl::SURFACE_TYPE,
        egl::WINDOW_BIT,
        egl::RENDERABLE_TYPE,
        egl::OPENGL_ES3_BIT,
        egl::RED_SIZE,
        8,
        egl::GREEN_SIZE,
        8,
        egl::BLUE_SIZE,
        8,
        egl::ALPHA_SIZE,
        8,
        egl::NONE,
    ];

    let count = instance
        .matching_config_count(display, &query_attribs)
        .map_err(|e| format!("eglChooseConfig (count) failed: {e}"))?;
    let mut candidates = Vec::with_capacity(count);
    instance
        .choose_config(display, &query_attribs, &mut candidates)
        .map_err(|e| format!("eglChooseConfig failed: {e}"))?;

    let config = candidates
        .into_iter()
        .find(|candidate| {
            let attrs = query_config_attribs(&instance, display, *candidate);
            attrs.is_ok_and(satisfies_requirements)
        })
        .ok_or("no EGL config matches window+GLES3+8-bit-ARGB requirements")?;

    let context_attribs = [egl::CONTEXT_CLIENT_VERSION, 3, egl::NONE];
    let context = instance
        .create_context(display, config, None, &context_attribs)
        .map_err(|e| format!("eglCreateContext (GLES3) failed: {e}"))?;

    Ok(EglState { instance, display, config, context })
}

fn query_config_attribs(
    instance: &egl::Instance<egl::Static>,
    display: egl::Display,
    config: egl::Config,
) -> Result<ConfigAttribs, egl::Error> {
    Ok(ConfigAttribs {
        surface_type: instance.get_config_attrib(display, config, egl::SURFACE_TYPE)?,
        renderable_type: instance.get_config_attrib(display, config, egl::RENDERABLE_TYPE)?,
        red_size: instance.get_config_attrib(display, config, egl::RED_SIZE)?,
        green_size: instance.get_config_attrib(display, config, egl::GREEN_SIZE)?,
        blue_size: instance.get_config_attrib(display, config, egl::BLUE_SIZE)?,
        alpha_size: instance.get_config_attrib(display, config, egl::ALPHA_SIZE)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full_match() -> ConfigAttribs {
        ConfigAttribs {
            surface_type: egl::WINDOW_BIT,
            renderable_type: egl::OPENGL_ES3_BIT,
            red_size: 8,
            green_size: 8,
            blue_size: 8,
            alpha_size: 8,
        }
    }

    #[test]
    fn accepts_exact_window_gles3_argb8888_config() {
        assert!(satisfies_requirements(full_match()));
    }

    #[test]
    fn accepts_config_with_extra_bits_set() {
        // Supporting ES2 as well as ES3, and pbuffer as well as window, still satisfies the mask.
        let mut attrs = full_match();
        attrs.surface_type |= egl::PBUFFER_BIT;
        attrs.renderable_type |= egl::OPENGL_ES2_BIT;
        assert!(satisfies_requirements(attrs));
    }

    #[test]
    fn rejects_missing_window_bit() {
        let mut attrs = full_match();
        attrs.surface_type = egl::PBUFFER_BIT;
        assert!(!satisfies_requirements(attrs));
    }

    #[test]
    fn rejects_missing_gles3_bit() {
        let mut attrs = full_match();
        attrs.renderable_type = egl::OPENGL_ES2_BIT;
        assert!(!satisfies_requirements(attrs));
    }

    #[test]
    fn rejects_non_8_bit_channel() {
        let mut attrs = full_match();
        attrs.red_size = 5;
        assert!(!satisfies_requirements(attrs));

        let mut attrs = full_match();
        attrs.alpha_size = 0;
        assert!(!satisfies_requirements(attrs));
    }
}
