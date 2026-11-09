//! WSLg's native Wayland compositor can crash with winit windows (microsoft/wslg#1386).
//! Use its Xwayland path by default while allowing an explicit native-Wayland retest.

use std::path::Path;

const BACKEND_ENV: &str = "ATOMICS_WINDOW_BACKEND";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    X11,
    Wayland,
}

fn select(requested: Option<&str>, wslg: bool, x11_available: bool) -> Result<Option<Backend>, String> {
    match requested {
        Some("x11") => Ok(Some(Backend::X11)),
        Some("wayland") => Ok(Some(Backend::Wayland)),
        None | Some("auto") => Ok((wslg && x11_available).then_some(Backend::X11)),
        Some(value) => Err(format!("{BACKEND_ENV} must be auto, x11, or wayland; got {value:?}")),
    }
}

pub(crate) fn configure(native: &mut eframe::NativeOptions) -> Result<(), String> {
    let requested = match std::env::var(BACKEND_ENV) {
        Ok(value) => Some(value),
        Err(std::env::VarError::NotPresent) => None,
        Err(std::env::VarError::NotUnicode(_)) => return Err(format!("{BACKEND_ENV} must be auto, x11, or wayland")),
    };
    // These files belong to WSLg's shared system-distro mount, including when a desktop
    // launcher did not inherit WSL_INTEROP. A WSL kernel by itself does not imply WSLg.
    let wslg = Path::new("/mnt/wslg/versions.txt").is_file() && Path::new("/mnt/wslg/runtime-dir").is_dir();
    let x11_available = std::env::var_os("DISPLAY").is_some_and(|value| !value.is_empty());
    let Some(backend) = select(requested.as_deref(), wslg, x11_available)? else { return Ok(()) };
    native.event_loop_builder = Some(Box::new(move |builder| match backend {
        Backend::X11 => {
            use winit::platform::x11::EventLoopBuilderExtX11;
            builder.with_x11();
        }
        Backend::Wayland => {
            use winit::platform::wayland::EventLoopBuilderExtWayland;
            builder.with_wayland();
        }
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_wslg_with_an_x_display_defaults_to_x11() {
        for requested in [None, Some("auto")] {
            assert_eq!(select(requested, true, true).unwrap(), Some(Backend::X11));
            for (wslg, x11) in [(false, true), (true, false), (false, false)] {
                assert_eq!(select(requested, wslg, x11).unwrap(), None);
            }
        }
    }

    #[test]
    fn explicit_backends_allow_retesting_and_reject_typos() {
        for wslg in [false, true] {
            for x11 in [false, true] {
                assert_eq!(select(Some("wayland"), wslg, x11).unwrap(), Some(Backend::Wayland));
                assert_eq!(select(Some("x11"), wslg, x11).unwrap(), Some(Backend::X11));
            }
        }
        for value in ["", "Wayland", "xorg", "bogus"] {
            assert!(select(Some(value), true, true).unwrap_err().contains(BACKEND_ENV));
        }
    }
}
