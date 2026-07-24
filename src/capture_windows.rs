// Windows desktop capture via DXGI Desktop Duplication API.
// Direct driver-level capture that bypasses the Windows.Graphics.Capture
// WinRT abstraction, which has known bugs with window-layer visibility on
// certain Windows builds (e.g. Win10 22H2 + RTX 4060).

use crate::Shared;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CREATE_DEVICE_FLAG, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC,
};
use std::ptr::null;

/// Spawn a background thread that captures the selected monitor using the
/// DXGI Desktop Duplication API (driver-level, captures everything including
/// all application windows).
pub fn start(shared: Shared, monitor_index: usize) {
    std::thread::spawn(move || {
        let mut g = shared.lock().unwrap();
        g.width = 0;
        g.height = 0;
        g.data = Vec::new();
        g.gpu_index = None;
        g.gpu_handles = None;
        g.gpu_disabled = true;
        g.epoch = g.epoch.wrapping_add(1);
        g.monitor_index = monitor_index;
        drop(g);

        eprintln!("capture: initializing DXGI Desktop Duplication for monitor {}", monitor_index + 1);

        // Create D3D11 device
        let (device, ctx) = match create_d3d11_device() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("capture: D3D11CreateDevice failed: {e}");
                return;
            }
        };

        loop {
            match dxgi_capture_loop(&device, &ctx, &shared) {
                Ok(()) => {
                    eprintln!("capture: session ended normally");
                    return;
                }
                Err(DuplicationError::AccessLost) => {
                    eprintln!("capture: DXGI access lost, restarting...");
                    // Resolution change or mode switch; reset state and retry
                    let mut g = shared.lock().unwrap();
                    g.width = 0;
                    g.height = 0;
                    g.data = Vec::new();
                    g.epoch = g.epoch.wrapping_add(1);
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(DuplicationError::Fatal(msg)) => {
                    eprintln!("capture: fatal error: {msg}");
                    return;
                }
            }
        }
    });
}

enum DuplicationError {
    AccessLost,
    Fatal(String),
}

fn create_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext), String> {
    let mut device: Option<ID3D11Device> = None;
    let mut ctx: Option<ID3D11DeviceContext> = None;
    let flags = D3D11_CREATE_DEVICE_FLAG(0); // no debug flag
    unsafe {
        D3D11CreateDevice(
            None, // default adapter
            windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE,
            None, // no software rasterizer
            flags,
            None, // default feature level
            0,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None, // feature level out
            Some(&mut ctx),
        )
    }
    .map_err(|e| format!("{e:?}"))?;
    let device = device.ok_or("D3D11CreateDevice returned null device")?;
    let ctx = ctx.ok_or("D3D11CreateDevice returned null context")?;
    Ok((device, ctx))
}

