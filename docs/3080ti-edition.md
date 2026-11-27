# 3080 Ti Edition — hardware profile and validation plan

Target hardware:
- GPU: NVIDIA GeForce RTX 3080 Ti (12 GB VRAM)
- CPU: AMD Ryzen 7 7800X3D (8 cores / 16 threads)
- System memory: 16 GB RAM

## Status

This branch is a hardware-targeted work area, not a claim of verified compatibility or measured performance. No game files, commercial executables, proprietary DLLs, shaders, saves, keys, or dumps belong in this repository.

## Important memory constraint

The upstream project documents 32 GB RAM or more. This machine currently has 16 GB. A Windows page file is not equivalent to physical RAM and may lead to heavy paging and stutter. Do not describe the build as stable on 16 GB until a reproducible test demonstrates it.

## GPU and frame-generation notes

- RTX 3080 Ti supports DLSS Super Resolution, but does not support NVIDIA DLSS Frame Generation.
- The current source has an AMD FSR 3 frame-generation path.
- In the Series X profile, the source explicitly requires DLSS upscaling for FSR 3 frame generation; without DLSS, FSR 3 is disabled on Series X. The Series S profile has a separate path.
- Therefore the proposed Series X test configuration is DLSS upscaling plus FSR 3 frame generation at 2x, only after the required official NVIDIA Streamline and AMD FidelityFX runtime files are obtained from their respective vendors and verified.
- Do not claim FSR 3 works on this exact GPU/game combination until tested.

## Initial test profile (proposal, not yet applied to runtime defaults)

- Console profile: Series X
- Output resolution: 2560x1440 target for later testing; begin with 1920x1080 if memory pressure or instability appears.
- Upscaling: DLSS Quality as the first comparison point.
- Frame generation: off for baseline; then FSR 3 2x as a separate test.
- Avoid extreme CPU priority, uncapped FPS, or aggressive texture settings during initial validation.
- Record logs, driver version, game build identifier, commit SHA, resolution, scene, warm/cold shader cache, and RAM/VRAM usage for every run.

## Validation sequence

1. Build the untouched upstream code on the target Windows environment.
2. Run static checks and the repository test suite.
3. Establish a no-frame-generation baseline.
4. Test DLSS upscaling separately.
5. Test FSR 3 2x only after confirming the correct vendor runtime files and Series X prerequisites.
6. Compare frame pacing, crashes, RAM use, VRAM use, and repeatability; report failures honestly.
7. Change one setting at a time. Do not publish FPS claims without repeatable measurements.

## Safety and repository boundaries

- Keep the upstream repository untouched.
- Make all changes on this fork's `3080Ti-Edition` branch.
- Never commit game files or proprietary runtime binaries.
- Do not execute unsigned third-party binaries merely because they are included in a release.
