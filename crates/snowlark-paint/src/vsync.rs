// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! The display's vsync, as Chromium reads it on Windows (`ui/gl/vsync_thread_win_dxgi.cc`,
//! `ui/gl/vsync_provider_win.cc`): a wait for the primary monitor's next vertical blank, and that
//! monitor's refresh interval. Chromium drives every begin frame from the primary monitor
//! (`ExternalBeginFrameSourceWin::SetVSyncDisplayID`: "See if we should use non-primary displays").

use std::time::{Duration, Instant};

use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, HWND, POINT};
use windows::Win32::Graphics::Dwm::{DwmGetCompositionTimingInfo, DWM_TIMING_INFO};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsW, GetMonitorInfoW, MonitorFromPoint, DEVMODEW, ENUM_CURRENT_SETTINGS,
    HMONITOR, MONITORINFO, MONITORINFOEXW, MONITOR_DEFAULTTOPRIMARY,
};

use crate::timer_resolution::HighResWaits;

/// A wait shorter than this did not wait for a vertical blank (`kVBlankIntervalThreshold`):
/// `WaitForVBlank` returns at once when the monitor sleeps or the desktop is occluded.
const VBLANK_WAIT_THRESHOLD: Duration = Duration::from_millis(1);

/// The fallback interval (`BeginFrameArgs::DefaultInterval()`).
pub const DEFAULT_INTERVAL: Duration = Duration::from_nanos(16_666_667);

/// Waits for the primary monitor's vertical blanks (`VSyncThreadWinDXGI`).
#[derive(Default)]
pub struct VBlank {
    /// The DXGI output of the primary monitor, kept while it stays usable.
    output: Option<IDXGIOutput>,
}

impl VBlank {
    pub fn new() -> VBlank {
        VBlank::default()
    }

    /// Waits for the next vsync (`VSyncThreadWin::WaitForVSync`): the primary monitor's next
    /// vertical blank, or, when that wait fails or returns at once, a sleep of the refresh interval.
    /// Returns the refresh interval, and whether it slept.
    pub fn wait(&mut self) -> (Duration, bool) {
        let interval = refresh_interval();
        let start = Instant::now();
        let waited = self.wait_for_vblank();
        let slept = !waited || start.elapsed() < VBLANK_WAIT_THRESHOLD;
        if slept {
            // (`Sleep(vsync_interval.InMillisecondsRoundedUp())` on a high resolution timer)
            let mut high_res = HighResWaits::new();
            high_res.set(true);
            std::thread::sleep(Duration::from_millis(
                interval.as_nanos().div_ceil(1_000_000) as u64,
            ));
        }
        (interval, slept)
    }

    /// `VSyncThreadWinDXGI::WaitForVSyncImpl`: drops the kept output when its factory is no longer
    /// current or it is no longer on the primary monitor, finds it again, and waits on it.
    fn wait_for_vblank(&mut self) -> bool {
        let monitor = primary_monitor();
        if let Some(output) = &self.output {
            if !factory_is_current(output) || !output_is_on(output, monitor) {
                self.output = None;
            }
        }
        if self.output.is_none() {
            self.output = output_for(monitor);
        }
        let Some(output) = &self.output else {
            return false;
        };
        // SAFETY: a call on a live DXGI output; it blocks this thread until the vertical blank.
        unsafe { output.WaitForVBlank() }.is_ok()
    }
}

/// The primary monitor (`MonitorFromWindow(nullptr, MONITOR_DEFAULTTOPRIMARY)`).
fn primary_monitor() -> HMONITOR {
    // SAFETY: no pointers; the point (0, 0) is on the primary monitor by definition, and the flag
    // falls back to it anyway.
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

/// `DXGIFactoryIsCurrent`: an output of an adapter whose factory is stale fails its waits.
fn factory_is_current(output: &IDXGIOutput) -> bool {
    // SAFETY: COM calls on live interfaces; each returns a new reference or an error.
    unsafe {
        let Ok(adapter) = output.GetParent::<IDXGIAdapter>() else {
            return false;
        };
        let Ok(factory) = adapter.GetParent::<IDXGIFactory1>() else {
            return false;
        };
        factory.IsCurrent().as_bool()
    }
}

/// `DXGIOutputIsOnMonitor`.
fn output_is_on(output: &IDXGIOutput, monitor: HMONITOR) -> bool {
    // SAFETY: a COM call on a live output, which fills a plain struct.
    unsafe { output.GetDesc() }.is_ok_and(|d| d.Monitor == monitor)
}

/// `FindDXGIOutputForMonitor`: every adapter's outputs, for the one on `monitor`.
fn output_for(monitor: HMONITOR) -> Option<IDXGIOutput> {
    // SAFETY: COM calls on live interfaces; enumeration ends with DXGI_ERROR_NOT_FOUND.
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
        for a in 0.. {
            // (DXGI_ERROR_NOT_FOUND past the last one)
            let adapter = factory.EnumAdapters1(a).ok()?;
            for o in 0.. {
                match adapter.EnumOutputs(o) {
                    Ok(output) => {
                        if output_is_on(&output, monitor) {
                            return Some(output);
                        }
                    }
                    Err(_) => break,
                }
            }
        }
        None
    }
}

