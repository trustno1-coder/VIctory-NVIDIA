//! Original, deterministic SR fixture. No title inputs; only the check command uses it.
use super::*;
mod io;
use io::{readback, upload};
use std::time::Instant;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Validate SR in one process, with bounded warm-up and steady-state measurements.
pub fn verified_check(output: [u32; 2], iterations: u32) -> Result<Vec<String>, String> {
    if output.iter().any(|&n| n == 0 || n > 7680) || !(4..=48).contains(&iterations) {
        return Err("validation requires nonzero dimensions <=7680 and 4..48 iterations".into());
    }
    let result = run(output, iterations);
    let shutdown = shut_down();
    match (result, shutdown) {
        (Ok(mut lines), Some(line)) if line == "Streamline shut down: ok (0)" => {
            lines.push(line);
            Ok(lines)
        }
        (Ok(_), line) => Err(format!("validation shutdown failed: {line:?}")),
        (Err(why), line) => Err(format!("{why}; shutdown: {line:?}")),
    }
}

fn run(output: [u32; 2], iterations: u32) -> Result<Vec<String>, String> {
    let began = Instant::now();
    let mut asked = Asked::from_env()?;
    asked.dlss = true;
    asked.framegen = false;
    asked.fsr_framegen = false;
    asked.present_hook = true;
    if !load_and_init(&asked, false)?.dlss {
        return Err("DLSS SR unsupported".into());
    }
    let host = crate::host::host().ok_or("no host device")?;
    CHECK_DEVICE.store(true, Relaxed);
    if !DLSS_READY.load(Acquire) {
        return Err("DLSS device not ready".into());
    }
    let init_ms = began.elapsed().as_secs_f64() * 1000.0;
    let mode = match asked.mode {
        Mode::Dlaa => abi::DLSS_DLAA,
        Mode::Quality => abi::DLSS_MAX_QUALITY,
        Mode::Balanced => abi::DLSS_BALANCED,
        Mode::Performance => abi::DLSS_MAX_PERFORMANCE,
        Mode::UltraPerformance => abi::DLSS_ULTRA_PERFORMANCE,
    };
    let optimal = FUNCTIONS
        .get()
        .and_then(|f| f.dlss_optimal)
        .ok_or("no optimal-settings function")?;
    let mut settings = abi::DlssOptimalSettings::default();
    // SAFETY: initialized SDK, valid options and output structure.
    let code = unsafe {
        optimal(
            &abi::DlssOptions::new(mode, output, asked.preset),
            &mut settings,
        )
    };
    let render = [
        settings.optimal_render_width,
        settings.optimal_render_height,
    ];
    if code != 0 || render.contains(&0) || render.iter().any(|&n| n > 7680) {
        return Err(format!(
            "optimal settings failed: code={code}, render={render:?}"
        ));
    }
    calls().optimal.push((output, mode, Some(render)));
    let memory_before = host.video_memory();
    let d = &host.device;
    let colour = texture(
        d,
        render,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        D3D12_RESOURCE_FLAG_NONE,
        COPY_DEST,
    )
    .map_err(err)?;
    let depth = texture(
        d,
        render,
        DXGI_FORMAT_R32_FLOAT,
        D3D12_RESOURCE_FLAG_NONE,
        COPY_DEST,
    )
    .map_err(err)?;
    let motion = texture(
        d,
        render,
        DXGI_FORMAT_R16G16_FLOAT,
        D3D12_RESOURCE_FLAG_NONE,
        COPY_DEST,
    )
    .map_err(err)?;
    let exposure = texture(
        d,
        [1, 1],
        DXGI_FORMAT_R32_FLOAT,
        D3D12_RESOURCE_FLAG_NONE,
        COPY_DEST,
    )
    .map_err(err)?;
    let out = texture(
        d,
        output,
        DXGI_FORMAT_R16G16B16A16_FLOAT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        COPY_DEST,
    )
    .map_err(err)?;
    let list = host.new_list().map_err(err)?;
    let mut uploads = Vec::new();
    uploads.push(upload(
        d,
        &list.list,
        &colour,
        8,
        SHADER_READ,
        |x, _, pixel| {
            let values: [u16; 4] = if x < render[0] / 2 {
                [0x3000, 0x3400, 0x3800, 0x3c00]
            } else {
                [0x3a00, 0x3800, 0x3400, 0x3c00]
            };
            for (dst, value) in pixel.chunks_exact_mut(2).zip(values) {
                dst.copy_from_slice(&value.to_le_bytes());
            }
        },
    )?);
    uploads.push(upload(d, &list.list, &depth, 4, SHADER_READ, |_, _, p| {
        p.copy_from_slice(&0.5f32.to_le_bytes())
    })?);
    uploads.push(upload(
        d,
        &list.list,
        &motion,
        4,
        SHADER_READ,
        |_, _, p| p.fill(0),
    )?);
    uploads.push(upload(
        d,
        &list.list,
        &exposure,
        4,
        SHADER_READ,
        |_, _, p| p.copy_from_slice(&1.0f32.to_le_bytes()),
    )?);
    uploads.push(upload(d, &list.list, &out, 8, UNORDERED, |_, _, p| {
        for c in p.chunks_exact_mut(2) {
            c.copy_from_slice(&0x7e00u16.to_le_bytes());
        }
    })?);
    list.close();
    host.execute_sync_internal(&list);
    drop(uploads);
    let mut textures: Textures = Default::default();
    for (i, t, state) in [
        (COLOUR, colour, SHADER_READ),
        (DEPTH, depth, SHADER_READ),
        (MOTION, motion, SHADER_READ),
        (EXPOSURE, exposure, SHADER_READ),
        (OUTPUT, out.clone(), UNORDERED),
    ] {
        textures[i] = Some((t, state));
    }
    let camera = plan::Camera {
        jitter: [0.0; 2],
        motion_scale: [render[0] as f32, render[1] as f32],
        render,
        output,
        near: 10_000.0,
        far: 0.15,
        fov_y: 1.0,
        reset: true,
        flags: plan::FFX_HIGH_DYNAMIC_RANGE | plan::FFX_DEPTH_INVERTED,
    };
    let skip = std::env::var("VICTORY_CHECK_SKIP_EVALUATE").is_ok_and(|v| v == "1");
    let mut times = Vec::new();
    let mut warm = Vec::new();
    for round in 0..iterations {
        let list = host.new_list().map_err(err)?;
        let start = Instant::now();
        if !skip {
            record_mode(
                list.list.as_raw(),
                &textures,
                &camera,
                1.0,
                NEXT_ID.fetch_add(1, Relaxed) + 1,
                Some(mode),
            )?;
        }
        list.close();
        host.execute_sync_internal(&list);
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        if round < 3 {
            warm.push(ms);
        } else {
            times.push(ms);
        }
        // SAFETY: live device after synchronous submission.
        unsafe { d.GetDeviceRemovedReason() }.map_err(err)?;
    }
    let memory_after = host.video_memory();
    let pixels = readback(d, host, &out)?;
    let stats = inspect(&pixels, output)?;
    times.sort_by(f64::total_cmp);
    let percentile = |p: f64| times[((times.len() as f64 * p).ceil() as usize).saturating_sub(1)];
    Ok(vec![
        format!(
            "Validated SR: mode={} render={render:?} output={output:?} iterations={iterations}; deterministic split colour, depth=0.5, motion=0, exposure=1",
            asked.mode.name()
        ),
        format!("Readback PASS: {stats}; blake3={}", blake3::hash(&pixels)),
        format!(
            "Timing: init_ms={init_ms:.3}; warmup_ms={warm:?}; synchronized_host_ms p50={:.3} p95={:.3} samples={} (CPU recording + submit + GPU wait, not GPU-only)",
            percentile(0.5),
            percentile(0.95),
            times.len()
        ),
        format!(
            "Video memory [local_budget, local_usage, nonlocal_budget, nonlocal_usage]: before={memory_before:?}, after={memory_after:?}; not a leak test"
        ),
    ])
}

