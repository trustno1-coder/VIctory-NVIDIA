// fsr3_bridge.h — the C ABI between Rust (`crates/victory-gpu/src/fsr3.rs`)
// and this bridge DLL.
//
// The bridge is the only place that knows AMD's FidelityFX API structs.
// It is compiled on Windows against the FidelityFX SDK 1.1.4 headers
// (FSR 3.1), which `scripts/get-fsr3.ps1` fetches. Rust passes only
// opaque pointers and scalars across this boundary, so no AMD struct
// layout is ever hand-mirrored in Rust.
//
// Versioning: `fsr3_bridge_version()` must return FSR3_BRIDGE_ABI_VERSION.
// Rust refuses any other version — a stale bridge must never run against
// a newer Rust side, or vice versa.
//
// All functions return 0 on success, otherwise a FidelityFX return code
// (FFX_API_RETURN_OK is 0), or -1 for a bridge-side failure (missing AMD
// DLL, missing export, bad argument). No function throws; all failures
// are reported as a code so the Rust side can fall back to the plain
// present and the title's own upscaling.
//
// Threading: create / wrap / destroy are called once, on the presenting
// thread. prepare is called while recording the title's command list.
// configure is called once per present, on the presenting thread.
// frameID must increment by exactly 1 per frame (FSR 3.1's rule); the
// Rust side owns that counter (the upscale id).

#ifndef FSR3_BRIDGE_H
#define FSR3_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FSR3_BRIDGE_ABI_VERSION 1

// Flags for fsr3_fg_create (bit positions are this ABI's, not AMD's):
//   bit 0: HDR colour
//   bit 1: inverted depth [1..0]
//   bit 2: display-resolution motion vectors
//   bit 3: motion vectors carry the jitter
//   bit 4: async workloads (must be 0 in this milestone — off)
#define FSR3_FG_FLAG_HDR (1u << 0)
#define FSR3_FG_FLAG_DEPTH_INVERTED (1u << 1)
#define FSR3_FG_FLAG_DISPLAY_MV (1u << 2)
#define FSR3_FG_FLAG_MV_JITTER (1u << 3)
#define FSR3_FG_FLAG_ASYNC_WORKLOADS (1u << 4)

// The ABI version this bridge speaks.
__declspec(dllexport) int fsr3_bridge_version(void);

// Opaque frame-generation state. Created by fsr3_fg_create, destroyed
// by fsr3_fg_destroy. Do not free it yourself.
typedef struct Fsr3Fg Fsr3Fg;

// Create the FSR 3 frame-generation context for displaySize /
// renderSize, with backBufferFormat as a DXGI_FORMAT value.
// device is ID3D12Device*, gameQueue is ID3D12CommandQueue*.
// Returns 0 and *outFg on success.
__declspec(dllexport) int fsr3_fg_create(
    void *device, void *gameQueue,
    uint32_t displayWidth, uint32_t displayHeight,
    uint32_t renderWidth, uint32_t renderHeight,
    uint32_t backBufferFormat, uint32_t flags,
    void **outFg);

// Wrap an existing swap chain in FSR 3's frame-interpolation swap
// chain. *swapchainInOut is IDXGISwapChain4* on entry; on success
// *outWrapped is the wrapped IDXGISwapChain4* (a new reference, owned
// by the caller), and the original chain stays held by the wrap.
// Present / ResizeBuffers / GetBuffer must go through the wrapped
// chain from then on. Returns 0 on success.
__declspec(dllexport) int fsr3_fg_wrap_swapchain(
    void *fg, void **swapchainInOut, void **outWrapped);

// Remake the frame-generation context inside the same Fsr3Fg, at a
// new display / render size, keeping the wrapped swap chain (its
// swapchain context) alive: the host ResizeBuffers the wrapped chain
// on a resize, and the swapchain context's callbacks still point at
// this same Fsr3Fg, so a brand-new Fsr3Fg (whose callbacks the wrap
// never registered) would dispatch through a dead context. The old
// frame-generation context is destroyed first; the swapchain context
// is not touched. Returns 0 on success. Safe with NULL (returns -1).
__declspec(dllexport) int fsr3_fg_recreate(
    void *fg,
    uint32_t displayWidth, uint32_t displayHeight,
    uint32_t renderWidth, uint32_t renderHeight,
    uint32_t backBufferFormat, uint32_t flags);

// Record the frame-generation prepare pass into commandList
// (ID3D12GraphicsCommandList*), from depth and motionVectors
// (ID3D12Resource*, in the given D3D12_RESOURCE_STATES). frameID must
// match the configure call for the same frame. Returns 0 on success.
__declspec(dllexport) int fsr3_fg_prepare(
    void *fg, void *commandList,
    void *depth, uint32_t depthState,
    void *motionVectors, uint32_t motionVectorState,
    float jitterX, float jitterY,
    float motionVectorScaleX, float motionVectorScaleY,
    float frameTimeMs, int reset,
    float cameraNear, float cameraFar, float cameraFovAngleVertical,
    uint64_t frameID);

// Configure frame generation for the next present: swapChain is
// IDXGISwapChain*, hudlessColor is ID3D12Resource* (may be NULL) in
// hudlessState, frameID is the same id as the prepare's, enabled is
// 1 to generate, 0 for passthrough. Must be called once per frame.
// Returns 0 on success.
__declspec(dllexport) int fsr3_fg_configure(
    void *fg, void *swapChain,
    void *hudlessColor, uint32_t hudlessState,
    uint64_t frameID, int enabled);

// Destroy a context made by fsr3_fg_create. Safe with NULL.
__declspec(dllexport) void fsr3_fg_destroy(void *fg);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // FSR3_BRIDGE_H
