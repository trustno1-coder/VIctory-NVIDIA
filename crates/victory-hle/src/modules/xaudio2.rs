//! XAudio2: the host's own (`xaudio2_9.dll` is the same API on both, and
//! ordinal 1 is `XAudio2Create` on both), plus what a PC's lacks: XMA2
//! source voices. On the console those decode on the audio processor; the
//! host's XAudio2 refuses the format (`XAUDIO2_E_INVALID_CALL`).
//!
//! The engine the title gets is the host's, with a copy of its vtable in
//! which `CreateSourceVoice` is ours. An XMA2 voice becomes a 16-bit PCM
//! voice of the same channels and rate, again with a vtable copy, whose
//! `SubmitSourceBuffer` decodes each XMA2 buffer (`victory_formats::xma`)
//! and submits the PCM. Everything else, the host's own virtual calls
//! included, reaches the host's functions unchanged. The decoded buffers
//! live until the host has played them (`GetState`'s queued count says
//! which), or until the voice is destroyed.
//!
//! Not yet: XMA2 with more than one stream (more than two channels). Such
//! a voice is passed to the host as before, which refuses it.
#![allow(non_snake_case)]
// Title code calls these through its import table and vtables, passing
// pointers the real API would trust.
#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::collections::{HashMap, VecDeque};
use std::ffi::c_void;
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use victory_formats::xma;

use crate::modules::xgameruntime::HRESULT;
use crate::{HleExport, HleModule, hle_export, host};

const E_FAIL: HRESULT = 0x8000_4005_u32 as i32;
const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_XMA2: u16 = 0x166;
/// `XAUDIO2_END_OF_STREAM`: no more buffers after this one.
const END_OF_STREAM: u32 = 0x40;
const XAUDIO2_VOICE_NOSAMPLESPLAYED: u32 = 0x100;

// Vtable slots (pointer-sized entries).
/// IXAudio2: QueryInterface, AddRef, Release, RegisterForCallbacks,
/// UnregisterForCallbacks, then CreateSourceVoice.
const CREATE_SOURCE_VOICE: usize = 5;
/// IXAudio2Voice's 19 methods, DestroyVoice last, then the source voice's.
const DESTROY_VOICE: usize = 18;
const SUBMIT_SOURCE_BUFFER: usize = 21;
const GET_STATE: usize = 25;
/// Entries copied: more than either interface has, so that the host's
/// own further virtuals (its destructor, say) are carried over too.
const COPIED: usize = 64;

/// `XAUDIO2_BUFFER` (XAudio2.h packs its structures to 1 byte).
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct Buffer {
    flags: u32,
    audio_bytes: u32,
    audio_data: *const u8,
    play_begin: u32,
    play_length: u32,
    loop_begin: u32,
    loop_length: u32,
    loop_count: u32,
    context: *mut c_void,
}

/// `XAUDIO2_VOICE_STATE`.
#[repr(C, packed)]
#[derive(Default)]
struct VoiceState {
    current_context: usize,
    buffers_queued: u32,
    samples_played: u64,
}

type CreateSourceVoice = unsafe extern "system" fn(
    *mut c_void,
    *mut *mut c_void,
    *const u8,
    u32,
    f32,
    *mut c_void,
    *const c_void,
    *const c_void,
) -> HRESULT;
type SubmitSourceBuffer =
    unsafe extern "system" fn(*mut c_void, *const Buffer, *const c_void) -> HRESULT;
type GetState = unsafe extern "system" fn(*mut c_void, *mut VoiceState, u32);
type DestroyVoice = unsafe extern "system" fn(*mut c_void);

// --- Vtable copies -----------------------------------------------------------

/// Our copies of host vtables, made once per host vtable: entry `COPIED`
/// holds the host's vtable itself, for calling its own functions.
struct Copies {
    by_host: Mutex<Vec<(usize, &'static [usize])>>,
}

impl Copies {
    const fn new() -> Copies {
        Copies {
            by_host: Mutex::new(Vec::new()),
        }
    }

