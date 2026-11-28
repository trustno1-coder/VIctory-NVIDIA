//! The DSP's convolution, computed on the CPU.
//!
//! The title hands over each impulse response already in the coprocessor's
//! layout. Read from the responses the retail title passes to `XDspActivate`
//! (every block below checked against its neighbours): blocks of 2048 floats, each
//! the unscaled 2048-point FFT of a 2048-sample window of the response that
//! starts one block (1024 samples) before that block's own 1024 samples
//! (window k covers samples (k-1)*1024 .. (k+1)*1024, zeros before 0). The
//! bins are interleaved re/im, 0 to 1023 in natural order; bin 0's imaginary
//! slot is 0 (the Nyquist bin is not kept). A last block that starts with
//! the word 0xFFFFACE1 holds the hardware's schedule and is not response data.
//!
//! With windows like that, an input block zero-padded to 2048 samples and
//! multiplied with window k gives, in the second half of the inverse FFT,
//! that block's part of the output k blocks later: each output block is the
//! second half of IFFT(sum over k of X[t-k] * H[k]) / 2048. How the hardware
//! scales its result is not known; `dsp::GAIN` makes up for it.
//!
//! Worked out for vectors (`VICTORY_DSP_CONVOLVE` as before):
//! - real-input FFTs (`realfft`: a 1024-point complex FFT and a twist), not a
//!   2048-point complex FFT of a signal whose imaginary half is all zeros:
//!   eight times faster, measured;
//! - the spectra split (re / im), one row of 1024 bins per window, every row
//!   64-byte aligned (a whole number of cache lines, so no vector load is
//!   split across two of them; misaligned rows were 1.2-1.4 times slower);
//! - the sum over windows register-blocked (with AVX2 and FMA): 32 bins'
//!   sums stay in eight registers while the loop runs over every window,
//!   instead of the sum being loaded and stored again for each window
//!   (1.3-2 times faster). Bins are the vector lanes because they are what
//!   lies contiguous; with windows contiguous instead ([bin][window]) the
//!   same sum was six times slower.
//!
//! A block costs 1.7-6.5 times less than before (fewer windows: more).

const N: usize = 2048;
const B: usize = 1024;
const TRAILER: u32 = 0xFFFF_ACE1;

/// A complex radix-2 FFT of 2048 points (for [`layout`], the tests' builder).
#[cfg(test)]
struct Fft {
    cos: Vec<f32>,
    sin: Vec<f32>,
    rev: Vec<u16>,
}

#[cfg(test)]
fn fft() -> &'static Fft {
    static FFT: std::sync::OnceLock<Fft> = std::sync::OnceLock::new();
    FFT.get_or_init(|| {
        let bits = N.trailing_zeros();
        let rev = (0..N).map(|i| (i.reverse_bits() >> (usize::BITS - bits)) as u16).collect();
        let (cos, sin) = (0..N / 2)
            .map(|k| {
                let a = -2.0 * std::f64::consts::PI * k as f64 / N as f64;
                (a.cos() as f32, a.sin() as f32)
            })
            .unzip();
        Fft { cos, sin, rev }
    })
}

#[cfg(test)]
impl Fft {
    /// In place; `inverse` flips the sign of the exponent and does not scale.
    fn run(&self, re: &mut [f32; N], im: &mut [f32; N], inverse: bool) {
        for i in 0..N {
            let j = self.rev[i] as usize;
            if i < j {
                re.swap(i, j);
                im.swap(i, j);
            }
        }
        let sign = if inverse { -1.0 } else { 1.0 };
        let mut len = 2;
        while len <= N {
            let half = len / 2;
            let step = N / len;
            for start in (0..N).step_by(len) {
                for k in 0..half {
                    let (wr, wi) = (self.cos[k * step], sign * self.sin[k * step]);
                    let (a, b) = (start + k, start + k + half);
                    let (br, bi) = (re[b] * wr - im[b] * wi, re[b] * wi + im[b] * wr);
                    re[b] = re[a] - br;
                    im[b] = im[a] - bi;
                    re[a] += br;
                    im[a] += bi;
                }
            }
            len *= 2;
        }
    }
}

/// 64-byte aligned floats: rows of the spectra start on a cache line.
#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line([f32; 16]);