fn err(e: windows::core::Error) -> String {
    e.to_string()
}

fn half(bits: u16) -> f32 {
    let sign = if bits & 0x8000 == 0 { 1.0 } else { -1.0 };
    let exp = (bits >> 10) & 31;
    let mantissa = bits & 1023;
    match exp {
        0 => sign * f32::from(mantissa) * 2f32.powi(-24),
        31 => {
            if mantissa == 0 {
                sign * f32::INFINITY
            } else {
                f32::NAN
            }
        }
        _ => sign * (1.0 + f32::from(mantissa) / 1024.0) * 2f32.powi(i32::from(exp) - 15),
    }
}

fn inspect(bytes: &[u8], size: [u32; 2]) -> Result<String, String> {
    if bytes.len() != size[0] as usize * size[1] as usize * 8 {
        return Err("readback byte count mismatch".into());
    }
    let mut sums = [[0.0f64; 3]; 2];
    let mut count = [0u64; 2];
    for (i, p) in bytes.chunks_exact(8).enumerate() {
        for c in p[..6].chunks_exact(2) {
            let v = half(u16::from_le_bytes([c[0], c[1]]));
            if !v.is_finite() || !(-0.1..=2.0).contains(&v) {
                return Err(format!("invalid RGB pixel {i}: {v}"));
            }
        }
        let x = i as u32 % size[0];
        let region = if x < size[0] / 4 {
            Some(0)
        } else if x >= size[0] * 3 / 4 {
            Some(1)
        } else {
            None
        };
        if let Some(r) = region {
            for (sum, c) in sums[r].iter_mut().zip(p[..6].chunks_exact(2)) {
                *sum += f64::from(half(u16::from_le_bytes([c[0], c[1]])));
            }
            count[r] += 1;
        }
    }
    let means = [
        sums[0].map(|v| v / count[0] as f64),
        sums[1].map(|v| v / count[1] as f64),
    ];
    let reference = [[0.125, 0.25, 0.5], [0.75, 0.5, 0.25]];
    if count.contains(&0)
        || means
            .iter()
            .flatten()
            .zip(reference.iter().flatten())
            .any(|(a, b)| (a - b).abs() > 0.02)
    {
        return Err(format!(
            "split-colour reference mismatch: RGB means={means:?}"
        ));
    }
    Ok(format!(
        "{} finite RGB pixels; RGB means={means:?}; reference={reference:?}, tolerance=0.02",
        bytes.len() / 8
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_unwritten_and_flat_output() {
        assert!(inspect(&[0; 64], [8, 1]).is_err());
        let poison: Vec<u8> = (0..32).flat_map(|_| 0x7e00u16.to_le_bytes()).collect();
        assert!(inspect(&poison, [8, 1]).is_err());
        assert!(half(0x7c00).is_infinite());
        assert!(half(0x7e00).is_nan());
        assert_eq!(half(0x3000), 0.125);
        assert_eq!(half(0x3a00), 0.75);
    }
    #[test]
    fn accepts_reference_and_rejects_channel_corruption() {
        let mut pixels = Vec::new();
        for x in 0..8 {
            for c in if x < 4 {
                [0x3000u16, 0x3400, 0x3800, 0x3c00]
            } else {
                [0x3a00, 0x3800, 0x3400, 0x3c00]
            } {
                pixels.extend_from_slice(&c.to_le_bytes());
            }
        }
        assert!(inspect(&pixels, [8, 1]).is_ok());
        for p in pixels.chunks_exact_mut(8) {
            p[2..4].copy_from_slice(&0u16.to_le_bytes());
        }
        assert!(inspect(&pixels, [8, 1]).is_err());
        assert!(inspect(&pixels[..8], [8, 1]).is_err());
    }
}