fn dxgi_capture_loop(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    shared: &Shared,
) -> Result<(), DuplicationError> {
    // Get the DXGI output for the target monitor
    let idx = shared.lock().unwrap().monitor_index;
    let (dup, desc) = create_duplication(device, idx)?;

    let mut scratch: Vec<u8> = Vec::new();
    let mut first = true;
    let mut consecutive_timeouts: u32 = 0;

    loop {
        // Check for monitor switch
        if shared.lock().unwrap().monitor_index != idx {
            return Ok(());
        }

        match acquire_frame(&dup, device, ctx, &mut scratch) {
            Ok(Some((w, h, data))) => {
                consecutive_timeouts = 0;
                if first {
                    eprintln!("capture: first frame arrived via DXGI ({w}x{h})");
                    first = false;
                }
                let mut g = shared.lock().unwrap();
                let v = g.version;
                // diagnostic: log every 60 frames
                if v % 60 == 0 {
                    eprintln!("capture: frame #{v} ({w}x{h})");
                }
                g.data = data;
                g.width = w;
                g.height = h;
                g.gpu_index = None;
                g.version = v.wrapping_add(1);
            }
            Ok(None) => {
                // No new frame (timeout); briefly yield CPU
                consecutive_timeouts += 1;
                if consecutive_timeouts > 300 {
                    // ~30s with no frames; screen might be static, that's fine
                    consecutive_timeouts = 0;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(DuplicationError::AccessLost) => return Err(DuplicationError::AccessLost),
            Err(e) => return Err(e),
        }
    }
}

fn create_duplication(
    device: &ID3D11Device,
    monitor_index: usize,
) -> Result<(IDXGIOutputDuplication, DXGI_OUTDUPL_DESC), DuplicationError> {
    unsafe {
        // Get the DXGI device from D3D11 device
        let dxgi_device: IDXGIDevice = device.cast().map_err(|e| {
            DuplicationError::Fatal(format!("failed to cast to IDXGIDevice: {e}"))
        })?;
        let adapter: IDXGIAdapter = dxgi_device.GetAdapter().map_err(|e| {
            DuplicationError::Fatal(format!("GetAdapter failed: {e}"))
        })?;

        // Enumerate outputs
        let mut output_idx = 0u32;
        let mut target_output: Option<IDXGIOutput> = None;
        let mut found = 0usize;
        loop {
            let output = adapter.EnumOutputs(output_idx);
            match output {
                Ok(o) => {
                    if found == monitor_index {
                        target_output = Some(o);
                        break;
                    }
                    found += 1;
                    output_idx += 1;
                }
                Err(_) => break,
            }
        }

        let output = target_output.ok_or_else(|| {
            DuplicationError::Fatal(format!(
                "monitor {} not found ({} outputs enumerated)",
                monitor_index + 1,
                found
            ))
        })?;

        let output1: IDXGIOutput1 = output.cast().map_err(|e| {
            DuplicationError::Fatal(format!("failed to cast to IDXGIOutput1: {e}"))
        })?;

        let desc = output1.GetDesc().map_err(|e| {
            DuplicationError::Fatal(format!("GetDesc failed: {e}"))
        })?;

        eprintln!(
            "capture: monitor {} = {} ({}x{})",
            monitor_index + 1,
            String::from_utf16_lossy(&desc.DeviceName),
            desc.DesktopCoordinates.right - desc.DesktopCoordinates.left,
            desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top,
        );

        let dup = output1.DuplicateOutput(device).map_err(|e| {
            DuplicationError::Fatal(format!("DuplicateOutput failed: {e}"))
        })?;

        let dup_desc = dup.GetDesc();
        Ok((dup, dup_desc))
    }
}

fn acquire_frame(
    dup: &IDXGIOutputDuplication,
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    scratch: &mut Vec<u8>,
) -> Result<Option<(u32, u32, Vec<u8>)>, DuplicationError> {
    unsafe {
        let mut frame_info = std::mem::zeroed();
        let mut resource: Option<IDXGIResource> = None;

        let hr = dup.AcquireNextFrame(
            100, // timeout in ms
            &mut frame_info,
            &mut resource,
        );

        match hr {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(DuplicationError::AccessLost),
            Err(e) => {
                return Err(DuplicationError::Fatal(format!(
                    "AcquireNextFrame failed: {e:?}"
                )));
            }
        }

        if resource.is_none() {
            let _ = dup.ReleaseFrame();
            return Ok(None);
        }

        let resource = resource.unwrap();
        let texture: ID3D11Texture2D = resource.cast().map_err(|e| {
            DuplicationError::Fatal(format!("failed to cast frame to ID3D11Texture2D: {e}"))
        })?;

        let desc = std::mem::zeroed();
        texture.GetDesc(&mut std::mem::transmute(&desc));

        let width = desc.Width;
        let height = desc.Height;
        let expected_len = (width as usize) * (height as usize) * 4;

        // Create staging texture if size changed
        let mut staging_desc = desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ;
        staging_desc.MiscFlags = 0;

        let mut staging: Option<ID3D11Texture2D> = None;
        device
            .CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .map_err(|e| {
                DuplicationError::Fatal(format!("CreateTexture2D (staging) failed: {e}"))
            })?;
        let staging = staging.unwrap();

        ctx.CopyResource(&staging, &texture);

        let mapped = ctx.Map(
            &staging,
            0,
            D3D11_MAP_READ,
            0, // no map flags
        );

        match mapped {
            Ok(mapped) => {
                let row_pitch = mapped.RowPitch as usize;
                let data_ptr = mapped.pData as *const u8;

                let tight_pitch = (width as usize) * 4;
                if scratch.len() != tight_pitch * (height as usize) {
                    *scratch = vec![0u8; tight_pitch * (height as usize)];
                }

                // Copy row by row, handling stride
                for y in 0..height as usize {
                    let src_row = std::slice::from_raw_parts(
                        data_ptr.add(y * row_pitch),
                        tight_pitch,
                    );
                    scratch[y * tight_pitch..(y + 1) * tight_pitch].copy_from_slice(src_row);
                }

                ctx.Unmap(&staging, 0);
                let _ = dup.ReleaseFrame();

                Ok(Some((width, height, scratch.clone())))
            }
            Err(e) => {
                let _ = dup.ReleaseFrame();
                Err(DuplicationError::Fatal(format!("Map failed: {e:?}")))
            }
        }
    }
}
