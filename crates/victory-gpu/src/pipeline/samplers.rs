//! The order in which the host first sees the different static samplers,
//! kept the same in every launch.
//!
//! What was measured (RTX 3090 Ti, driver 616.92, the Series S profile, the
//! prologue's cell; first on fxceless/victory-fork, its issue #1459, then on
//! this tree): the title's picture depended on which launches had filled
//! the pipeline library. Here: a cache that launches of the Series S
//! profile with FSR 4 and of the Series X profile at two sizes had filled,
//! then a launch of the Series S profile with the title's own upscaler: the
//! print on the back of the prologue's shirt was missing in every
//! screenshot; the same build on a library of its own had it; and with this
//! module the first cache had it again.
//!
//! There (fxceless's tree): one more static sampler unlike
//! the title's own, in a root signature made before the title's and never
//! bound, was enough to give the main character bright yellow hair and a
//! wall lit in blocks for the whole run, 20 runs of 20, where a launch
//! without it showed dark hair and an evenly dark wall, 12 of 12. A sampler
//! equal to one of the title's in all a filter makes use of did nothing.
//! The same build with the same switches showed the wrong picture on a
//! library that launches of another configuration had filled, and the right
//! one from an empty library and from the library that launch left.
//!
//! The reading that fits all of it: the host numbers the different static
//! samplers of a device in the order they are first made, a compiled
//! pipeline names them by number, and a pipeline kept in the library keeps
//! the numbers of the launch that compiled it. A launch that makes its
//! static samplers in another order then loads pipelines that sample with
//! the wrong ones (the title's shadow lookups without their comparison,
//! for one). The library's own check passes: the descriptions are equal.
//!
//! So the order is made the same in every launch. The samplers this cache
//! has ever seen are listed in a file beside the library, in the order they
//! were first made, and before the first root signature of a launch each of
//! them is made again in that order, alone in a root signature that lives
//! for the whole run. A sampler not in the list yet goes to its end, and
//! its own root signature is made under the same lock before the one that
//! asked for it, so the file's order is the host's.
//!
//! A library filled before there was a list is not used: its pipelines may
//! carry any order (`crate::pipeline::library`).
//!
//! `VICTORY_SAMPLER_ORDER=0`: nothing of this (as before).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

/// The list's file, beside the library's.
const LIST_FILE: &str = "static-samplers.txt";

/// A static sampler's state, without where a shader finds it (its register,
/// space and visibility): filter, three address modes, the bias's bits,
/// maximum anisotropy, comparison function, border colour, the bits of the
/// first and the last level.
type Key = [u32; 10];

/// Whether the order is kept (`VICTORY_SAMPLER_ORDER=0`: not).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SAMPLER_ORDER").is_none_or(|v| v != "0"))
}

/// Whether `dir` held a list when this launch first asked: a library there
/// was then filled with it. Asked once, before the list is written
/// ([`before`] asks first), so that the answer is the same whichever of
/// the library and the first root signature comes first.
pub(crate) fn listed_at_start(dir: &Path) -> bool {
    static LISTED: OnceLock<bool> = OnceLock::new();
    *LISTED.get_or_init(|| dir.join(LIST_FILE).exists())
}

fn key(s: &D3D12_STATIC_SAMPLER_DESC) -> Key {
    [
        s.Filter.0 as u32,
        s.AddressU.0 as u32,
        s.AddressV.0 as u32,
        s.AddressW.0 as u32,
        s.MipLODBias.to_bits(),
        s.MaxAnisotropy,
        s.ComparisonFunc.0 as u32,
        s.BorderColor.0 as u32,
        s.MinLOD.to_bits(),
        s.MaxLOD.to_bits(),
    ]
}

fn sampler(k: &Key) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER(k[0] as i32),
        AddressU: D3D12_TEXTURE_ADDRESS_MODE(k[1] as i32),
        AddressV: D3D12_TEXTURE_ADDRESS_MODE(k[2] as i32),
        AddressW: D3D12_TEXTURE_ADDRESS_MODE(k[3] as i32),
        MipLODBias: f32::from_bits(k[4]),
        MaxAnisotropy: k[5],
        ComparisonFunc: D3D12_COMPARISON_FUNC(k[6] as i32),
        BorderColor: D3D12_STATIC_BORDER_COLOR(k[7] as i32),
        MinLOD: f32::from_bits(k[8]),
        MaxLOD: f32::from_bits(k[9]),
        ShaderRegister: 0,
        RegisterSpace: 0,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }
}

/// The list's line for a sampler: its ten words in hexadecimal.
fn line(k: &Key) -> String {
    k.iter().map(|w| format!("{w:08x}")).collect::<Vec<_>>().join(" ")
}

/// The samplers a list's text names, in its order, each once. A line that
/// is not ten hexadecimal words is left out (a file cut short).
fn parse(text: &str) -> Vec<Key> {
    let mut seen = HashSet::new();
    text.lines()
        .filter_map(|l| {
            let words: Vec<u32> = l.split_whitespace().map(|w| u32::from_str_radix(w, 16)).collect::<Result<_, _>>().ok()?;
            Key::try_from(words).ok()
        })
        .filter(|k| seen.insert(*k))
        .collect()
}