/// The primary monitor's refresh interval (`VSyncProviderWin::GetVSyncIntervalIfAvailable`, as
/// Chromium reads it on Windows 11 after 22H2): from the display configuration's refresh rate,
/// which keeps fractions; else DWM's composition rate; else the display settings' whole hertz; else
/// 60 Hz.
pub fn refresh_interval() -> Duration {
    let monitor = primary_monitor();
    let Some(device) = device_name(monitor) else {
        return dwm_interval().unwrap_or(DEFAULT_INTERVAL);
    };
    display_config_interval(&device)
        .or_else(dwm_interval)
        .or_else(|| display_settings_interval(&device))
        .unwrap_or(DEFAULT_INTERVAL)
}

/// The GDI device name of `monitor` (`\\.\DISPLAYn`).
fn device_name(monitor: HMONITOR) -> Option<[u16; 32]> {
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    // SAFETY: `info` is a MONITORINFOEXW with its size set, which the call may fill as such.
    let ok = unsafe { GetMonitorInfoW(monitor, &mut info.monitorInfo) }.as_bool();
    ok.then_some(info.szDevice)
}

/// `TryGetVsyncIntervalFromDisplayConfig`: the active display path whose source is `device`
/// (`display::win::GetDisplayConfigPathInfo`), and its target's refresh rate.
fn display_config_interval(device: &[u16; 32]) -> Option<Duration> {
    let (mut paths_len, mut modes_len) = (0u32, 0u32);
    // SAFETY: two out-parameters the call writes.
    let sized = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths_len, &mut modes_len)
    };
    if sized != ERROR_SUCCESS {
        return None;
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); paths_len as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); modes_len as usize];
    // SAFETY: the arrays have the lengths the counts say; the call writes at most that many and
    // sets the counts to what it wrote.
    let queried = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths_len,
            paths.as_mut_ptr(),
            &mut modes_len,
            modes.as_mut_ptr(),
            None,
        )
    };
    if queried != ERROR_SUCCESS {
        return None;
    }
    paths.truncate(paths_len as usize);
    let len = device.iter().position(|&c| c == 0).unwrap_or(device.len());
    for path in &paths {
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the header starts a DISPLAYCONFIG_SOURCE_DEVICE_NAME of the size it gives.
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }
        let name = &source.viewGdiDeviceName;
        let n = name.iter().position(|&c| c == 0).unwrap_or(name.len());
        if name[..n] != device[..len] {
            continue;
        }
        let rate = path.targetInfo.refreshRate;
        if rate.Numerator != 0 && rate.Denominator != 0 {
            let micros = 1e6 * f64::from(rate.Denominator) / f64::from(rate.Numerator);
            return Some(Duration::from_micros(micros.round() as u64));
        }
    }
    None
}

/// `TryGetVSyncParamsFromDwmCompInfo`'s interval from `rateRefresh` (`qpcRefreshPeriod` is noisy).
fn dwm_interval() -> Option<Duration> {
    let mut timing = DWM_TIMING_INFO {
        cbSize: std::mem::size_of::<DWM_TIMING_INFO>() as u32,
        ..Default::default()
    };
    // SAFETY: a struct of the size its `cbSize` gives, which the call fills.
    unsafe { DwmGetCompositionTimingInfo(HWND::default(), &mut timing) }.ok()?;
    let rate = timing.rateRefresh;
    (rate.uiNumerator > 0 && rate.uiDenominator > 0).then(|| {
        Duration::from_micros(
            u64::from(rate.uiDenominator) * 1_000_000 / u64::from(rate.uiNumerator),
        )
    })
}

/// `TryGetVSyncIntervalFromDisplaySettings`: whole hertz only.
fn display_settings_interval(device: &[u16; 32]) -> Option<Duration> {
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..Default::default()
    };
    // SAFETY: the name is NUL-terminated (from GetMonitorInfoW) and `mode` has its size set.
    let ok = unsafe {
        EnumDisplaySettingsW(
            windows::core::PCWSTR(device.as_ptr()),
            ENUM_CURRENT_SETTINGS,
            &mut mode,
        )
    }
    .as_bool();
    (ok && mode.dmDisplayFrequency > 1)
        .then(|| Duration::from_secs_f64(1.0 / f64::from(mode.dmDisplayFrequency)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_primary_monitor_has_a_refresh_interval() {
        // (any machine with a display: between 24 Hz and 1000 Hz)
        let interval = refresh_interval();
        eprintln!("refresh interval {interval:?}, by DWM {:?}", dwm_interval());
        assert!(
            (Duration::from_millis(1)..=Duration::from_millis(42)).contains(&interval),
            "{interval:?}"
        );
    }

    #[test]
    fn waits_last_about_a_refresh_interval() {
        // A vblank wait, or the sleep that stands in for one, never returns at once: ten waits take
        // about ten intervals.
        let mut vblank = VBlank::new();
        let (interval, _) = vblank.wait();
        let start = Instant::now();
        for _ in 0..10 {
            vblank.wait();
        }
        let ten = start.elapsed();
        assert!(
            ten >= interval * 8 && ten <= interval * 10 + Duration::from_millis(50),
            "{ten:?} for 10 waits of {interval:?}"
        );
    }
}