    /// Points `object` at our copy of its vtable, with `ours` in place of
    /// the host's entries. Already ours: left as it is.
    ///
    /// # Safety
    /// `object` is a live COM object of the host's.
    unsafe fn adopt(&self, object: *mut c_void, ours: &[(usize, usize)]) {
        let slot = object.cast::<*const usize>();
        // SAFETY: a COM object starts with its vtable pointer.
        let host = unsafe { *slot } as usize;
        let mut copies = self.by_host.lock().unwrap_or_else(|e| e.into_inner());
        if copies.iter().any(|(_, c)| c.as_ptr() as usize == host) {
            return;
        }
        let copy = match copies.iter().find(|(h, _)| *h == host) {
            Some((_, c)) => *c,
            None => {
                let n = readable_entries(host, COPIED);
                let mut table = vec![0usize; COPIED + 1];
                // SAFETY: `n` entries from `host` are readable (just checked).
                unsafe {
                    std::ptr::copy_nonoverlapping(host as *const usize, table.as_mut_ptr(), n)
                };
                for &(index, f) in ours {
                    table[index] = f;
                }
                table[COPIED] = host;
                let leaked: &'static [usize] = Box::leak(table.into_boxed_slice());
                copies.push((host, leaked));
                leaked
            }
        };
        // SAFETY: as above; the object is the host's, its memory writable.
        unsafe { *slot = copy.as_ptr() };
    }
}

/// How many of `wanted` pointer-sized entries from `address` are in
/// committed, readable memory.
fn readable_entries(address: usize, wanted: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    // SAFETY: a plain query; `info` is ours.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    let got = unsafe {
        VirtualQuery(
            address as *const c_void,
            &mut info,
            size_of::<MEMORY_BASIC_INFORMATION>(),
        )
    };
    if got == 0 || info.State != MEM_COMMIT || info.Protect & PAGE_NOACCESS != 0 {
        return 0;
    }
    let end = info.BaseAddress as usize + info.RegionSize;
    ((end - address) / size_of::<usize>()).min(wanted)
}

/// The host's own entry `index` for an object whose vtable is one of ours.
///
/// # Safety
/// `object`'s vtable was set by [`Copies::adopt`].
unsafe fn host_entry(object: *mut c_void, index: usize) -> usize {
    // SAFETY: ours, `COPIED + 1` entries, the last the host's vtable.
    unsafe {
        let ours = *object.cast::<*const usize>();
        let host = *ours.add(COPIED) as *const usize;
        *host.add(index)
    }
}

static ENGINES: Copies = Copies::new();
static XMA_VOICES: Copies = Copies::new();

// --- Engine ----------------------------------------------------------------------

fn host_create(name: &'static str) -> Option<usize> {
    static FOUND: LazyLock<Mutex<HashMap<&'static str, Option<usize>>>> =
        LazyLock::new(Default::default);
    let mut found = FOUND.lock().unwrap_or_else(|e| e.into_inner());
    *found
        .entry(name)
        .or_insert_with(|| host::function("xaudio2_9.dll", name))
}

/// Gives the engine our `CreateSourceVoice`.
fn adopt_engine(hr: HRESULT, out: *mut *mut c_void) -> HRESULT {
    // SAFETY: the host just filled `out` on success.
    if hr >= 0 && !out.is_null() && unsafe { !(*out).is_null() } {
        unsafe {
            ENGINES.adopt(
                *out,
                &[(
                    CREATE_SOURCE_VOICE,
                    create_source_voice as *const () as usize,
                )],
            )
        };
    }
    hr
}

/// `XAudio2Create` (ordinal 1): the host's engine, with XMA2 voices.
pub extern "system" fn XAudio2Create(out: *mut *mut c_void, flags: u32, processor: u32) -> HRESULT {
    let Some(f) = host_create("XAudio2Create") else {
        return E_FAIL;
    };
    // SAFETY: the host's `XAudio2Create`.
    let f: unsafe extern "system" fn(*mut *mut c_void, u32, u32) -> HRESULT =
        unsafe { std::mem::transmute(f) };
    adopt_engine(unsafe { f(out, flags, processor) }, out)
}

/// The PC header's form of [`XAudio2Create`].
pub extern "system" fn XAudio2CreateWithVersionInfo(
    out: *mut *mut c_void,
    flags: u32,
    processor: u32,
    ntddi: u32,
) -> HRESULT {
    let Some(f) = host_create("XAudio2CreateWithVersionInfo") else {
        return E_FAIL;
    };
    // SAFETY: the host's `XAudio2CreateWithVersionInfo`.
    let f: unsafe extern "system" fn(*mut *mut c_void, u32, u32, u32) -> HRESULT =
        unsafe { std::mem::transmute(f) };
    adopt_engine(unsafe { f(out, flags, processor, ntddi) }, out)
}