struct Aligned(Vec<Line>);

impl Aligned {
    fn zeros(floats: usize) -> Aligned {
        Aligned(vec![Line([0.0; 16]); floats.div_ceil(16)])
    }
    fn get(&self) -> &[f32] {
        // SAFETY: `Line` is 16 floats with no padding.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * 16) }
    }
    fn get_mut(&mut self) -> &mut [f32] {
        // SAFETY: as above.
        unsafe { std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), self.0.len() * 16) }
    }
    /// The same memory as half floats (two to a float).
    fn halves(&self) -> &[u16] {
        // SAFETY: `Line` is 32 u16s' worth of bytes with no padding.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * 32) }
    }
}

/// Whether the response is kept as half floats (`VICTORY_DSP_HALF=0`: as
/// floats). Half the bytes to bring in from memory for every block: 1.1-1.4
/// times faster where the response is long (measured), the sum then within
/// about 2e-4 of its peak (-73 dB), which no one hears. Needs F16C.
fn half_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        #[cfg(target_arch = "x86_64")]
        let can = std::arch::is_x86_feature_detected!("avx2")
            && std::arch::is_x86_feature_detected!("fma")
            && std::arch::is_x86_feature_detected!("f16c");
        #[cfg(not(target_arch = "x86_64"))]
        let can = false;
        can && std::env::var_os("VICTORY_DSP_HALF").is_none_or(|v| v != "0")
    })
}

/// The real FFTs of 2048 points, forward and inverse, planned once.
fn plans() -> &'static (std::sync::Arc<dyn realfft::RealToComplex<f32>>, std::sync::Arc<dyn realfft::ComplexToReal<f32>>) {
    static PLANS: std::sync::OnceLock<(std::sync::Arc<dyn realfft::RealToComplex<f32>>, std::sync::Arc<dyn realfft::ComplexToReal<f32>>)> =
        std::sync::OnceLock::new();
    PLANS.get_or_init(|| {
        let mut planner = realfft::RealFftPlanner::<f32>::new();
        (planner.plan_fft_forward(N), planner.plan_fft_inverse(N))
    })
}

/// One mono convolution stream's state.
pub(super) struct Convolver {
    /// The response's windows: `parts` rows of 1024 bins, split re/im.
    h_re: Aligned,
    h_im: Aligned,
    /// The same as half floats instead ([`half_on`]; the floats are then empty).
    half: Option<(Aligned, Aligned)>,
    parts: usize,
    /// The last `parts` input spectra, a ring of rows; `head` is the newest.
    x_re: Aligned,
    x_im: Aligned,
    head: usize,
    /// Input blocks in a row that were silent: once the ring holds only
    /// silence, the output is silence without any arithmetic.
    quiet: usize,
    /// Per window, the ring row it meets this block (newest first).
    rows: Vec<usize>,
    y_re: Aligned,
    y_im: Aligned,
    time: Vec<f32>,
    spectrum: Vec<realfft::num_complex::Complex32>,
    scratch: Vec<realfft::num_complex::Complex32>,
}

impl Convolver {
    /// `None` when the response is not in the layout described above.
    pub(super) fn new(ir: &[f32]) -> Option<Self> {
        Self::with_half(ir, half_on())
    }

