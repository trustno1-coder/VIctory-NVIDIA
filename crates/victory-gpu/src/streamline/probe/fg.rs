//! Initialized temporal FG fixture, with strict SDK-count acceptance.
//! SDK counts do not measure physical scanout or generated-image correctness.
use super::*;
use super::io::transition;

const WARMUP: u32 = 10;
const PERIOD: u32 = 256;

pub fn framegen_check(size: [u32; 2], frames: u32, lifecycle: bool, focus_wait_s: u32) -> Result<Vec<String>, String> {
    check(size, frames, lifecycle, focus_wait_s, false)
}

pub fn combined_check(size: [u32; 2], frames: u32, lifecycle: bool, focus_wait_s: u32) -> Result<Vec<String>, String> {
    check(size, frames, lifecycle, focus_wait_s, true)
}

fn check(size: [u32; 2], frames: u32, lifecycle: bool, focus_wait_s: u32, combined: bool) -> Result<Vec<String>, String> {
    let limit = if combined { 18000 } else { 600 };
    if size.iter().any(|&n| !(128..=4096).contains(&n)) || !(30..=limit).contains(&frames) {
        return Err(format!("validation requires dimensions 128..4096 and 30..{limit} frames"));
    }
    // Production singleton SDK/device state is deliberately not reinitialized in-process.
    // Repeat complete processes for init/shutdown coverage.
    let mut lines = Vec::new();
    let result = run(size, frames, lifecycle, std::time::Duration::from_secs(focus_wait_s.into()), combined, &mut lines);
    let shutdown = shut_down();
    CHECKING.store(false, Relaxed);
    lines.push(format!("Shutdown: {shutdown:?}"));
    match result {
        Ok(()) if shutdown.as_deref() == Some("Streamline shut down: ok (0)") => Ok(lines),
        Ok(()) => Err(format!("{}\nshutdown failed", lines.join("\n"))),
        Err(e) => Err(format!("{}\nFG validation FAILED: {e}", lines.join("\n"))),
    }
}