/// An XMA2 format's channels and rate, if we decode it: `XMA2WAVEFORMATEX`
/// is a `WAVEFORMATEX` (18 bytes) with `NumStreams` first in its extra.
fn xma2_format(format: *const u8) -> Option<(u16, u32)> {
    if format.is_null() {
        return None;
    }
    // SAFETY: a WAVEFORMATEX, and its extra bytes when cbSize says so.
    let head = unsafe { std::slice::from_raw_parts(format, 18) };
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
    if u16_at(head, 0) != WAVE_FORMAT_XMA2 || u16_at(head, 16) < 34 {
        return None;
    }
    let channels = u16_at(head, 2);
    let rate = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    // SAFETY: cbSize >= 34.
    let streams = u16_at(unsafe { std::slice::from_raw_parts(format, 20) }, 18);
    if streams != 1 || !(1..=2).contains(&channels) {
        tracing::warn!(
            channels,
            streams,
            "an XMA2 voice of more than one stream: not decoded yet, left to the host"
        );
        return None;
    }
    Some((channels, rate))
}

/// 16-bit PCM, as a packed WAVEFORMATEX.
fn pcm_format(channels: u16, rate: u32) -> [u8; 18] {
    let align = channels * 2;
    let mut f = [0u8; 18];
    f[0..2].copy_from_slice(&WAVE_FORMAT_PCM.to_le_bytes());
    f[2..4].copy_from_slice(&channels.to_le_bytes());
    f[4..8].copy_from_slice(&rate.to_le_bytes());
    f[8..12].copy_from_slice(&(rate * align as u32).to_le_bytes());
    f[12..14].copy_from_slice(&align.to_le_bytes());
    f[14..16].copy_from_slice(&16u16.to_le_bytes());
    f
}

#[allow(clippy::too_many_arguments)]
extern "system" fn create_source_voice(
    this: *mut c_void,
    out: *mut *mut c_void,
    format: *const u8,
    flags: u32,
    max_ratio: f32,
    callback: *mut c_void,
    sends: *const c_void,
    effects: *const c_void,
) -> HRESULT {
    // SAFETY: `this` is an engine we adopted.
    let create: CreateSourceVoice =
        unsafe { std::mem::transmute(host_entry(this, CREATE_SOURCE_VOICE)) };
    let Some((channels, rate)) = xma2_format(format) else {
        // SAFETY: the host's, with the title's arguments.
        return unsafe {
            create(
                this, out, format, flags, max_ratio, callback, sends, effects,
            )
        };
    };
    let pcm = pcm_format(channels, rate);
    // SAFETY: as above, with a PCM format of ours.
    let hr = unsafe {
        create(
            this,
            out,
            pcm.as_ptr(),
            flags,
            max_ratio,
            callback,
            sends,
            effects,
        )
    };
    if hr < 0 || out.is_null() {
        return hr;
    }
    // SAFETY: filled by the host on success.
    let voice = unsafe { *out };
    if voice.is_null() {
        return hr;
    }
    // SAFETY: a voice the host just made.
    unsafe {
        XMA_VOICES.adopt(
            voice,
            &[
                (
                    SUBMIT_SOURCE_BUFFER,
                    submit_source_buffer as *const () as usize,
                ),
                (DESTROY_VOICE, destroy_voice as *const () as usize),
            ],
        )
    };
    let state = XmaVoice {
        channels: channels as usize,
        decoder: xma::Decoder::new(channels as usize, rate),
        playing: VecDeque::new(),
    };
    voices().insert(voice as usize, Arc::new(Mutex::new(state)));
    tracing::info!(
        channels,
        rate,
        "an XMA2 source voice: decoded to PCM as its buffers arrive"
    );
    hr
}

// --- XMA2 voices -----------------------------------------------------------------

struct XmaVoice {
    channels: usize,
    decoder: xma::Decoder,
    /// Decoded buffers the host may still be reading, oldest first.
    playing: VecDeque<Box<[i16]>>,
}

fn voices() -> std::sync::MutexGuard<'static, HashMap<usize, Arc<Mutex<XmaVoice>>>> {
    static VOICES: OnceLock<Mutex<HashMap<usize, Arc<Mutex<XmaVoice>>>>> = OnceLock::new();
    VOICES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Decodes the frames of one buffer of XMA2 packets (one stream) and
