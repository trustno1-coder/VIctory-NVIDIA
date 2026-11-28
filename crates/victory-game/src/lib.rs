//! Everything specific to the title: its container formats, build
//! identities, the hooks we place in its code (located by per-build offsets
//! or byte-pattern signatures), and the pictures we build from the user's
//! own copy to serve in place of the title's (PlayStation button prompts).
//! What may and may not be stored here is defined in docs/asset-policy.md.

pub mod prompt_glyphs;
pub mod settings_diagram;
pub mod sgd5;

/// A specific build of the title executable we know how to run.
#[derive(Debug, Clone, Copy)]
pub struct KnownBuild {
    /// Our own label, e.g. "1.0.0 retail".
    pub label: &'static str,
    /// BLAKE3 of the original executable, hex.
    pub blake3: &'static str,
}

/// Builds we support. Empty until the executable is available.
pub const KNOWN_BUILDS: &[KnownBuild] = &[];

/// Looks up a build by the BLAKE3 hash of its executable.
pub fn identify(blake3_hex: &str) -> Option<&'static KnownBuild> {
    KNOWN_BUILDS
        .iter()
        .find(|b| b.blake3.eq_ignore_ascii_case(blake3_hex))
}
