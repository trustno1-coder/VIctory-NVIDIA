//! Camera inputs from the title's fullscreen velocity pass. Take the
//! camera sample at draw recording, then associate it with an upscale in
//! command-list submission order.

// Ported from Marvisak/VIctory PR #2751 (GPL-3.0-or-later).
use std::sync::Mutex;

use crate::host::HostList;

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Pose {
    pub(crate) position: [f32; 3],
    pub(crate) right: [f32; 3],
    pub(crate) up: [f32; 3],
    pub(crate) forward: [f32; 3],
}

impl Pose {
    /// The analytical provider does not use the pose.
    pub(crate) const LEGACY: Self = Self {
        position: [0.0; 3],
        right: [1.0, 0.0, 0.0],
        up: [0.0, 1.0, 0.0],
        forward: [0.0, 0.0, 1.0],
    };
}

#[derive(Clone, Copy, PartialEq)]
struct Sample {
    pose: Pose,
    viewport: [f32; 2],
    projection_offset: [f32; 2],
}

const BUFFER_SIZE: u32 = 480;
const MAX_CANDIDATES: usize = 16;
static CANDIDATES: Mutex<Vec<Sample>> = Mutex::new(Vec::new());

pub(crate) const WARNING_AFTER_UPSCALES: u8 = 60;

#[derive(Default)]
pub(crate) struct MissingCameraWarning {
    misses: u8,
    warned: bool,
}

impl MissingCameraWarning {
    /// Call only when an upscale supplies otherwise usable frame inputs.
    pub(crate) fn observe(&mut self, matched: bool) -> bool {
        if self.warned {
            return false;
        }
        if matched {
            self.misses = 0;
            return false;
        }
        self.misses += 1;
        self.warned = self.misses == WARNING_AFTER_UPSCALES;
        self.warned
    }
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a.into_iter().zip(b).map(|(x, y)| x * y).sum()
}

fn vec3(words: &[f32; 52], at: usize) -> [f32; 3] {
    [words[at], words[at + 1], words[at + 2]]
}

impl Sample {
    fn decode(words: &[f32; 52], viewport: [f32; 2]) -> Option<Self> {
        if !words.iter().all(|x| x.is_finite())
            || !viewport.iter().all(|&x| x.is_finite() && x > 0.0)
            || words[16] <= 0.0
            || words[21] <= 0.0
            || words[27] != -1.0
            || words[31] != 0.0
            || words[51] != 1.0
        {
            return None;
        }
        // The buffer contains view rotation and its inverse. World-space
        // camera axes are the rows of the inverse, with backward negated.
        let axes = [vec3(words, 32), vec3(words, 36), vec3(words, 40)];
        for (i, axis) in axes.iter().enumerate() {
            for (j, other) in axes.iter().enumerate() {
                let expected = if i == j { 1.0 } else { 0.0 };
                if (dot(*axis, *other) - expected).abs() > 0.001
                    || (axis[j] - words[j * 4 + i]).abs() > 0.001
                {
                    return None;
                }
            }
        }
        Some(Self {
            pose: Pose {
                position: vec3(words, 48),
                right: axes[0],
                up: axes[1],
                forward: axes[2].map(|value| -value),
            },
            viewport,
            projection_offset: [words[24], words[25]],
        })
    }

    fn matches(self, render: [u32; 2], jitter: [f32; 2]) -> bool {
        // The velocity shader's projection is in clip space. FidelityFX
        // takes offsets in pixels. The two paths can use different signs.
        (0..2).all(|axis| {
            let extent = render[axis] as f32;
            let projected = self.projection_offset[axis].abs() * extent * 0.5;
            (self.viewport[axis] - extent).abs() < 0.5
                && (projected - jitter[axis].abs()).abs() < 0.015
        })
    }
}

/// Samples recorded before and after the last upscale in a single list.
#[derive(Default)]
pub(crate) struct Recording {
    before: Vec<Sample>,
    after: Vec<Sample>,
    upscales: u32,
}

fn add_sample(samples: &mut Vec<Sample>, sample: Sample) {
    if samples.len() <= MAX_CANDIDATES && !samples.contains(&sample) {
        samples.push(sample);
    }
}

impl Recording {
    pub(crate) fn mark_upscale(&mut self) {
        self.before = std::mem::take(&mut self.after);
        self.upscales = self.upscales.saturating_add(1);
    }

    pub(crate) fn submitted(self, expected: Option<([u32; 2], [f32; 2])>) -> Option<Pose> {
        let mut candidates = CANDIDATES.lock().unwrap_or_else(|e| e.into_inner());
        if self.upscales > 1 {
            candidates.clear();
        }
        for sample in self.before {
            add_sample(&mut candidates, sample);
        }
        let pose = expected.and_then(|(render, jitter)| unique_match(&candidates, render, jitter));
        if self.upscales != 0 {
            candidates.clear();
        }
        for sample in self.after {
            add_sample(&mut candidates, sample);
        }
        pose
    }
}