/// A root signature of nothing but the sampler.
fn alone(device: &ID3D12Device, k: &Key) -> windows::core::Result<ID3D12RootSignature> {
    let samplers = [sampler(k)];
    let desc = D3D12_ROOT_SIGNATURE_DESC {
        NumParameters: 0,
        pParameters: std::ptr::null(),
        NumStaticSamplers: 1,
        pStaticSamplers: samplers.as_ptr(),
        Flags: D3D12_ROOT_SIGNATURE_FLAG_NONE,
    };
    let mut blob: Option<ID3DBlob> = None;
    // SAFETY: the description points at the live array above; the blob
    // owns its bytes.
    unsafe {
        D3D12SerializeRootSignature(&desc, D3D_ROOT_SIGNATURE_VERSION_1, &mut blob, None)?;
        let blob = blob.ok_or_else(windows::core::Error::empty)?;
        let bytes = std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        device.CreateRootSignature(0, bytes)
    }
}

/// The samplers made so far, in the list's order, and the root signatures
/// that keep them made.
struct Order {
    seen: HashSet<Key>,
    kept: Vec<ID3D12RootSignature>,
    /// The list's file, if there is a cache folder.
    file: Option<PathBuf>,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Order {}

impl Order {
    /// Makes the samplers the list in `dir` names, in its order.
    fn start(device: &ID3D12Device, dir: Option<&Path>) -> Order {
        let file = dir.map(|d| d.join(LIST_FILE));
        let keys = file.as_ref().and_then(|f| std::fs::read_to_string(f).ok()).map(|t| parse(&t)).unwrap_or_default();
        let mut order = Order { seen: HashSet::new(), kept: Vec::new(), file };
        for k in &keys {
            order.make(device, k);
        }
        // The file as it is read from now on: whole lines, each sampler
        // once (and a file where there was none: the library beside it is
        // filled with this order).
        order.save(&keys);
        tracing::info!(
            samplers = keys.len(),
            "static samplers are made in the order of this cache's list before anything else (VICTORY_SAMPLER_ORDER=0: not)"
        );
        order
    }

    fn make(&mut self, device: &ID3D12Device, k: &Key) {
        if !self.seen.insert(*k) {
            return;
        }
        match alone(device, k) {
            Ok(root) => self.kept.push(root),
            // (Its number is then taken when the root signature that asked
            // for it is made, which comes right after, under the lock.)
            Err(e) => tracing::warn!(error = %e, sampler = line(k), "a static sampler could not be made alone"),
        }
    }

    fn save(&self, keys: &[Key]) {
        let Some(file) = &self.file else { return };
        let text: String = keys.iter().map(|k| line(k) + "\n").collect();
        if let Err(e) = std::fs::write(file, text) {
            tracing::warn!(error = %e, "the list of static samplers was not written: the next launch may make them in another order");
        }
    }

