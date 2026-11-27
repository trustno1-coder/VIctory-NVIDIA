//! Everything specific to the title: its container formats, build
//! identities, and the hooks we place in its code (located by per-build
//! offsets or byte-pattern signatures). What may and may not be stored here
//! is defined in docs/asset-policy.md.

pub mod sgd5;

/// A specific build of the title executable we know how to run.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    /// Our own label, e.g. "1.0.0 retail".
    pub label: &'static str,
    /// BLAKE3 of the original executable, hex.
    pub blake3: &'static str,
    /// Changes to its code made before it is mapped.
    pub patches: &'static [CodePatch],
}

/// A change to title code: at `rva`, the bytes `expect` (a short signature
/// checked before anything is written) become `write`, our own code.
#[derive(Debug, Clone, Copy)]
pub struct CodePatch {
    /// Our name for it, as logged.
    pub name: &'static str,
    pub rva: u32,
    pub expect: &'static [u8],
    pub write: &'static [u8],
}

/// `mov eax, 6; ret`: the upscaler a Series X uses.
const UPSCALER_6: &[u8] = &[0xb8, 0x06, 0x00, 0x00, 0x00, 0xc3];

/// Builds we support.
pub const KNOWN_BUILDS: &[KnownBuild] = &[KnownBuild {
    label: "Xbox Series master",
    blake3: "b99f876b74d4a77f03af5f878223b27bd647b5c35a79d5decd9f716504239e93",
    // The title picks its upscaler by console: on a Series S with the
    // FidelityFX upscaler's capability, FSR 4 (type 7), else its own
    // (type 6, as on a Series X). Both functions that make the choice
    // return 6, so a Series S runs its own render profile with its own
    // upscaler. (From fxceless/victory-fork#798, where FSR 4's frames were
    // black with `--console series-s`.) On this tree FSR 4 runs, through
    // the translator, and costs: in the prologue's cell (RTX 3090 Ti,
    // 4,300 to 4,700 draws a frame) a median of 28.5 frames a second with
    // it against 30.9 without, 88 against 38 MB a frame of title memory
    // copied for the card and 21.6 against 0.8 MB a frame copied back.
    // Not applied when the title's FidelityFX path is asked for (DLSS,
    // frame generation, `VICTORY_FSR_PATH`: `victory run` says so).
    patches: &[
        CodePatch {
            name: "upscaler choice",
            rva: 0x7e_93f0,
            expect: &[0x48, 0x83, 0xec, 0x28, 0x80, 0x3d],
            write: UPSCALER_6,
        },
        CodePatch {
            name: "upscaler choice (the per-frame copy)",
            rva: 0x540_0190,
            expect: &[0x48, 0x83, 0xec, 0x28, 0x48, 0x8b],
            write: UPSCALER_6,
        },
    ],
}];

/// Looks up a build by the BLAKE3 hash of its executable.
pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    KNOWN_BUILDS
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_builds_are_well_formed() {
        for build in KNOWN_BUILDS {
            assert_eq!(build.blake3.len(), 64);
            assert!(build.blake3.bytes().all(|b| b.is_ascii_hexdigit()));
            assert_eq!(
                identify(&build.blake3.to_uppercase()).map(|b| b.label),
                Some(build.label)
            );
            for patch in build.patches {
                assert!(!patch.write.is_empty(), "{}", patch.name);
                assert_eq!(patch.expect.len(), patch.write.len(), "{}", patch.name);
            }
        }
        assert!(identify("00").is_none());
    }
}