pub(crate) fn record(list: &HostList, address: u64, size: u32, viewport: [f32; 2]) {
    if size != BUFFER_SIZE {
        return;
    }
    let mut bytes = [0; 52 * 4];
    if !crate::host::read_into(address as *const u8, &mut bytes) {
        return;
    }
    let chunks = bytes.as_chunks::<4>().0;
    let words = std::array::from_fn(|i| f32::from_le_bytes(chunks[i]));
    let Some(sample) = Sample::decode(&words, viewport) else {
        return;
    };
    static FIRST_SAMPLE: std::sync::Once = std::sync::Once::new();
    FIRST_SAMPLE.call_once(|| tracing::info!(?viewport, "FSR 4: decoded first velocity-pass camera"));
    list.record_framegen_camera(|recording| add_sample(&mut recording.after, sample));
}

pub(crate) fn clear() {
    CANDIDATES.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

fn unique_match(candidates: &[Sample], render: [u32; 2], jitter: [f32; 2]) -> Option<Pose> {
    if candidates.len() > MAX_CANDIDATES {
        return None;
    }
    let mut fits = candidates
        .iter()
        .filter(|candidate| candidate.matches(render, jitter));
    let pose = fits.next()?.pose;
    fits.all(|candidate| candidate.pose == pose).then_some(pose)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words() -> [f32; 52] {
        let mut words = [0.0; 52];
        words[..16].copy_from_slice(&[
            0.0, -1.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]);
        words[16] = 1.2;
        words[21] = 2.0;
        words[24] = -0.5 / 1920.0;
        words[25] = 0.75 / 1080.0;
        words[27] = -1.0;
        words[32..48].copy_from_slice(&[
            0.0, 1.0, 0.0, 0.0, -1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]);
        words[48..].copy_from_slice(&[20.0, 40.0, -10.0, 1.0]);
        words
    }

    #[test]
    fn decodes_world_axes_and_rejects_non_inverse_matrices() {
        let mut words = words();
        let sample = Sample::decode(&words, [1920.0, 1080.0]).unwrap();
        assert_eq!(sample.pose.position, [20.0, 40.0, -10.0]);
        assert_eq!(sample.pose.right, [0.0, 1.0, 0.0]);
        assert_eq!(sample.pose.up, [-1.0, 0.0, 0.0]);
        assert_eq!(sample.pose.forward, [0.0, 0.0, -1.0]);
        words[4] = -1.0;
        assert!(Sample::decode(&words, [1920.0, 1080.0]).is_none());
    }

    #[test]
    fn partners_a_pose_only_with_a_matching_unambiguous_upscale() {
        let size = [1920, 1080];
        let jitter = [0.25, -0.375];
        let sample = Sample::decode(&words(), [1920.0, 1080.0]).unwrap();
        assert_eq!(unique_match(&[sample], size, jitter), Some(sample.pose));
        assert_eq!(unique_match(&[sample], [2560, 1440], jitter), None);
        assert_eq!(unique_match(&[sample], size, [0.0; 2]), None);
        let mut other = sample;
        other.pose.position[0] += 5.0;
        assert_eq!(unique_match(&[sample, other], size, jitter), None);
    }

    #[test]
    fn a_camera_after_the_last_upscale_belongs_to_the_next_list() {
        clear();
        let dimensions = [1920, 1080];
        let jitter = [0.25, -0.375];
        let sample = Sample::decode(&words(), [1920.0, 1080.0]).unwrap();
        let mut first = Recording::default();
        first.mark_upscale();
        add_sample(&mut first.after, sample);
        assert_eq!(first.submitted(Some((dimensions, jitter))), None);
        let mut second = Recording::default();
        second.mark_upscale();
        assert_eq!(second.submitted(Some((dimensions, jitter))), Some(sample.pose));
        let mut third = Recording::default();
        third.mark_upscale();
        assert_eq!(third.submitted(Some((dimensions, jitter))), None);
    }

    #[test]
    fn warns_once_only_after_a_sustained_stretch_without_a_camera() {
        let mut warning = MissingCameraWarning::default();
        for _ in 1..WARNING_AFTER_UPSCALES {
            assert!(!warning.observe(false));
        }
        assert!(!warning.observe(true));
        for _ in 1..WARNING_AFTER_UPSCALES {
            assert!(!warning.observe(false));
        }
        assert!(warning.observe(false));
        assert!(!warning.observe(true));
        for _ in 0..WARNING_AFTER_UPSCALES {
            assert!(!warning.observe(false));
        }
    }
}