    /// [`Convolver::new`], the response as half floats or not.
    fn with_half(ir: &[f32], half: bool) -> Option<Self> {
        if ir.is_empty() || !ir.len().is_multiple_of(N) {
            return None;
        }
        let mut blocks = ir.len() / N;
        if ir[(blocks - 1) * N].to_bits() == TRAILER {
            blocks -= 1;
        }
        // trailing windows of zeros add nothing
        while blocks > 0 && ir[(blocks - 1) * N..blocks * N].iter().all(|&v| v == 0.0) {
            blocks -= 1;
        }
        let parts = blocks.max(1);
        let mut h_re = Aligned::zeros(parts * B);
        let mut h_im = Aligned::zeros(parts * B);
        for k in 0..blocks {
            let block = &ir[k * N..(k + 1) * N];
            for bin in 0..B {
                let (r, i) = (block[2 * bin], block[2 * bin + 1]);
                // a non-finite value is not response data; leave it out
                h_re.get_mut()[k * B + bin] = if r.is_finite() { r } else { 0.0 };
                h_im.get_mut()[k * B + bin] = if i.is_finite() { i } else { 0.0 };
            }
        }
        #[cfg(target_arch = "x86_64")]
        let half = half.then(|| {
            let to_half = |from: &Aligned| {
                let mut to = Aligned::zeros(parts * B / 2);
                // SAFETY: F16C is there (`half_on`); both hold parts * B values.
                unsafe { to_halves(&from.get()[..parts * B], to.0.as_mut_ptr().cast()) };
                to
            };
            let pair = (to_half(&h_re), to_half(&h_im));
            (h_re, h_im) = (Aligned::zeros(0), Aligned::zeros(0));
            pair
        });
        #[cfg(not(target_arch = "x86_64"))]
        let half = half.then(|| unreachable!());
        let (forward, inverse) = plans();
        let scratch = forward.get_scratch_len().max(inverse.get_scratch_len());
        Some(Self {
            h_re,
            h_im,
            half,
            parts,
            x_re: Aligned::zeros(parts * B),
            x_im: Aligned::zeros(parts * B),
            head: 0,
            quiet: usize::MAX,
            rows: vec![0; parts],
            y_re: Aligned::zeros(B),
            y_im: Aligned::zeros(B),
            time: vec![0.0; N],
            spectrum: vec![Default::default(); B + 1],
            scratch: vec![Default::default(); scratch],
        })
    }

    /// One block: 1024 input samples in, 1024 output samples out, times `gain`.
    pub(super) fn process(&mut self, input: &[f32], output: &mut [f32], gain: f32) {
        let silent = input.iter().all(|&v| v == 0.0 || !v.is_finite());
        self.quiet = if silent { self.quiet.saturating_add(1) } else { 0 };
        self.head = (self.head + 1) % self.parts;
        let slot = self.head * B;
        let (forward, inverse) = plans();
        if silent {
            self.x_re.get_mut()[slot..slot + B].fill(0.0);
            self.x_im.get_mut()[slot..slot + B].fill(0.0);
            if self.quiet >= self.parts {
                output.fill(0.0);
                return;
            }
        } else {
            for (t, &v) in self.time[..B].iter_mut().zip(input) {
                *t = if v.is_finite() { v } else { 0.0 };
            }
            self.time[B..].fill(0.0);
            if forward.process_with_scratch(&mut self.time, &mut self.spectrum, &mut self.scratch).is_err() {
                output.fill(0.0);
                return;
            }
            let (xr, xi) = (&mut self.x_re.get_mut()[slot..slot + B], &mut self.x_im.get_mut()[slot..slot + B]);
            for (bin, c) in self.spectrum[..B].iter().enumerate() {
                xr[bin] = c.re;
                xi[bin] = c.im;
            }
        }
        // sum over k of X[t-k] * H[k]
        for (k, row) in self.rows.iter_mut().enumerate() {
            *row = ((self.head + self.parts - k) % self.parts) * B;
        }
        let (xr, xi, hr, hi) = (self.x_re.get(), self.x_im.get(), self.h_re.get(), self.h_im.get());
        let (yr, yi) = (self.y_re.get_mut(), self.y_im.get_mut());
        #[cfg(target_arch = "x86_64")]
        let done = if let Some((hr, hi)) = &self.half {
            // SAFETY: F16C, AVX2 and FMA are there (`half_on`); rows as below,
            // the response's rows of half floats from 64-byte boundaries too.
            unsafe { sum_avx2_half(xr, xi, hr.halves(), hi.halves(), &self.rows, yr, yi) };
            true
        } else if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") {
            // SAFETY: the features are there; every row holds B (a multiple of 32) floats from a 64-byte boundary.
            unsafe { sum_avx2(xr, xi, hr, hi, &self.rows, yr, yi) };
            true
        } else {
            false
        };
        #[cfg(not(target_arch = "x86_64"))]
        let done = false;
        if !done {
            sum(xr, xi, hr, hi, &self.rows, yr, yi);
        }
        // the spectrum of a real signal: bins 0 to 1023 (Nyquist bin stays 0)
        for (bin, c) in self.spectrum[..B].iter_mut().enumerate() {
            *c = realfft::num_complex::Complex32::new(yr[bin], yi[bin]);
        }
        self.spectrum[0].im = 0.0;
        self.spectrum[B] = Default::default();
        if inverse.process_with_scratch(&mut self.spectrum, &mut self.time, &mut self.scratch).is_err() {
            output.fill(0.0);
            return;
        }
        let scale = gain / N as f32;
        for (o, &v) in output.iter_mut().zip(&self.time[B..]) {
            *o = v * scale;
        }
    }
}