    /// One more sampler at the list's end.
    fn add(&mut self, device: &ID3D12Device, k: &Key) {
        self.make(device, k);
        let Some(file) = &self.file else { return };
        use std::io::Write;
        let written = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(file)
            .and_then(|mut f| f.write_all((line(k) + "\n").as_bytes()));
        if let Err(e) = written {
            tracing::warn!(error = %e, "the list of static samplers was not added to: the next launch may make them in another order");
        }
    }
}

static ORDER: Mutex<Option<Order>> = Mutex::new(None);

/// Called before a root signature with `samplers` is made on `device`
/// (also with none: the first call of a launch makes the list's). `dir`:
/// the folder of the pipeline library, if there is one.
pub(crate) fn before(device: &ID3D12Device, samplers: &[D3D12_STATIC_SAMPLER_DESC], dir: Option<&Path>) {
    if !on() {
        #[cfg(feature = "diagnostics")]
        extra(device, None);
        return;
    }
    if let Some(dir) = dir {
        listed_at_start(dir);
    }
    let mut order = ORDER.lock().unwrap_or_else(|e| e.into_inner());
    let first = order.is_none();
    let order = order.get_or_insert_with(|| Order::start(device, dir));
    #[cfg(feature = "diagnostics")]
    if first {
        extra(device, Some(order));
    }
    #[cfg(not(feature = "diagnostics"))]
    let _ = first;
    for s in samplers {
        let k = key(s);
        if !order.seen.contains(&k) {
            order.add(device, &k);
        }
    }
}

/// For reproducing fxceless/victory-fork#1459 (diagnostics builds,
/// `VICTORY_EXTRA_STATIC_SAMPLER=1`): one more static sampler, unlike the
/// title's own, is made before the title's first root signature. With the
/// order kept it goes through the list like any other (`order`); without,
/// it is made alone and kept, as a pass of the layer's with a sampler of
/// its own would be.
#[cfg(feature = "diagnostics")]
fn extra(device: &ID3D12Device, order: Option<&mut Order>) {
    static ASKED: OnceLock<bool> = OnceLock::new();
    static MADE: std::sync::Once = std::sync::Once::new();
    if !*ASKED.get_or_init(|| std::env::var_os("VICTORY_EXTRA_STATIC_SAMPLER").is_some_and(|v| v == "1")) {
        return;
    }
    // Linear, clamped, and the root signature language's values for the rest.
    let k = key(&D3D12_STATIC_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 16,
        ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
        BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
        MaxLOD: f32::MAX,
        ..Default::default()
    });
    match order {
        Some(order) => {
            if !order.seen.contains(&k) {
                order.add(device, &k);
                tracing::info!("one more static sampler was added to the list before the title's first root signature (VICTORY_EXTRA_STATIC_SAMPLER=1)");
            }
        }
        None => MADE.call_once(|| match alone(device, &k) {
            Ok(root) => {
                tracing::info!("one more static sampler was made before the title's first root signature, outside any list (VICTORY_EXTRA_STATIC_SAMPLER=1, VICTORY_SAMPLER_ORDER=0)");
                std::mem::forget(root);
            }
            Err(e) => tracing::warn!(error = %e, "the extra static sampler could not be made"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some(filter: i32, last: f32) -> D3D12_STATIC_SAMPLER_DESC {
        D3D12_STATIC_SAMPLER_DESC {
            Filter: D3D12_FILTER(filter),
            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
            AddressV: D3D12_TEXTURE_ADDRESS_MODE_BORDER,
            AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
            MipLODBias: -0.5,
            MaxAnisotropy: 4,
            ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
            BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
            MinLOD: 0.0,
            MaxLOD: last,
            ShaderRegister: 7,
            RegisterSpace: 3,
            ShaderVisibility: D3D12_SHADER_VISIBILITY_PIXEL,
        }
    }

    /// A sampler comes back from its line as it went in, in all a key
    /// holds; where a shader finds it is not part of the key.
    #[test]
    fn a_sampler_comes_back_from_its_line() {
        let s = some(0x95, 12.0);
        let k = key(&s);
        assert_eq!(parse(&line(&k)), vec![k]);
        let back = sampler(&k);
        assert_eq!(key(&back), k);
        assert_eq!((back.Filter, back.AddressV, back.MaxAnisotropy), (s.Filter, s.AddressV, s.MaxAnisotropy));
        assert_eq!((back.MipLODBias.to_bits(), back.MaxLOD.to_bits()), (s.MipLODBias.to_bits(), s.MaxLOD.to_bits()));
        assert_eq!((back.ShaderRegister, back.RegisterSpace, back.ShaderVisibility), (0, 0, D3D12_SHADER_VISIBILITY_ALL));
        let mut elsewhere = s;
        (elsewhere.ShaderRegister, elsewhere.RegisterSpace, elsewhere.ShaderVisibility) = (1, 0, D3D12_SHADER_VISIBILITY_VERTEX);
        assert_eq!(key(&elsewhere), k);
    }

    /// A list keeps its order, names each sampler once and leaves out a
    /// line cut short.
    #[test]
    fn a_list_keeps_its_order() {
        let (a, b, c) = (key(&some(0x15, 12.0)), key(&some(0x14, 12.0)), key(&some(0x15, f32::MAX)));
        let text = format!("{}\n{}\n{}\n{}\n{}", line(&a), line(&b), line(&a), line(&c), &line(&b)[..40]);
        assert_eq!(parse(&text), vec![a, b, c]);
        assert_eq!(parse(""), Vec::<Key>::new());
        assert_eq!(parse("not a sampler\n"), Vec::<Key>::new());
    }

    /// On a device: the list's samplers are made at the first call, a new
    /// one is added to the file's end, and a second start reads the same
    /// order back.
    #[test]
    fn a_new_sampler_goes_to_the_end_of_the_file() {
        let Some(host) = crate::host::host() else {
            return; // no host device
        };
        let dir = std::env::temp_dir().join(format!("victory-samplers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a folder for the test");
        let (a, b, c) = (some(0x15, 12.0), some(0x14, 12.0), some(0x15, f32::MAX));
        std::fs::write(dir.join(LIST_FILE), format!("{}\n{}\n", line(&key(&a)), line(&key(&b)))).expect("a list");
        let mut order = Order::start(&host.device, Some(&dir));
        assert_eq!(order.kept.len(), 2);
        assert!(order.seen.contains(&key(&a)) && order.seen.contains(&key(&b)) && !order.seen.contains(&key(&c)));
        order.add(&host.device, &key(&c));
        // (A second asking adds nothing.)
        if !order.seen.contains(&key(&c)) {
            order.add(&host.device, &key(&c));
        }
        assert_eq!(order.kept.len(), 3);
        let text = std::fs::read_to_string(dir.join(LIST_FILE)).expect("the list");
        assert_eq!(parse(&text), vec![key(&a), key(&b), key(&c)]);
        let again = Order::start(&host.device, Some(&dir));
        assert_eq!(again.kept.len(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