fn run(size: [u32; 2], frames: u32, lifecycle: bool, focus_wait: std::time::Duration, combined: bool, lines: &mut Vec<String>) -> Result<(), String> {
    if std::env::var("VICTORY_FRAMEGEN_CYCLE").is_ok_and(|v| v != "0") {
        return Err("unset VICTORY_FRAMEGEN_CYCLE: the probe owns phase transitions".into());
    }
    let mut asked = Asked::from_env()?;
    asked.dlss = combined;
    asked.framegen = true;
    asked.fsr_framegen = false;
    asked.present_hook = false;
    load_and_init(&asked, false)?;
    if !PLAN.get().is_some_and(|p| p.framegen) { return Err("FG unsupported".into()); }
    let host = crate::host::host().ok_or("no D3D12 host")?;
    CHECK_DEVICE.store(true, Relaxed);
    CHECKING.store(true, Relaxed);
    if !FG_READY.load(Acquire) { return Err("FG device not ready".into()); }
    if combined && !DLSS_READY.load(Acquire) { return Err("combined SR device not ready".into()); }
    let split = combined && super::combined::split_token();
    if combined {
        lines.push("Combined SR -> FP16 HUD-less colour -> RGBA8 composition -> FG; static reference; first/final SR readback, no generated-image oracle".into());
        lines.push(format!("Combined frame token: {}; SR submission: {}",
            if split { "split SR/FG counters (VICTORY_CHECK_SPLIT_TOKEN=1)" } else { "one index per frame shared by SR constants, FG constants and present markers" },
            if super::combined::asynchronous() { "unwaited, ordered on the direct queue (VICTORY_CHECK_SR_ASYNC=1)" } else { "waited (default)" }));
    }
    let get_state = FUNCTIONS.get().and_then(|f| f.dlssg_get_state).ok_or("no FG state query")?;
    let mut caps = abi::DlssgState::default();
    // SAFETY: initialized SDK, live device, valid output. Before any presents; does not drain live counts.
    let code = unsafe { get_state(&abi::ViewportHandle::new(VIEWPORT_FG), &mut caps, std::ptr::null()) };
    if code != 0 { return Err(format!("FG capabilities query: {code}")); }
    let max = caps.num_frames_to_generate_max.saturating_add(1);
    let requested = framegen_wanted();
    if requested > max { return Err(format!("requested {requested}x exceeds SDK maximum {max}x")); }
    if size.iter().any(|&n| n < caps.min_width_or_height) { return Err(format!("below SDK minimum {}", caps.min_width_or_height)); }
    lines.push(format!("FG capabilities: maximum={max}x minimum={} requested={requested}x; original scrolling pattern, depth 0.5, pixel motion+1x; SDK counts only", caps.min_width_or_height));
    let smaller = size.map(|n| (n * 3 / 4).max(caps.min_width_or_height).max(128));
    let phases = if lifecycle {
        vec![("off",size,0,false),("on",size,2,false),("maximum",size,max.min(6),false),
             ("off-again",size,0,false),("resize-on",smaller,2,false),("resize-back",size,2,false),
             ("recreate",size,2,true)]
    } else { vec![("steady",size,requested,false)] };
    let mut frame = 0u64;
    let mut failed = Vec::new();
    for (name, output, mode, recreate) in phases {
        crate::host::settle_for_fence();
        if recreate {
            resizing();
            host.drop_presenter();
        }
        set_framegen(mode);
        let fixture = if combined { None } else { Some(Fixture::new(host, output)?) };
        let mut sr = if combined { Some(super::combined::Combined::new(host, output, &asked)?) } else { None };
        let before = host.video_memory();
        let mut times = Vec::new();
        let mut shown = 0u64;
        let mut on = 0u64;
        let mut status = 0u32;
        let mut last_shown = FG_SHOWN_ALL.load(Relaxed);
        let mut last_on = FG_ON_ALL.load(Relaxed);
        let mut native_start = None;
        let mut focused = 0u64;
        let mut deltas = Vec::new();
        // Warm-up is at least WARMUP presents and lasts until the window has been
        // in front for WARMUP presents running and DLSS-G showed `mode` frames
        // for the last one, or the focus wait ends; then frames-WARMUP are measured.
        let deadline = Instant::now() + focus_wait;
        let wanted = frames - WARMUP;
        let mut measuring_from: Option<u32> = None;
        // DLSS-G resumes a few presents after its window comes to the front.
        let mut in_front_run = 0u32;
        let mut per = String::new();
        let mut round = 0u32;
        // Presents paced to the 16.7 ms frame time given to DLSS-G, from a
        // deadline rather than a fixed sleep after the work.
        let period = std::time::Duration::from_micros(16_667);
        let mut next = Instant::now();
        let mut last_start: Option<Instant> = None;
        let (mut adv, mut pres, mut settle, mut interval) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        // Per measured present: (round, advance, present, settle) ms. Per interval
        // between measured starts: (ms, row of the present it spans, its sleep's lateness ms).
        let mut rows: Vec<(u32, f64, f64, f64)> = Vec::new();
        // (The first interval spans the last warm-up present: no row.)
        let mut gaps: Vec<(f64, Option<usize>, f64)> = Vec::new();
        let mut oversleep = 0.0f64;
        let mut vram_peak = 0u64;
        while measuring_from.is_none_or(|m| round - m < wanted) {
            frame += 1;
            // One frame-token index per frame for SR, FG and the present markers.
            let token = (!split).then_some(frame);
            let start = Instant::now();
            if let Some(s) = &mut sr { s.advance(host, round, token)?; }
            if let Some(f) = &fixture { f.advance(host, round % PERIOD)?; }
            let advanced = Instant::now();
            if let Some(s) = &sr {
                *lock(&SUBMITTED) = Some(Noted { id:token.unwrap_or_else(||NEXT_ID.fetch_add(1,Relaxed)+1),
                    depth:Source::Fixed(s.depth.clone(),SHADER_READ),
                    motion:Source::Fixed(s.motion.clone(),SHADER_READ),camera:s.camera,frame_ms:16.7 });
                host.present_planes(&s.out, SHADER_READ, Some((&s.overlay, SHADER_READ)), frame)
                    .ok_or("combined present/resize failed")?;
            } else if let Some(fixture) = &fixture {
            let camera = plan::Camera { jitter:[0.0;2], motion_scale:[1.0;2], render:output, output,
                near:0.15, far:10_000.0, fov_y:1.0, reset:round==0, flags:plan::FFX_DEPTH_INVERTED };
            *lock(&SUBMITTED) = Some(Noted { id:NEXT_ID.fetch_add(1,Relaxed)+1,
                depth:Source::Fixed(fixture.depth.clone(),SHADER_READ),
                motion:Source::Fixed(fixture.motion.clone(),SHADER_READ),camera,frame_ms:16.7 });
            lock(&HUD_SUBMITTED).push(Copied { id:frame,target:fixture.base.as_raw() as usize,texture:fixture.hud.clone() });
            host.present_planes(&fixture.base, SHADER_READ, None, frame).ok_or("present/resize failed")?;
            }
            let presented = Instant::now();
            crate::host::settle_for_fence();
            let settled = Instant::now();
            // The production present path already calls GetState; a second call would drain counts.
            let now_shown=FG_SHOWN_ALL.load(Relaxed);
            let now_on=FG_ON_ALL.load(Relaxed);
            // DLSS-G makes no frames for a window out of focus, with status 0.
            let in_front = host.present_window_focused()==Some(true);
            if let Some(m) = measuring_from {
                if in_front { focused+=1; } else if round==m || round+1==m+wanted { tracing::warn!(round, "FG probe: window not in front: {}", host.foreground_owner()); }
                deltas.push(now_shown-last_shown);
                let ms=|a:Instant,b:Instant|(b-a).as_secs_f64()*1000.0;
                adv.push(ms(start,advanced)); pres.push(ms(advanced,presented)); settle.push(ms(presented,settled));
                if let Some(p)=last_start { interval.push(ms(p,start)); gaps.push((ms(p,start),rows.len().checked_sub(1),oversleep)); }
                rows.push((round,ms(start,advanced),ms(advanced,presented),ms(presented,settled)));
                if let Some(g)=host.video_memory_groups() { vram_peak=vram_peak.max(g[0][1]); }
                shown+=now_shown-last_shown; on+=now_on-last_on; status|=FG_STATUS.load(Relaxed); times.push(start.elapsed().as_secs_f64()*1000.0);
            } else {
                let in_front = in_front || host.focus_present_window()==Some(true);
                in_front_run = if in_front { in_front_run + 1 } else { 0 };
                // And until DLSS-G makes frames again: it pauses after a resize or a
                // new swap chain while it makes its resources anew.
                let resumed = mode==0 || now_shown-last_shown==u64::from(mode);
                if round+1>=WARMUP && ((in_front_run>=WARMUP && resumed) || Instant::now()>=deadline) {
                    tracing::info!(round, in_front, resumed, "FG probe: warm-up over, measuring");
                    measuring_from=Some(round+1);
                    native_start=host.present_counts();
                }
            }
            per.push(char::from_digit(u32::try_from(now_shown-last_shown).unwrap_or(35).min(35),36).unwrap_or('?'));
            last_shown=now_shown; last_on=now_on;
            // SAFETY: live device and completed application queue work.
            unsafe { host.device.GetDeviceRemovedReason() }.map_err(err)?;
            if measuring_from.is_some() { last_start=Some(start); }
            next += period;
            // How late the sleep woke (timer granularity), kept for the next present's row.
            oversleep = match next.checked_duration_since(Instant::now()) {
                Some(wait) => { std::thread::sleep(wait); Instant::now().saturating_duration_since(next).as_secs_f64()*1000.0 }
                None => { next = Instant::now(); 0.0 }
            };
            round += 1;
        }
        let warm = measuring_from.unwrap_or(0);
        tracing::info!(phase = name, "FG probe: SDK frames shown per present: {per}");
        let native_end=host.present_counts();
        let native=native_delta(native_start,native_end);
        let measured=u64::from(wanted);
        let verdict=if mode>0 && focused<measured { Err(format!("window focused for {focused}/{measured} measured presents; DLSS-G does not interpolate unfocused")) } else { accept(mode,on,&deltas,status,native.0) };
        for v in [&mut times,&mut adv,&mut pres,&mut settle,&mut interval] { v.sort_by(f64::total_cmp); }
        let at=|v:&[f64],p:f64|v.get(((v.len() as f64*p).ceil() as usize).saturating_sub(1)).copied().unwrap_or(f64::NAN);
        let pct=|p:f64|at(&times,p);
        lines.push(format!("Phase {name}: size={output:?} requested={mode}x presents={round} warmup={warm} measured={measured} enabled={on} SDK_shown={shown} status={status:#x} focused={focused} pause={} proxy={} PASS={}; synchronized_host_ms p50={:.3} p95={:.3}; memory_before={before:?} memory_after={:?}; dxgi_present_delta={:?} dxgi_stats_delta={:?}; split_ms p50 advance={:.3} present={:.3} settle={:.3}; present_ms p95={:.3}; interval_ms p50={:.3} p95={:.3} max={:.3}",verdict.as_deref().unwrap_or("-"),SWAP_CHAIN_PROXIED.load(Relaxed),verdict.is_ok(),pct(0.5),pct(0.95),host.video_memory(),native.0,native.1,at(&adv,0.5),at(&pres,0.5),at(&settle,0.5),at(&pres,0.95),at(&interval,0.5),at(&interval,0.95),interval.last().copied().unwrap_or(f64::NAN)));
        gaps.sort_by(|a,b| b.0.total_cmp(&a.0));
        let slow: Vec<String> = gaps.iter().take(5).map(|&(g,i,late)| match i.and_then(|i| rows.get(i).map(|r| (i, *r))) {
            Some((i,(r,a,p,s))) => format!("{g:.3}@r{r}(advance={a:.3} present={p:.3} settle={s:.3} sleep_late={late:.3} shown={})", deltas.get(i).copied().unwrap_or(0)),
            None => format!("{g:.3}@warmup-end(sleep_late={late:.3})"),
        }).collect();
        let mem = super::combined::process_memory();
        lines.push(format!("Phase {name} diagnostics: slowest_intervals_ms {}; vram_local_usage_peak_sampled={vram_peak}; process_private={:?} peak_private={:?} peak_working_set={:?} (bytes; peaks are process-lifetime)",
            slow.join(" "), mem.map(|m| m.0), mem.map(|m| m.1), mem.map(|m| m.2)));
        if let Err(e)=verdict { failed.push(format!("{name}: {e}")); }
        if let Some(s) = &sr { s.wait(); lines.push(s.check(host)?); }
        // Disable and drop SDK references before the fixture's resources are released.
        resizing();
        *lock(&SUBMITTED)=None;
        lock(&HUD_SUBMITTED).clear();
        crate::host::settle_for_fence();
    }
    if failed.is_empty() { Ok(()) } else { Err(failed.join("; ")) }
}