/// The sum over windows, one window at a time (without AVX2 and FMA).
fn sum(xr: &[f32], xi: &[f32], hr: &[f32], hi: &[f32], rows: &[usize], yr: &mut [f32], yi: &mut [f32]) {
    yr[..B].fill(0.0);
    yi[..B].fill(0.0);
    for (k, &x) in rows.iter().enumerate() {
        let h = k * B;
        let (xr, xi) = (&xr[x..x + B], &xi[x..x + B]);
        let (hr, hi) = (&hr[h..h + B], &hi[h..h + B]);
        for bin in 0..B {
            yr[bin] += xr[bin] * hr[bin] - xi[bin] * hi[bin];
            yi[bin] += xr[bin] * hi[bin] + xi[bin] * hr[bin];
        }
    }
}

/// Floats to half floats, eight at a time (`n` a multiple of 8).
///
/// # Safety
/// F16C; `to` holds `from.len()` u16s.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "f16c,avx")]
unsafe fn to_halves(from: &[f32], to: *mut u16) {
    use std::arch::x86_64::*;
    for (k, eight) in from.chunks_exact(8).enumerate() {
        // SAFETY: inside both (the caller's).
        unsafe { _mm_storeu_si128(to.add(8 * k).cast(), _mm256_cvtps_ph::<_MM_FROUND_TO_NEAREST_INT>(_mm256_loadu_ps(eight.as_ptr()))) };
    }
}

/// [`sum_avx2`] with the response as half floats, widened as they are loaded.
///
/// # Safety
/// F16C, AVX2 and FMA; as [`sum_avx2`], `hr` and `hi` holding the response's
/// rows of `B` halves each from 64-byte boundaries.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma,f16c")]
unsafe fn sum_avx2_half(xr: &[f32], xi: &[f32], hr: &[u16], hi: &[u16], rows: &[usize], yr: &mut [f32], yi: &mut [f32]) {
    use std::arch::x86_64::*;
    assert!(yr.len() >= B && yi.len() >= B && hr.len() >= rows.len() * B && hi.len() >= rows.len() * B);
    assert!(rows.iter().all(|&r| r + B <= xr.len() && r + B <= xi.len()));
    let mut bin = 0;
    while bin < B {
        let zero = _mm256_setzero_ps();
        let (mut r0, mut r1, mut r2, mut r3) = (zero, zero, zero, zero);
        let (mut i0, mut i1, mut i2, mut i3) = (zero, zero, zero, zero);
        for (k, &row) in rows.iter().enumerate() {
            // SAFETY: inside the slices (asserted above), on 16- and 32-byte boundaries.
            unsafe {
                let (px, pxi) = (xr.as_ptr().add(row + bin), xi.as_ptr().add(row + bin));
                let (ph, phi) = (hr.as_ptr().add(k * B + bin), hi.as_ptr().add(k * B + bin));
                macro_rules! eight {
                    ($o:expr, $r:ident, $i:ident) => {{
                        let (a, b) = (_mm256_load_ps(px.add($o)), _mm256_load_ps(pxi.add($o)));
                        let c = _mm256_cvtph_ps(_mm_load_si128(ph.add($o).cast()));
                        let d = _mm256_cvtph_ps(_mm_load_si128(phi.add($o).cast()));
                        $r = _mm256_fnmadd_ps(b, d, _mm256_fmadd_ps(a, c, $r));
                        $i = _mm256_fmadd_ps(b, c, _mm256_fmadd_ps(a, d, $i));
                    }};
                }
                eight!(0, r0, i0);
                eight!(8, r1, i1);
                eight!(16, r2, i2);
                eight!(24, r3, i3);
            }
        }
        // SAFETY: inside `yr` and `yi`, on 32-byte boundaries.
        unsafe {
            let (py, pyi) = (yr.as_mut_ptr().add(bin), yi.as_mut_ptr().add(bin));
            _mm256_store_ps(py, r0);
            _mm256_store_ps(py.add(8), r1);
            _mm256_store_ps(py.add(16), r2);
            _mm256_store_ps(py.add(24), r3);
            _mm256_store_ps(pyi, i0);
            _mm256_store_ps(pyi.add(8), i1);
            _mm256_store_ps(pyi.add(16), i2);
            _mm256_store_ps(pyi.add(24), i3);
        }
        bin += 32;
    }
}

