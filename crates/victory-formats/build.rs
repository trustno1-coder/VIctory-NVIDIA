//! Builds the C part of BCPack's decoder (`csrc/bcpack_lanes.c`).
fn main() {
    println!("cargo:rerun-if-changed=csrc/bcpack_lanes.c");
    cc::Build::new().file("csrc/bcpack_lanes.c").opt_level(3).warnings(true).compile("victory_bcpack_lanes");
}
