// Windows desktop capture via DXGI Desktop Duplication API.
// Direct driver-level capture that bypasses the Windows.Graphics.Capture
// WinRT abstraction, which has known bugs with window-layer visibility on
// certain Windows builds (e.g. Win10 22H2 + RTX 4060).

use crate::Shared;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext,
    ID3D11Texture2D, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11_CREATE_DEVICE_FLAG,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIDevice, IDXGIFactory1,
    IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Foundation::HMODULE;

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

        let (device, ctx) = match create_d3d11_device() {
            Ok(v) => v,
            Err(e) => {
                eprintln!("capture: D3D11 device creation failed: {e}");
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
    // Use DXGI factory to get the hardware adapter, then create a D3D11
    // device explicitly bound to it. This is more reliable than passing
    // None for the adapter (which has had API-surface issues in the
    // windows 0.62 crate).
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1().map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
        let adapter: IDXGIAdapter = factory.EnumAdapters1(0).map_err(|e| format!("EnumAdapters1(0): {e}"))?;

        let mut device: Option<ID3D11Device> = None;
        let mut ctx: Option<ID3D11DeviceContext> = None;

        D3D11CreateDevice(
            Some(&adapter),
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE(0),
            D3D11_CREATE_DEVICE_FLAG(0x20), // BGRA_SUPPORT
            None,
            0,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut ctx),
        )
        .map_err(|e| format!("D3D11CreateDevice: {e}"))?;

        let device = device.ok_or("null device")?;
        let ctx = ctx.ok_or("null context")?;
        eprintln!("capture: D3D11 device created successfully");
        Ok((device, ctx))
    }
}

fn dxgi_capture_loop(
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
    shared: &Shared,
) -> Result<(), DuplicationError> {
    let idx = shared.lock().unwrap().monitor_index;
    let dup = create_duplication(device, idx)?;

    let mut first = true;

    loop {
        if shared.lock().unwrap().monitor_index != idx {
            return Ok(());
        }

        match acquire_frame(&dup, device, ctx) {
            Ok(Some((w, h, data))) => {
                if first {
                    eprintln!("capture: first frame arrived via DXGI ({w}x{h})");
                    first = false;
                }
                let mut g = shared.lock().unwrap();
                let v = g.version;
                if v.wrapping_rem(60) == 0 {
                    eprintln!("capture: frame #{v} ({w}x{h})");
                }
                g.data = data;
                g.width = w;
                g.height = h;
                g.gpu_index = None;
                g.version = v.wrapping_add(1);
            }
            Ok(None) => {
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
) -> Result<IDXGIOutputDuplication, DuplicationError> {
    unsafe {
        let dxgi_device: IDXGIDevice = device.cast().map_err(|e| {
            DuplicationError::Fatal(format!("cast to IDXGIDevice: {e}"))
        })?;
        let adapter: IDXGIAdapter = dxgi_device.GetAdapter().map_err(|e| {
            DuplicationError::Fatal(format!("GetAdapter: {e}"))
        })?;

        let mut output_idx = 0u32;
        let mut target_output: Option<IDXGIOutput> = None;
        let mut found = 0usize;
        loop {
            match adapter.EnumOutputs(output_idx) {
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
                "monitor {} not found ({} outputs)",
                monitor_index + 1, found
            ))
        })?;

        let output1: IDXGIOutput1 = output.cast().map_err(|e| {
            DuplicationError::Fatal(format!("cast to IDXGIOutput1: {e}"))
        })?;

        let desc = output1.GetDesc().map_err(|e| {
            DuplicationError::Fatal(format!("GetDesc: {e}"))
        })?;

        eprintln!(
            "capture: monitor {} = {} ({}x{})",
            monitor_index + 1,
            String::from_utf16_lossy(&desc.DeviceName),
            desc.DesktopCoordinates.right - desc.DesktopCoordinates.left,
            desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top,
        );

        output1.DuplicateOutput(device).map_err(|e| {
            DuplicationError::Fatal(format!("DuplicateOutput: {e}"))
        })
    }
}

fn acquire_frame(
    dup: &IDXGIOutputDuplication,
    device: &ID3D11Device,
    ctx: &ID3D11DeviceContext,
) -> Result<Option<(u32, u32, Vec<u8>)>, DuplicationError> {
    unsafe {
        let mut frame_info = std::mem::zeroed();
        let mut resource: Option<IDXGIResource> = None;

        match dup.AcquireNextFrame(100, &mut frame_info, &mut resource) {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return Err(DuplicationError::AccessLost),
            Err(e) => {
                return Err(DuplicationError::Fatal(format!(
                    "AcquireNextFrame: {e:?}"
                )));
            }
        }

        let resource = match resource {
            Some(r) => r,
            None => {
                let _ = dup.ReleaseFrame();
                return Ok(None);
            }
        };

        let texture: ID3D11Texture2D = resource.cast().map_err(|e| {
            DuplicationError::Fatal(format!("cast to Texture2D: {e}"))
        })?;

        let mut desc = D3D11_TEXTURE2D_DESC::default();
        texture.GetDesc(&mut desc);

        let width = desc.Width;
        let height = desc.Height;
        let tight_pitch = (width as usize) * 4;

        let staging_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: desc.Format,
            SampleDesc: desc.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: 0x20000,
            MiscFlags: 0,
        };

        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))
            .map_err(|e| {
                DuplicationError::Fatal(format!("CreateTexture2D: {e}"))
            })?;
        let staging = staging.unwrap();

        ctx.CopyResource(&staging, &texture);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        ctx.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
            .map_err(|e| {
                let _ = dup.ReleaseFrame();
                DuplicationError::Fatal(format!("Map: {e:?}"))
            })?;

        let row_pitch = mapped.RowPitch as usize;
        let data_ptr = mapped.pData as *const u8;

        let mut out = vec![0u8; tight_pitch * (height as usize)];
        for y in 0..height as usize {
            let src = std::slice::from_raw_parts(
                data_ptr.add(y * row_pitch),
                tight_pitch,
            );
            out[y * tight_pitch..(y + 1) * tight_pitch].copy_from_slice(src);
        }

        ctx.Unmap(&staging, 0);
        let _ = dup.ReleaseFrame();

        Ok(Some((width, height, out)))
    }
}