/// appends them, interleaved, to `out`. Returns the frames that did not
/// decode (their time is kept, as silence).
fn decode_packets(decoder: &mut xma::Decoder, data: &[u8], out: &mut Vec<i16>) -> usize {
    let Some(first) = xma::packet(data, 0) else {
        return 0;
    };
    let mut bad = 0;
    let mut at = 32 + first.first_frame_bit as usize;
    while let Some(frame) = xma::read_frame(data, at) {
        if decoder.decode(&frame.data, frame.bits, out).is_err() {
            bad += 1;
        }
        match frame.next {
            Some(next) if next > at => at = next,
            _ => break,
        }
    }
    bad
}

/// Keeps a play or loop region (`begin`, `length` in samples, 0 = to the
/// end) inside the decoded length, which can differ a little from what the
/// title computed from the stream's sample count.
fn clamp_region(begin: u32, length: u32, frames: u32) -> (u32, u32) {
    if begin >= frames {
        return (0, 0);
    }
    if length == 0 || begin + length <= frames {
        (begin, length)
    } else {
        (begin, frames - begin)
    }
}

extern "system" fn submit_source_buffer(
    this: *mut c_void,
    buffer: *const Buffer,
    wma: *const c_void,
) -> HRESULT {
    // SAFETY: `this` is an XMA2 voice we adopted.
    let submit: SubmitSourceBuffer =
        unsafe { std::mem::transmute(host_entry(this, SUBMIT_SOURCE_BUFFER)) };
    let state = voices().get(&(this as usize)).cloned();
    let (Some(state), false) = (state, buffer.is_null()) else {
        // SAFETY: the host's, with the title's arguments.
        return unsafe { submit(this, buffer, wma) };
    };
    // SAFETY: the title's XAUDIO2_BUFFER; packed, so read unaligned.
    let b = unsafe { buffer.read_unaligned() };
    let data = if b.audio_data.is_null() {
        &[][..]
    } else {
        // SAFETY: the title's buffer of `audio_bytes`.
        unsafe { std::slice::from_raw_parts(b.audio_data, b.audio_bytes as usize) }
    };
    let mut voice = state.lock().unwrap_or_else(|e| e.into_inner());
    forget_played(this, &mut voice);
    let mut pcm = Vec::with_capacity(
        data.len() / xma::PACKET_BYTES * 4 * xma::FRAME_SAMPLES * voice.channels,
    );
    let channels = voice.channels;
    let bad = decode_packets(&mut voice.decoder, data, &mut pcm);
    if bad > 0 {
        tracing::debug!(bad, bytes = data.len(), "XMA2 frames that did not decode");
    }
    if b.flags & END_OF_STREAM != 0 {
        voice.decoder.flush(&mut pcm);
    }
    if pcm.is_empty() {
        // An empty buffer is refused; a sample of silence keeps the title's
        // buffer callbacks coming.
        pcm.resize(channels, 0);
    }
    let pcm = pcm.into_boxed_slice();
    let frames = (pcm.len() / channels) as u32;
    let (play_begin, play_length) = clamp_region(b.play_begin, b.play_length, frames);
    let (loop_begin, loop_length) = if b.loop_count > 0 {
        clamp_region(b.loop_begin, b.loop_length, frames)
    } else {
        (0, 0)
    };
    let ours = Buffer {
        audio_bytes: (pcm.len() * 2) as u32,
        audio_data: pcm.as_ptr().cast(),
        play_begin,
        play_length,
        loop_begin,
        loop_length,
        ..b
    };
    // SAFETY: the host's, with a PCM buffer we keep until it is played.
    let hr = unsafe { submit(this, &ours, std::ptr::null()) };
    if hr >= 0 {
        voice.playing.push_back(pcm);
    }
    hr
}

/// Drops the decoded buffers the host has finished with: all but the
/// newest `BuffersQueued`.
fn forget_played(this: *mut c_void, voice: &mut XmaVoice) {
    // SAFETY: `this` is an XMA2 voice we adopted.
    let get_state: GetState = unsafe { std::mem::transmute(host_entry(this, GET_STATE)) };
    let mut state = VoiceState::default();
    // SAFETY: the host's, into our struct.
    unsafe { get_state(this, &mut state, XAUDIO2_VOICE_NOSAMPLESPLAYED) };
    let queued = state.buffers_queued as usize;
    while voice.playing.len() > queued {
        voice.playing.pop_front();
    }
}

