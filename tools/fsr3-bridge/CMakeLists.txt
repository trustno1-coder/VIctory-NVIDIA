# Build fsr3_bridge.dll against the FidelityFX SDK (FSR 3.1).
#
#   cmake -S tools/fsr3-bridge -B build/fsr3-bridge ^
#     -DFSR3_SDK_DIR=<path to FidelityFX-SDK checkout> ^
#     -DCMAKE_BUILD_TYPE=Release
#   cmake --build build/fsr3-bridge --config Release
#
# FSR3_SDK_DIR must contain ffx-api/include (the SDK headers) — see
# scripts/get-fsr3.ps1, which fetches SDK 1.1.4 and runs this build.
# The DLL is loaded at run time; nothing here is linked into victory.exe.
# AMD's signed DLL (amd_fidelityfx_dx12.dll) is loaded by the bridge
# itself, from the same folder as fsr3_bridge.dll, at run time.

cmake_minimum_required(VERSION 3.20)
project(fsr3_bridge CXX)

if(NOT WIN32)
  message(FATAL_ERROR "fsr3_bridge.dll is Windows-only (DirectX 12)")
endif()

if(NOT DEFINED FSR3_SDK_DIR)
  message(FATAL_ERROR "FSR3_SDK_DIR is required: the FidelityFX SDK checkout (ffx-api/include)")
endif()

add_library(fsr3_bridge SHARED
  fsr3_bridge.cpp
  fsr3_bridge.h
)

target_include_directories(fsr3_bridge PRIVATE
  "${FSR3_SDK_DIR}/ffx-api/include"
)

target_compile_features(fsr3_bridge PRIVATE cxx_std_17)
target_compile_definitions(fsr3_bridge PRIVATE WIN32_LEAN_AND_MEAN NOMINMAX)

# The Windows SDK's d3d12 / dxgi headers, plus the bridge's own exports.
target_link_libraries(fsr3_bridge PRIVATE d3d12 dxgi)

# The DLL must land next to AMD's DLL, in fsr3-sdk\, not in a build dir.
# get-fsr3.ps1 copies it there; a manual build can set the output dir:
#   -DCMAKE_RUNTIME_OUTPUT_DIRECTORY_RELEASE=<repo>/fsr3-sdk
