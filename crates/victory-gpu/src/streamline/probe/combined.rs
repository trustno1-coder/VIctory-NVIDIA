//! VN03E: actual SR output feeds FG through the existing two-plane compositor.
//! Static camera/scene, zero motion; this is not a temporal-quality oracle.
use super::*;
use super::io::transition;

pub(super) struct Combined {
    pub(super) out: ID3D12Resource,
    pub(super) overlay: ID3D12Resource,
    pub(super) depth: ID3D12Resource,
    pub(super) motion: ID3D12Resource,
    pub(super) camera: plan::Camera,
    textures: Textures,
    list: crate::host::HostList,
    mode: u32,
    evaluations: u64,
    skip: bool,
    /// `VICTORY_CHECK_SR_ASYNC=1`: SR is sent without a CPU wait; ordering
    /// against the compositor comes from the shared direct queue, and
    /// `fence` reaching `fenced` says the list's last run is done.
    asynchronous: bool,
    fence: ID3D12Fence,
    fenced: u64,
}

impl Drop for Combined {
    fn drop(&mut self) {
        // Textures and the list must outlive an unwaited SR submission.
        self.wait();
    }
}

impl Combined {
    pub(super) fn new(host: &crate::host::Host, output: [u32; 2], asked: &Asked) -> Result<Self, String> {
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
        (DEPTH, depth.clone(), SHADER_READ),
        (MOTION, motion.clone(), SHADER_READ),
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
        near: 0.15,
        far: 10_000.0,
        fov_y: 1.0,
        reset: true,
        flags: plan::FFX_HIGH_DYNAMIC_RANGE | plan::FFX_DEPTH_INVERTED,
    };

        // Transparent overlay makes the existing compositor convert FP16 SR output
        // to the RGBA8 swap chain. The same FP16 output is tagged as HUD-less colour.
        let overlay = texture(d, output, DXGI_FORMAT_R8G8B8A8_UNORM,
            D3D12_RESOURCE_FLAG_NONE, COPY_DEST).map_err(err)?;
        list.reset();
        let staging = upload(d, &list.list, &overlay, 4, SHADER_READ, |_,_,p| p.fill(0))?;
        transition(&list.list, &out, UNORDERED, SHADER_READ);
        list.close();
        host.execute_sync_internal(&list);
        drop(staging);
        // SAFETY: live device; a plain fence of our own.
        let fence: ID3D12Fence = unsafe { d.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.map_err(err)?;
        Ok(Self { out, overlay, depth, motion, camera, textures, list, mode,
            evaluations: 0, skip: std::env::var("VICTORY_CHECK_SKIP_EVALUATE").is_ok_and(|v| v == "1"),
            asynchronous: asynchronous(), fence, fenced: 0 })
    }

    /// Blocks until the last unwaited SR submission has run (none: at once).
    pub(super) fn wait(&self) {
        // SAFETY: our fence; a null event makes the call block until the value is reached.
        unsafe {
            if self.fenced == 0 { return; }
            let completed = self.fence.GetCompletedValue();
            if completed == u64::MAX {
                crate::host::fail(format_args!("combined SR: device removed while waiting for {}", self.fenced));
            }
            if completed < self.fenced {
                if let Err(e) = self.fence.SetEventOnCompletion(self.fenced, windows::Win32::Foundation::HANDLE::default()) {
                    crate::host::fail(format_args!("combined SR: fence wait failed: {e}"));
                }
                let completed = self.fence.GetCompletedValue();
                if completed == u64::MAX || completed < self.fenced {
                    crate::host::fail(format_args!("combined SR: fence {completed} did not reach {}", self.fenced));
                }
            }
        }
    }

    /// One SR evaluation. `token`: the frame-token index shared with this
    /// frame's FG constants and present markers; `None` keeps the earlier
    /// separate counter (`VICTORY_CHECK_SPLIT_TOKEN=1`, for A/B only).
    pub(super) fn advance(&mut self, host: &crate::host::Host, round: u32, token: Option<u64>) -> Result<(), String> {
        self.camera.reset = round == 0;
        // The list's previous run must be done before its allocator is reset.
        self.wait();
        self.list.reset();
        transition(&self.list.list, &self.out, SHADER_READ, UNORDERED);
        let id = token.unwrap_or_else(|| NEXT_ID.fetch_add(1, Relaxed) + 1);
        let recorded = if self.skip { Ok(()) } else {
            record_mode(self.list.list.as_raw(), &self.textures, &self.camera, 1.0,
                id, Some(self.mode)).map(|_| ())
        };
        // Even on a recording error, submit (and wait) before teardown can release inputs.
        transition(&self.list.list, &self.out, UNORDERED, SHADER_READ);
        self.list.close();
        if self.asynchronous {
            let sent = host.execute_unwaited(&self.list, &self.fence, self.fenced + 1);
            if sent.is_ok() { self.fenced += 1; }
            if recorded.is_err() || sent.is_err() { self.wait(); }
            sent.map_err(err)?;
        } else {
            host.execute_sync_internal(&self.list);
        }
        recorded?;
        if !self.skip { self.evaluations += 1; }
        if round == 0 { self.check(host)?; }
        Ok(())
    }

    pub(super) fn check(&self, host: &crate::host::Host) -> Result<String, String> {
        let began = Instant::now();
        tracing::info!(evaluations=self.evaluations, "combined SR readback beginning (outside measured presents)");
        let pixels = super::io::readback_from(&host.device, host, &self.out, SHADER_READ)?;
        let stats = inspect(&pixels, self.camera.output)?;
        tracing::info!(evaluations=self.evaluations, packed_bytes=pixels.len(), readback_check_ms=began.elapsed().as_secs_f64()*1000.0,
            "combined SR readback complete (including CPU reference inspection)");
        Ok(format!("Combined SR readback PASS: render={:?} output={:?} evaluations={}; {stats}; blake3={}",
            self.camera.render, self.camera.output, self.evaluations, blake3::hash(&pixels)))
    }
}

/// `VICTORY_CHECK_SR_ASYNC=1`: send SR without waiting (the `VICTORY_RELAY`
/// style overlap). Default: waited for, as every default-mode submission is.
pub(super) fn asynchronous() -> bool {
    std::env::var("VICTORY_CHECK_SR_ASYNC").is_ok_and(|v| v == "1")
}

/// `VICTORY_CHECK_SPLIT_TOKEN=1`: SR and FG take separate frame-token
/// indices, as before VN03E-opt (A/B only).
pub(super) fn split_token() -> bool {
    std::env::var("VICTORY_CHECK_SPLIT_TOKEN").is_ok_and(|v| v == "1")
}

/// Process memory counters: (private commit, peak private commit, peak working set) in bytes.
pub(super) fn process_memory() -> Option<(usize, usize, usize)> {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS { cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32, ..Default::default() };
    // SAFETY: our own process and a structure of the size given.
    (unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0)
        .then_some((c.PagefileUsage, c.PeakPagefileUsage, c.PeakWorkingSetSize))
}