extern "system" fn destroy_voice(this: *mut c_void) {
    // SAFETY: `this` is an XMA2 voice we adopted.
    let destroy: DestroyVoice = unsafe { std::mem::transmute(host_entry(this, DESTROY_VOICE)) };
    // SAFETY: the host's. It returns once its audio thread is done with
    // the voice, so the decoded buffers can go after it.
    unsafe { destroy(this) };
    voices().remove(&(this as usize));
}

pub(crate) fn module() -> HleModule {
    HleModule {
        name: "xaudio2",
        dlls: &["xaudio2_9.dll"],
        exports: vec![
            hle_export!(XAudio2Create),
            HleExport {
                name: "#1",
                address: XAudio2Create as *const () as usize,
            },
            hle_export!(XAudio2CreateWithVersionInfo),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regions_stay_inside_the_decoded_samples() {
        assert_eq!(clamp_region(0, 0, 1000), (0, 0));
        assert_eq!(clamp_region(100, 500, 1000), (100, 500));
        assert_eq!(clamp_region(100, 950, 1000), (100, 900));
        assert_eq!(clamp_region(1200, 10, 1000), (0, 0));
    }

    fn xma2(channels: u16, streams: u16) -> Vec<u8> {
        let mut f = vec![0u8; 52];
        f[0..2].copy_from_slice(&WAVE_FORMAT_XMA2.to_le_bytes());
        f[2..4].copy_from_slice(&channels.to_le_bytes());
        f[4..8].copy_from_slice(&48_000u32.to_le_bytes());
        f[16..18].copy_from_slice(&34u16.to_le_bytes());
        f[18..20].copy_from_slice(&streams.to_le_bytes());
        f
    }

    #[test]
    fn one_stream_xma2_is_ours_the_rest_the_hosts() {
        assert_eq!(xma2_format(xma2(2, 1).as_ptr()), Some((2, 48_000)));
        assert_eq!(xma2_format(xma2(1, 1).as_ptr()), Some((1, 48_000)));
        assert_eq!(xma2_format(xma2(6, 3).as_ptr()), None);
        let pcm = pcm_format(2, 48_000);
        assert_eq!(xma2_format(pcm.as_ptr()), None);
        assert_eq!(&pcm[12..14], &4u16.to_le_bytes());
        assert_eq!(size_of::<Buffer>(), 44);
        assert_eq!(size_of::<VoiceState>(), 20);
    }

    /// The host's XAudio2 with an XMA2 voice: made as PCM, decodes what is
    /// submitted, and is destroyed cleanly. Needs an audio device.
    #[test]
    #[ignore]
    fn the_host_engine_takes_xma2_voices() {
        #[link(name = "ole32")]
        unsafe extern "system" {
            fn CoInitializeEx(reserved: *const c_void, model: u32) -> HRESULT;
        }
        // SAFETY: COM for this thread, as a title's start-up does.
        unsafe { CoInitializeEx(std::ptr::null(), 0) };
        let mut engine = std::ptr::null_mut();
        assert!(XAudio2Create(&mut engine, 0, 1) >= 0);
        type Master = unsafe extern "system" fn(
            *mut c_void,
            *mut *mut c_void,
            u32,
            u32,
            u32,
            *const u16,
            *const c_void,
            u32,
        ) -> HRESULT;
        type Release = unsafe extern "system" fn(*mut c_void) -> u32;
        // SAFETY: the engine's vtable, IXAudio2 slots 7 and 2.
        unsafe {
            let vt = *engine.cast::<*const usize>();
            let master: Master = std::mem::transmute(*vt.add(7));
            let mut mv = std::ptr::null_mut();
            let hr = master(
                engine,
                &mut mv,
                2,
                48_000,
                0,
                std::ptr::null(),
                std::ptr::null(),
                6,
            );
            assert!(hr >= 0, "mastering voice: {:#x}", hr as u32);
            let create: CreateSourceVoice = std::mem::transmute(*vt.add(CREATE_SOURCE_VOICE));
            let mut voice = std::ptr::null_mut();
            let format = xma2(2, 1);
            assert_eq!(
                create(
                    engine,
                    &mut voice,
                    format.as_ptr(),
                    0,
                    2.0,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null()
                ),
                0
            );
            assert!(voices().contains_key(&(voice as usize)));
            let vvt = *voice.cast::<*const usize>();
            let destroy: DestroyVoice = std::mem::transmute(*vvt.add(DESTROY_VOICE));
            destroy(voice);
            assert!(!voices().contains_key(&(voice as usize)));
            let release: Release = std::mem::transmute(*vt.add(2));
            release(engine);
        }
    }
}