/// The sum over windows, 32 bins at a time with their sums in registers
/// while every window goes by; aligned loads.
///
/// # Safety
/// AVX2 and FMA; `rows` and the response's rows hold `B` floats each from a
/// 64-byte boundary inside the slices; `yr`, `yi` hold `B` from one.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn sum_avx2(xr: &[f32], xi: &[f32], hr: &[f32], hi: &[f32], rows: &[usize], yr: &mut [f32], yi: &mut [f32]) {
    use std::arch::x86_64::*;
    assert!(yr.len() >= B && yi.len() >= B && hr.len() >= rows.len() * B && hi.len() >= rows.len() * B);
    assert!(rows.iter().all(|&r| r + B <= xr.len() && r + B <= xi.len()));
    let mut bin = 0;
    while bin < B {
        let zero = _mm256_setzero_ps();
        let (mut r0, mut r1, mut r2, mut r3) = (zero, zero, zero, zero);
        let (mut i0, mut i1, mut i2, mut i3) = (zero, zero, zero, zero);
        for (k, &row) in rows.iter().enumerate() {
            // SAFETY: inside the slices (asserted above), on 32-byte boundaries.
            unsafe {
                let (px, pxi) = (xr.as_ptr().add(row + bin), xi.as_ptr().add(row + bin));
                let (ph, phi) = (hr.as_ptr().add(k * B + bin), hi.as_ptr().add(k * B + bin));
                macro_rules! eight {
                    ($o:expr, $r:ident, $i:ident) => {{
                        let (a, b) = (_mm256_load_ps(px.add($o)), _mm256_load_ps(pxi.add($o)));
                        let (c, d) = (_mm256_load_ps(ph.add($o)), _mm256_load_ps(phi.add($o)));
                        $r = _mm256_fnmadd_ps(b, d, _mm256_fmadd_ps(a, c, $r));
                        $i = _mm256_fmadd_ps(b, c, _mm256_fmadd_ps(a, d, $i));
                    }};
                }
                eight!(0, r0, i0);
                eight!(8, r1, i1);
                eight!(16, r2, i2);
                eight!(24, r3, i3);
            }
        }
        // SAFETY: inside `yr` and `yi`, on 32-byte boundaries.
        unsafe {
            let (py, pyi) = (yr.as_mut_ptr().add(bin), yi.as_mut_ptr().add(bin));
            _mm256_store_ps(py, r0);
            _mm256_store_ps(py.add(8), r1);
            _mm256_store_ps(py.add(16), r2);
            _mm256_store_ps(py.add(24), r3);
            _mm256_store_ps(pyi, i0);
            _mm256_store_ps(pyi.add(8), i1);
            _mm256_store_ps(pyi.add(16), i2);
            _mm256_store_ps(pyi.add(24), i3);
        }
        bin += 32;
    }
}