/// DXGI present-count deltas (GetLastPresentCount, and FrameStatistics present/refresh).
fn native_delta(a:Option<(u32,Option<(u32,u32)>)>,b:Option<(u32,Option<(u32,u32)>)>)->(Option<u32>,Option<(u32,u32)>) {
    match (a,b) {
        (Some((l0,s0)),Some((l1,s1)))=>(Some(l1.wrapping_sub(l0)),s0.zip(s1).map(|((p0,r0),(p1,r1))|(p1.wrapping_sub(p0),r1.wrapping_sub(r0)))),
        _=>(None,None),
    }
}

/// Longest run of presents DLSS-G may skip once in a phase: the window's
/// one-time promotion by DWM to a hardware plane (Independent Flip) pauses
/// generation for about seven presents on an RTX 5090 at 1080p.
const MAX_PAUSE: usize = 10;

/// Per measured present the SDK must report 1 (base only) or `mode` frames,
/// never another count; base-only presents may form one run of at most
/// [`MAX_PAUSE`]; DXGI's own present count, when known, must show at least
/// 90% of the generated frames and at most two multipliers more. Ok gives
/// the pause ("none" or its first measured index and length).
fn accept(mode:u32, on:u64, deltas:&[u64], status:u32, dxgi:Option<u32>)->Result<String,String> {
    let measured=deltas.len() as u64;
    let shown:u64=deltas.iter().sum();
    if status!=0 { return Err(format!("SDK status {status:#x}")); }
    if mode==0 {
        return if on==0 && deltas.iter().all(|&d|d<=1) {Ok("none".into())} else {Err(format!("FG remained active while off: enabled={on}, SDK_shown={shown}"))};
    }
    let mode=u64::from(mode);
    if on!=measured { return Err(format!("FG enabled on {on}/{measured} measured presents")); }
    if let Some(i)=deltas.iter().position(|&d|d!=1 && d!=mode) {
        return Err(format!("present {i} showed {} frames, expected 1 or {mode}",deltas[i]));
    }
    let mut runs=Vec::new();
    let mut i=0;
    while i<deltas.len() {
        if deltas[i]==1 { let s=i; while i<deltas.len() && deltas[i]==1 { i+=1; } runs.push((s,i-s)); } else { i+=1; }
    }
    // A single base-only present at either end of the window is accounting lag
    // across the window boundary, not a pause.
    let last=deltas.len().saturating_sub(1);
    runs.retain(|&(s,n)|!(n==1 && (s==0 || s==last)));
    let pause=match runs.as_slice() {
        [] => "none".to_string(),
        [(s,n)] if *n<=MAX_PAUSE => format!("{n}@{s}"),
        _ => return Err(format!("presents without generated frames: runs (start,len) {runs:?}; at most one run of {MAX_PAUSE} allowed")),
    };
    // DXGI counts what reached it: generated frames come from the present
    // thread, a few base presents late at each end of the window, and at high
    // multipliers some never reach it (6x windowed: up to ~2% fewer). It must
    // show at least 90% of the generated frames, and never more than the SDK
    // plus two multipliers.
    if let Some(d)=dxgi {
        let d=u64::from(d);
        let floor=measured+(shown.saturating_sub(measured))*9/10;
        if d<floor || d>shown+2*mode {
            return Err(format!("DXGI counted {d} presents, SDK {shown}; expected {floor}..={}",shown+2*mode));
        }
    }
    Ok(pause)
}
/// `list` is the upload list, kept and reset for every frame's copies (a new
/// allocator and list per frame cost more than the copies themselves).
struct Fixture { base:ID3D12Resource,hud:ID3D12Resource,atlas:ID3D12Resource,depth:ID3D12Resource,motion:ID3D12Resource,size:[u32;2],list:crate::host::HostList }
impl Fixture {
    fn new(host:&crate::host::Host,size:[u32;2])->Result<Self,String> {
        let d=&host.device;
        let make=|s,f|texture(d,s,f,D3D12_RESOURCE_FLAG_NONE,COPY_DEST).map_err(err);
        let base=make(size,DXGI_FORMAT_R8G8B8A8_UNORM)?;
        let hud=make(size,DXGI_FORMAT_R8G8B8A8_UNORM)?;
        let atlas=make([size[0]+PERIOD,size[1]],DXGI_FORMAT_R8G8B8A8_UNORM)?;
        let depth=make(size,DXGI_FORMAT_R32_FLOAT)?;
        let motion=make(size,DXGI_FORMAT_R16G16_FLOAT)?;
        let l=host.new_list().map_err(err)?;
        let uploads=vec![
            upload(d,&l.list,&atlas,4,D3D12_RESOURCE_STATE_COPY_SOURCE.0 as u32,|x,y,p|{
                let a=x%PERIOD;let b=y%PERIOD;p.copy_from_slice(&[a as u8,b as u8,if (a/32+b/32).is_multiple_of(2){48}else{208},255]);
            })?,
            upload(d,&l.list,&depth,4,SHADER_READ,|_,_,p|p.copy_from_slice(&0.5f32.to_le_bytes()))?,
            upload(d,&l.list,&motion,4,SHADER_READ,|_,_,p|p.copy_from_slice(&[0,0x3c,0,0]))?,
        ];
        transition(&l.list,&base,COPY_DEST,SHADER_READ);
        l.close();host.execute_sync_internal(&l);drop(uploads);
        Ok(Self{base,hud,atlas,depth,motion,size,list:l})
    }
    fn advance(&self,host:&crate::host::Host,offset:u32)->Result<(),String>{
        // Every submission is waited for (host.rs), so the list's last run is done.
        let l=&self.list;
        l.reset();
        transition(&l.list,&self.base,SHADER_READ,COPY_DEST);
        // SAFETY: compatible RGBA8 textures, source box within initialized atlas; resources live through wait.
        unsafe {
            let loc=|t:&ID3D12Resource|D3D12_TEXTURE_COPY_LOCATION{pResource:std::mem::transmute_copy(t),Type:D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,Anonymous:D3D12_TEXTURE_COPY_LOCATION_0{SubresourceIndex:0}};
            let source=loc(&self.atlas);
            let region=D3D12_BOX{left:offset,top:0,front:0,right:offset+self.size[0],bottom:self.size[1],back:1};
            for t in [&self.base,&self.hud]{l.list.CopyTextureRegion(&loc(t),0,0,0,&source,Some(&region));}
        }
        transition(&l.list,&self.base,COPY_DEST,SHADER_READ);
        l.close();host.execute_sync_internal(l);Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn settings_success_is_not_generated_output(){
        let all=|n:u64,len:usize|vec![n;len];
        assert!(accept(2,80,&all(1,80),0,None).is_err());
        assert!(accept(6,80,&all(1,80),0,None).is_err());
        assert_eq!(accept(2,80,&all(2,80),0,Some(160)).as_deref(),Ok("none"));
        assert!(accept(6,80,&all(6,80),0,Some(480)).is_ok());
        assert!(accept(0,1,&all(1,80),0,None).is_err());
        assert!(accept(0,0,&all(1,80),0,None).is_ok());
        assert!(accept(2,80,&all(2,80),8,None).is_err());
        assert!(accept(2,79,&all(2,80),0,None).is_err());
    }
    #[test] fn one_short_pause_is_the_plane_promotion(){
        let mut d=vec![4u64;110];
        d[28..35].fill(1);
        assert_eq!(accept(4,110,&d,0,Some(419)).as_deref(),Ok("7@28"));
        assert!(accept(4,110,&d,0,Some(300)).is_err(), "DXGI disagrees");
        assert!(accept(4,110,&d,0,Some(411)).is_ok(), "DXGI a little behind");
        assert!(accept(4,110,&d,0,Some(110)).is_err(), "DXGI saw no generated frames");
        assert!(accept(4,110,&d,0,Some(428)).is_err(), "DXGI far above the SDK");
        assert!(accept(6,110,&vec![6;110],0,Some(607)).is_ok(), "6x windowed shortfall seen on RTX 5090");
        let mut e=d.clone(); e[0]=1; e[109]=1;
        assert_eq!(accept(4,110,&e,0,None).as_deref(),Ok("7@28"), "boundary lag at both ends");
        d[60]=1;
        assert!(accept(4,110,&d,0,None).is_err(), "a second pause");
        let mut d=vec![2u64;110];
        d[10..21].fill(1);
        assert!(accept(2,110,&d,0,None).is_err(), "pause longer than MAX_PAUSE");
        d[10..21].fill(2); d[50]=3;
        assert!(accept(2,110,&d,0,None).is_err(), "partial multiplier");
    }
}