/// Builds a response in the hardware layout from time-domain samples, as
/// the title's responses were found to be laid out.
#[cfg(test)]
pub(super) fn layout(ir: &[f32]) -> Vec<f32> {
    // one window more than the response has blocks: its first half holds the last block
    let blocks = ir.len().div_ceil(B) + 1;
    let mut out = vec![0f32; (blocks + 1) * N];
    for k in 0..blocks {
        let mut re = [0f32; N];
        let mut im = [0f32; N];
        for (i, r) in re.iter_mut().enumerate() {
            let t = (k * B + i) as isize - B as isize;
            if t >= 0 && (t as usize) < ir.len() {
                *r = ir[t as usize];
            }
        }
        fft().run(&mut re, &mut im, false);
        for bin in 0..B {
            out[k * N + 2 * bin] = re[bin];
            out[k * N + 2 * bin + 1] = if bin == 0 { 0.0 } else { im[bin] };
        }
    }
    out[blocks * N] = f32::from_bits(TRAILER);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_a_direct_convolution_across_blocks() {
        let ir: Vec<f32> = (0..2500).map(|i| ((i * 37 % 101) as f32 - 50.0) / 400.0 * (-(i as f32) / 900.0).exp()).collect();
        let mut c = Convolver::new(&layout(&ir)).unwrap();
        let input: Vec<f32> = (0..B * 5).map(|i| ((i * 13 % 29) as f32 - 14.0) / 20.0).collect();
        let mut got = vec![0f32; B * 5];
        for (i, o) in input.chunks(B).zip(got.chunks_mut(B)) {
            c.process(i, o, 1.0);
        }
        for n in 0..B * 5 {
            let want: f32 = (0..=n).filter(|&m| n - m < ir.len()).map(|m| input[m] * ir[n - m]).sum();
            assert!((got[n] - want).abs() < 2e-3, "sample {n}: {} vs {want}", got[n]);
        }
    }

    #[test]
    fn the_register_blocked_sum_is_the_plain_one() {
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma") {
            let parts = 5;
            let fill = |seed: u32| {
                let mut a = Aligned::zeros(parts * B);
                for (i, v) in a.get_mut().iter_mut().enumerate() {
                    *v = ((i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed) >> 20) as f32 / 4096.0 - 0.5;
                }
                a
            };
            let (xr, xi, hr, hi) = (fill(1), fill(2), fill(3), fill(4));
            let rows: Vec<usize> = (0..parts).map(|k| ((3 + parts - k) % parts) * B).collect();
            let (mut ar, mut ai, mut br, mut bi) = (Aligned::zeros(B), Aligned::zeros(B), Aligned::zeros(B), Aligned::zeros(B));
            sum(xr.get(), xi.get(), hr.get(), hi.get(), &rows, ar.get_mut(), ai.get_mut());
            // SAFETY: the features are there; aligned rows.
            unsafe { sum_avx2(xr.get(), xi.get(), hr.get(), hi.get(), &rows, br.get_mut(), bi.get_mut()) };
            for (a, b) in ar.get().iter().zip(br.get()).chain(ai.get().iter().zip(bi.get())) {
                assert!((a - b).abs() < 1e-5, "{a} vs {b}");
            }
        }
    }

    #[test]
    fn half_floats_give_the_same_sound() {
        if !half_on() {
            return;
        }
        let ir: Vec<f32> = (0..48_000).map(|i| (((i as u32).wrapping_mul(2_654_435_761) >> 20) as f32 / 4096.0 - 0.5) * (-(i as f32) / 12_000.0).exp() * 0.05).collect();
        let lay = layout(&ir);
        let (mut full, mut half) = (Convolver::with_half(&lay, false).unwrap(), Convolver::with_half(&lay, true).unwrap());
        assert!(half.half.is_some() && full.half.is_none());
        let (mut peak, mut most) = (0f32, 0f32);
        for blk in 0..12 {
            let input: Vec<f32> = (0..B).map(|i| (((i * 31 + blk * 7) % 97) as f32 - 48.0) / 100.0).collect();
            let (mut a, mut b) = (vec![0f32; B], vec![0f32; B]);
            full.process(&input, &mut a, 1.0);
            half.process(&input, &mut b, 1.0);
            for (x, y) in a.iter().zip(&b) {
                peak = peak.max(x.abs());
                most = most.max((x - y).abs());
            }
        }
        // -60 dB of the peak at most (measured: about -73)
        assert!(most <= peak * 1e-3, "{most} of {peak}");
    }

    #[test]
    fn a_silent_ring_gives_silence() {
        let mut c = Convolver::new(&layout(&[1.0, 0.5])).unwrap();
        let mut out = vec![1f32; B];
        for _ in 0..4 {
            c.process(&[0.0; B], &mut out, 1.0);
        }
        assert!(out.iter().all(|&v| v == 0.0));
    }
}
