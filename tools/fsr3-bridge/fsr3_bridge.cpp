// fsr3_bridge.cpp - FSR 3 frame generation for VIctory, via the
// FidelityFX API (FSR 3.1, FidelityFX SDK 1.1.4).
//
// This is the ONLY file in the repository that knows AMD's FidelityFX
// structs. It is compiled on Windows against the real SDK headers
// (`ffx-api/include`), so a struct that changed in a newer SDK fails
// the build here, loudly, instead of corrupting memory at run time
// from a hand-mirrored layout in Rust.
//
// What it does, per GPUOpen ("AMD FSR Frame Generation API",
// "FSR 3.1 Integration Overview", FSR 3.1.4):
//   1. Loads AMD's signed DLL at run time (amd_fidelityfx_dx12.dll,
//      SDK 2.0 fallback amd_fidelityfx_loader_dx12.dll) and resolves
//      the five ffx* functions with GetProcAddress.
//   2. fsr3_fg_create: creates a frame-generation context
//      (ffxCreateContextDescFrameGeneration + backend desc). SDK 1.1.4
//      has no version struct to chain (that arrived in SDK 2.1), so
//      the backend desc is chained directly.
//   3. fsr3_fg_wrap_swapchain: wraps the game's swap chain in FSR 3's
//      frame-interpolation swap chain
//      (ffxCreateContextDescFrameGenerationSwapChainWrapDX12, type
//      0x30001). Presents from then on go through the wrapped chain.
//      The wrap holds an extra reference on the chain across the call:
//      the first FSR run's wrap failed with RUNTIME_ERROR and the host
//      then crashed in dxgi.dll presenting through the chain, which
//      points at the DLL consuming a reference to it in its error
//      cleanup (the hardening worked: the second run survived the
//      failed wrap and ran 3,763 frames). The wrap also writes
//      fsr3_wrap_diag.txt beside this DLL (chain desc, whether its
//      device is the FG context's device, the DLL's return code, and
//      probes of the D3D12 calls the wrap needs: the game queue's
//      timestamp frequency, a test present queue, a fence, a command
//      allocator/list, a test interpolation texture, GetBuffer and
//      GetParent, the swap chain's IDXGISwapChain3/4 support, a
//      descriptor heap, copy/compute queues and a shared fence) so a
//      failed wrap can be traced to the call that actually fails,
//      without another build.
//   4. fsr3_fg_prepare: dispatches the frame-generation prepare pass
//      (ffxDispatchDescFrameGenerationPrepare) into the caller's
//      command list, from depth + motion vectors, with the camera
//      info linked after it
//      (ffxDispatchDescFrameGenerationPrepareCameraInfo - a required
//      input in FSR 3.1.4). There is no PrepareV2 in SDK 1.1.4 (it
//      replaced these two structs in SDK 2.1 / FSR 3.1.6).
//   5. fsr3_fg_configure: configures frame generation once per frame
//      (ffxConfigureDescFrameGeneration), registering the frame-
//      generation callback (which dispatches the interpolation) and
//      the present callback (which copies the back buffer, with the
//      UI already in it, into the output buffer).
//   6. fsr3_fg_recreate: on a resize, remakes only the frame-
//      generation context at the new size, in place - the wrapped
//      swap chain (its swapchain context) stays alive, because the
//      host ResizeBuffers it and keeps presenting through it.
//
// Known approximations, stated so they can be fixed with real data:
//   - Camera vectors: the title gives no camera matrices (same as
//     DLSS in this layer, which builds its constants from FOV/near/
//     far alone). Prepare is given position (0,0,0), up (0,1,0),
//     right (1,0,0), forward (0,0,-1). If FSR 3's optical flow needs
//     the real camera, this is where it goes in.
//   - UI: the host draws its F1 menu / stats panel onto the back
//     buffer before Present, so the present callback copies that
//     buffer as-is. HUDLessColor (the scene texture, no host UI) is
//     passed so FSR 3 can tell UI from scene. The F1 menu should be
//     closed while frame generation is on.
//   - SDR only: VIctory presents SDR (2560x1440 SDR). The
//     back-buffer transfer function is SRGB and the luminance range
//     is [0, 1].
//   - Async workloads are off (flag bit 4 must be 0): the optical-
//     flow and interpolation work runs on the graphics queue, in
//     the order the layer submits it.
//   - Reset: FSR 3.1.4's prepare has only `unused_reset`, which the
//     implementation ignores - there is no working reset flag in
//     this SDK. A scene cut resets FSR 3 only through a disjoint
//     frameID (a step other than +1), which the Rust side owns:
//     it must skip an id when the title cuts. Until then, a hard
//     cut may ghost for a frame or two, as in every 3.1.4
//     integration.

#include "fsr3_bridge.h"

#include <windows.h>
#include <d3d12.h>
#include <dxgi1_4.h>

#include <ffx_api/ffx_api.h>
#include <ffx_api/dx12/ffx_api_dx12.h>

// The frame-generation structs live in their own header, which
// ffx_api.h does not include. Its name differs between SDK
// generations (ffx_framegeneration.h in 1.1.x, ffx_fg.h in the
// migration guide's naming); take whichever this SDK has.
#if __has_include(<ffx_api/ffx_framegeneration.h>)
#  include <ffx_api/ffx_framegeneration.h>
#elif __has_include(<ffx_api/ffx_fg.h>)
#  include <ffx_api/ffx_fg.h>
#else
#  error "no frame-generation header in this SDK (looked for ffx_api/ffx_framegeneration.h and ffx_api/ffx_fg.h)"
#endif

#include <cstdio>
#include <cstring>
#include <cwchar>
#include <new>
#include <type_traits>

// The bridge DLL's own base, for finding AMD's DLL beside it.
EXTERN_C IMAGE_DOS_HEADER __ImageBase;

namespace {

// The five FidelityFX API functions, resolved from AMD's DLL.
// Signatures are the documented ones (ffx_api.h):
//   ffxCreateContext(ffxContext*, ffxCreateContextDescHeader*,
//                    const ffxAllocationCallbacks*)
//   ffxDestroyContext(ffxContext*, const ffxAllocationCallbacks*)
//   ffxDispatch(ffxContext*, const ffxDispatchDescHeader*)
//   ffxQuery(ffxContext*, ffxQueryDescHeader*)
//   ffxConfigure(ffxContext*, ffxConfigureDescHeader*)
using FfxCreateContextFn = ffxReturnCode_t (*)(ffxContext *, ffxCreateContextDescHeader *,
                                                const ffxAllocationCallbacks *);
using FfxDestroyContextFn = ffxReturnCode_t (*)(ffxContext *, const ffxAllocationCallbacks *);
using FfxDispatchFn = ffxReturnCode_t (*)(ffxContext *, const ffxDispatchDescHeader *);
using FfxQueryFn = ffxReturnCode_t (*)(ffxContext *, ffxQueryDescHeader *);
using FfxConfigureFn = ffxReturnCode_t (*)(ffxContext *, ffxConfigureDescHeader *);

struct FfxApi {
    HMODULE module = nullptr;
    FfxCreateContextFn createContext = nullptr;
    FfxDestroyContextFn destroyContext = nullptr;
    FfxDispatchFn dispatch = nullptr;
    FfxQueryFn query = nullptr;
    FfxConfigureFn configure = nullptr;
};

// Named Fsr3FgState (not Fsr3Fg) so it cannot be confused with the
// opaque `typedef struct Fsr3Fg Fsr3Fg;` in fsr3_bridge.h.
struct Fsr3FgState {
    FfxApi api;
    ffxContext fgContext = nullptr;
    ffxContext swapContext = nullptr;
    ID3D12Device *device = nullptr;
    ID3D12CommandQueue *gameQueue = nullptr;
    uint32_t displayWidth = 0, displayHeight = 0;
    uint32_t renderWidth = 0, renderHeight = 0;
    uint32_t backBufferFormat = 0; // DXGI_FORMAT
    uint32_t flags = 0;
    uint64_t lastFrameID = 0;
    bool enabled = false;
};

// Load AMD's DLL from the same folder as this bridge DLL.
bool loadAmd(FfxApi &api) {
    wchar_t path[MAX_PATH];
    if (GetModuleFileNameW(reinterpret_cast<HMODULE>(&__ImageBase), path, MAX_PATH) == 0) {
        return false;
    }
    // __ImageBase is the bridge DLL's own base; its folder is AMD's.
    wchar_t *slash = wcsrchr(path, L'\\');
    if (slash) *slash = L'\0';
    const wchar_t *names[] = {L"amd_fidelityfx_dx12.dll", L"amd_fidelityfx_loader_dx12.dll"};
    for (const wchar_t *name : names) {
        wchar_t full[MAX_PATH];
        _snwprintf_s(full, MAX_PATH, _TRUNCATE, L"%s\\%s", path, name);
        HMODULE module = LoadLibraryW(full);
        if (!module) continue;
        auto get = [&](const char *n) {
            return reinterpret_cast<void *>(GetProcAddress(module, n));
        };
        api.createContext = reinterpret_cast<FfxCreateContextFn>(get("ffxCreateContext"));
        api.destroyContext = reinterpret_cast<FfxDestroyContextFn>(get("ffxDestroyContext"));
        api.dispatch = reinterpret_cast<FfxDispatchFn>(get("ffxDispatch"));
        api.query = reinterpret_cast<FfxQueryFn>(get("ffxQuery"));
        api.configure = reinterpret_cast<FfxConfigureFn>(get("ffxConfigure"));
        if (api.createContext && api.destroyContext && api.dispatch && api.query && api.configure) {
            api.module = module;
            return true;
        }
        FreeLibrary(module);
        api = FfxApi{};
    }
    return false;
}

FfxApiSurfaceFormat surfaceFormat(uint32_t dxgiFormat) {
    switch (dxgiFormat) {
        case DXGI_FORMAT_R8G8B8A8_UNORM: return FFX_API_SURFACE_FORMAT_R8G8B8A8_UNORM;
        case DXGI_FORMAT_B8G8R8A8_UNORM: return FFX_API_SURFACE_FORMAT_B8G8R8A8_UNORM;
        case DXGI_FORMAT_R10G10B10A2_UNORM: return FFX_API_SURFACE_FORMAT_R10G10B10A2_UNORM;
        case DXGI_FORMAT_R16G16B16A16_FLOAT: return FFX_API_SURFACE_FORMAT_R16G16B16A16_FLOAT;
        default: return FFX_API_SURFACE_FORMAT_UNKNOWN;
    }
}

uint32_t fgFlags(uint32_t flags) {
    uint32_t out = 0;
    if (flags & FSR3_FG_FLAG_HDR) out |= FFX_FRAMEGENERATION_ENABLE_HIGH_DYNAMIC_RANGE;
    if (flags & FSR3_FG_FLAG_DEPTH_INVERTED) out |= FFX_FRAMEGENERATION_ENABLE_DEPTH_INVERTED;
    if (flags & FSR3_FG_FLAG_DISPLAY_MV) out |= FFX_FRAMEGENERATION_ENABLE_DISPLAY_RESOLUTION_MOTION_VECTORS;
    if (flags & FSR3_FG_FLAG_MV_JITTER) out |= FFX_FRAMEGENERATION_ENABLE_MOTION_VECTORS_JITTER_CANCELLATION;
    if (flags & FSR3_FG_FLAG_ASYNC_WORKLOADS) out |= FFX_FRAMEGENERATION_ENABLE_ASYNC_WORKLOAD_SUPPORT;
    return out;
}

// The D3D12 state a title texture is in, as AMD's state: the backend
// barriers from the declared state to what it needs, so the declared
// state must be the real one - a raw D3D12 value cast to the AMD
// enum would be meaningless bits. States with no FFX equivalent
// (render target, depth write) fall back to generic read.
FfxApiResourceState apiState(uint32_t d3d12State) {
    if (d3d12State & D3D12_RESOURCE_STATE_UNORDERED_ACCESS) return FFX_API_RESOURCE_STATE_UNORDERED_ACCESS;
    if (d3d12State & D3D12_RESOURCE_STATE_COPY_SOURCE) return FFX_API_RESOURCE_STATE_COPY_SRC;
    if (d3d12State & D3D12_RESOURCE_STATE_COPY_DEST) return FFX_API_RESOURCE_STATE_COPY_DEST;
    if (d3d12State & D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE) return FFX_API_RESOURCE_STATE_PIXEL_COMPUTE_READ;
    if (d3d12State & D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE) return FFX_API_RESOURCE_STATE_COMPUTE_READ;
    return FFX_API_RESOURCE_STATE_GENERIC_READ;
}

// An FfxApiResource for a title texture, in the D3D12 state it is in.
// The DX12 backend helper fills the description from the D3D12
// resource itself, so no layout is hand-mirrored here either.
FfxApiResource apiResource(ID3D12Resource *resource, uint32_t d3d12State) {
    return ffxApiGetResourceDX12(resource, apiState(d3d12State));
}

// Whether a linked struct carries a header. SDK 1.1.4's
// ffxDispatchDescFrameGenerationPrepareCameraInfo does (its type id
// goes in that header); later SDKs stripped it when the struct was
// deprecated in favour of PrepareV2.
template <typename T, typename = void>
struct HasHeader : std::false_type {};
template <typename T>
struct HasHeader<T, std::void_t<decltype(std::declval<T>().header)>> : std::true_type {};

// Chain `linked` after a dispatch desc, setting its header type when
// it has one. The header is the struct's first member, so the
// struct's address is the header's address either way.
template <typename T, typename U>
ffxDispatchDescHeader *linkHeader(T &linked, U type) {
    if constexpr (HasHeader<T>::value) {
        linked.header.type = static_cast<decltype(linked.header.type)>(type);
    }
    return reinterpret_cast<ffxDispatchDescHeader*>(&linked);
}

// Called by the FSR swap chain during Present, to generate a frame.
// Dispatches the interpolation with the parameters FSR 3 supplies.
ffxReturnCode_t frameGenerationCallback(ffxDispatchDescFrameGeneration *params, void *userCtx) {
    auto *fg = static_cast<Fsr3FgState *>(userCtx);
    if (!fg || !params) return FFX_API_RETURN_ERROR;
    return fg->api.dispatch(&fg->fgContext, &params->header);
}

// Called by the FSR swap chain for every frame presented (real or
// generated). Copies the back buffer - UI already composited into it
// by the host - into the output buffer.
ffxReturnCode_t presentCallback(ffxCallbackDescFrameGenerationPresent *params, void * /*userCtx*/) {
    if (!params || !params->commandList) return FFX_API_RETURN_ERROR;
    auto *list = static_cast<ID3D12GraphicsCommandList *>(params->commandList);
    auto *back = static_cast<ID3D12Resource *>(params->currentBackBuffer.resource);
    auto *out = static_cast<ID3D12Resource *>(params->outputSwapChainBuffer.resource);
    if (!back || !out) return FFX_API_RETURN_ERROR;
    list->CopyResource(out, back);
    return FFX_API_RETURN_OK;
}

// Make the frame-generation context from fg's current fields.
// The swapchain context is untouched: it keeps pointing at this
// same Fsr3FgState for its callbacks.
int createFgContext(Fsr3FgState *fg) {
    ffxCreateBackendDX12Desc backendDesc{};
    backendDesc.header.type = FFX_API_CREATE_CONTEXT_DESC_TYPE_BACKEND_DX12;
    backendDesc.device = fg->device;

    ffxCreateContextDescFrameGeneration createFg{};
    createFg.header.type = FFX_API_CREATE_CONTEXT_DESC_TYPE_FRAMEGENERATION;
    createFg.displaySize = {fg->displayWidth, fg->displayHeight};
    createFg.maxRenderSize = {fg->renderWidth, fg->renderHeight};
    createFg.flags = fgFlags(fg->flags);
    createFg.backBufferFormat = surfaceFormat(fg->backBufferFormat);
    if (createFg.backBufferFormat == FFX_API_SURFACE_FORMAT_UNKNOWN) {
        return -1;
    }

    // SDK 1.1.4 has no version struct to chain (that arrived in
    // SDK 2.1); the backend desc is chained directly.
    createFg.header.pNext = &backendDesc.header;

    ffxReturnCode_t code = fg->api.createContext(&fg->fgContext, &createFg.header, nullptr);
    if (code != FFX_API_RETURN_OK || !fg->fgContext) {
        return static_cast<int>(code);
    }
    return 0;
}

// The folder this bridge DLL was loaded from (AMD's DLL lives there too).
bool bridgeFolder(wchar_t *out, size_t count) {
    if (GetModuleFileNameW(reinterpret_cast<HMODULE>(&__ImageBase), out, static_cast<DWORD>(count)) == 0) return false;
    wchar_t *slash = wcsrchr(out, L'\\');
    if (!slash) return false;
    *slash = L'\0';
    return true;
}

// Probe the D3D12 calls the wrap needs, so a RUNTIME_ERROR from
// AMD's DLL can be traced to the call that actually fails. Every
// probe creates and immediately releases its object; a failure here
// is the wrap's failure, not a new one. The first round of probes
// (queue timestamp, present queue, fence, allocator/list, texture,
// GetBuffer, GetParent) all passed, so the failure is in something
// they did not cover. The prime suspect now is the swap chain
// itself: the DLL needs the current back-buffer index, which lives
// on IDXGISwapChain3, and victory's swap chain may be a proxy that
// only implements the base interface.
struct WrapProbes {
    HRESULT queueTimestampHr = E_FAIL;
    UINT64 queueTimestampFreq = 0;
    HRESULT testQueueHr = E_FAIL;
    HRESULT testFenceHr = E_FAIL;
    HRESULT testAllocatorHr = E_FAIL;
    HRESULT testListHr = E_FAIL;
    HRESULT testTextureHr = E_FAIL;
    HRESULT heapHr = E_FAIL;
    HRESULT copyQueueHr = E_FAIL;
    HRESULT computeQueueHr = E_FAIL;
    HRESULT sharedFenceHr = E_FAIL;
    HRESULT getBufferHr = E_FAIL;
    HRESULT getParentHr = E_FAIL;
    HRESULT sc3Hr = E_FAIL;
    UINT backBufferIndex = 0xFFFFFFFF;
    HRESULT sc4Hr = E_FAIL;
    HRESULT fullscreenHr = E_FAIL;
    BOOL fullscreen = FALSE;
    HRESULT outputHr = E_FAIL;
};

WrapProbes probeWrap(ID3D12Device *device, ID3D12CommandQueue *gameQueue, IDXGISwapChain4 *swapchain) {
    WrapProbes p{};
    if (gameQueue) {
        p.queueTimestampHr = gameQueue->GetTimestampFrequency(&p.queueTimestampFreq);
    }
    if (device) {
        D3D12_COMMAND_QUEUE_DESC qd{};
        qd.Type = D3D12_COMMAND_LIST_TYPE_DIRECT;
        ID3D12CommandQueue *testQueue = nullptr;
        p.testQueueHr = device->CreateCommandQueue(&qd, IID_PPV_ARGS(&testQueue));
        if (testQueue) testQueue->Release();
        ID3D12Fence *fence = nullptr;
        p.testFenceHr = device->CreateFence(0, D3D12_FENCE_FLAG_NONE, IID_PPV_ARGS(&fence));
        if (fence) fence->Release();
        ID3D12CommandAllocator *alloc = nullptr;
        p.testAllocatorHr = device->CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT, IID_PPV_ARGS(&alloc));
        if (alloc) {
            ID3D12GraphicsCommandList *list = nullptr;
            p.testListHr = device->CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, alloc, nullptr, IID_PPV_ARGS(&list));
            if (list) {
                list->Close();
                list->Release();
            }
            alloc->Release();
        }
        D3D12_HEAP_PROPERTIES heap{};
        heap.Type = D3D12_HEAP_TYPE_DEFAULT;
        D3D12_RESOURCE_DESC tex{};
        tex.Dimension = D3D12_RESOURCE_DIMENSION_TEXTURE2D;
        tex.Width = 64;
        tex.Height = 64;
        tex.DepthOrArraySize = 1;
        tex.MipLevels = 1;
        tex.Format = DXGI_FORMAT_R16G16B16A16_FLOAT;
        tex.SampleDesc.Count = 1;
        tex.Flags = D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS;
        ID3D12Resource *resource = nullptr;
        p.testTextureHr = device->CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &tex,
                                                          D3D12_RESOURCE_STATE_COMMON, nullptr, IID_PPV_ARGS(&resource));
        if (resource) resource->Release();
        // A descriptor heap (the DLL needs one for its interpolation
        // resources) and queues of the other types, in case it makes
        // one of those instead of a direct queue.
        D3D12_DESCRIPTOR_HEAP_DESC hd{};
        hd.Type = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        hd.NumDescriptors = 16;
        ID3D12DescriptorHeap *descHeap = nullptr;
        p.heapHr = device->CreateDescriptorHeap(&hd, IID_PPV_ARGS(&descHeap));
        if (descHeap) descHeap->Release();
        D3D12_COMMAND_QUEUE_DESC cqd{};
        cqd.Type = D3D12_COMMAND_LIST_TYPE_COPY;
        ID3D12CommandQueue *copyQueue = nullptr;
        p.copyQueueHr = device->CreateCommandQueue(&cqd, IID_PPV_ARGS(&copyQueue));
        if (copyQueue) copyQueue->Release();
        D3D12_COMMAND_QUEUE_DESC kqd{};
        kqd.Type = D3D12_COMMAND_LIST_TYPE_COMPUTE;
        ID3D12CommandQueue *computeQueue = nullptr;
        p.computeQueueHr = device->CreateCommandQueue(&kqd, IID_PPV_ARGS(&computeQueue));
        if (computeQueue) computeQueue->Release();
        // A shared fence, in case the DLL shares one between its own
        // present queue and the game's.
        ID3D12Fence *sharedFence = nullptr;
        p.sharedFenceHr = device->CreateFence(0, D3D12_FENCE_FLAG_SHARED, IID_PPV_ARGS(&sharedFence));
        if (sharedFence) sharedFence->Release();
    }
    if (swapchain) {
        ID3D12Resource *back = nullptr;
        p.getBufferHr = swapchain->GetBuffer(0, IID_PPV_ARGS(&back));
        if (back) back->Release();
        IDXGIFactory *factory = nullptr;
        p.getParentHr = swapchain->GetParent(IID_PPV_ARGS(&factory));
        if (factory) factory->Release();
        // The current back-buffer index lives on IDXGISwapChain3.
        // If victory's swap chain is a proxy that only implements
        // the base interface, this is the call the wrap dies on.
        IDXGISwapChain3 *sc3 = nullptr;
        p.sc3Hr = swapchain->QueryInterface(IID_PPV_ARGS(&sc3));
        if (sc3) {
            p.backBufferIndex = sc3->GetCurrentBackBufferIndex();
            sc3->Release();
        }
        IDXGISwapChain4 *sc4 = nullptr;
        p.sc4Hr = swapchain->QueryInterface(IID_PPV_ARGS(&sc4));
        if (sc4) sc4->Release();
        BOOL fs = FALSE;
        p.fullscreenHr = swapchain->GetFullscreenState(&fs, nullptr);
        p.fullscreen = fs;
        IDXGIOutput *output = nullptr;
        p.outputHr = swapchain->GetContainingOutput(&output);
        if (output) output->Release();
    }
    return p;
}

// Write the wrap diagnostics to fsr3_wrap_diag.txt beside this DLL:
// the chain's desc, whether its device is the FG context's device,
// the probes above, and the DLL's return code. Diagnostics must
// never break the wrap, so every failure here is ignored.
void writeWrapDiag(const wchar_t *folder, const DXGI_SWAP_CHAIN_DESC1 *desc1, HRESULT hrDesc,
                   bool deviceMatches, HRESULT hrDev, void *gameQueue, int resultCode, bool wrapped,
                   const WrapProbes *probes) {
    wchar_t path[MAX_PATH];
    _snwprintf_s(path, MAX_PATH, _TRUNCATE, L"%s\\fsr3_wrap_diag.txt", folder);
    FILE *f = nullptr;
    if (_wfopen_s(&f, path, L"w") != 0 || !f) return;
    fwprintf(f, L"FSR 3 swap-chain wrap diagnostics\n");
    if (desc1 && hrDesc == S_OK) {
        fwprintf(f, L"chain desc: %ux%u format=%u buffers=%u usage=0x%x swapEffect=%d sample=%u/%u flags=0x%x stereo=%d scaling=%d alpha=%d\n",
                 (unsigned)desc1->Width, (unsigned)desc1->Height, (unsigned)desc1->Format,
                 (unsigned)desc1->BufferCount, (unsigned)desc1->BufferUsage,
                 (int)desc1->SwapEffect, (unsigned)desc1->SampleDesc.Count,
                 (unsigned)desc1->SampleDesc.Quality, (unsigned)desc1->Flags,
                 (int)desc1->Stereo, (int)desc1->Scaling, (int)desc1->AlphaMode);
    } else {
        fwprintf(f, L"chain desc: GetDesc1 failed hr=0x%08x\n", (unsigned)hrDesc);
    }
    fwprintf(f, L"chain device: GetDevice hr=0x%08x, matches the FG context device: %s\n",
             (unsigned)hrDev, deviceMatches ? L"yes" : L"NO");
    fwprintf(f, L"game queue: %p\n", gameQueue);
    if (probes) {
        fwprintf(f, L"probes: queue timestamp hr=0x%08x freq=%llu; test queue hr=0x%08x; test fence hr=0x%08x; test allocator hr=0x%08x; test list hr=0x%08x; test texture hr=0x%08x; descriptor heap hr=0x%08x; copy queue hr=0x%08x; compute queue hr=0x%08x; shared fence hr=0x%08x; GetBuffer(0) hr=0x%08x; GetParent hr=0x%08x\n",
                 (unsigned)probes->queueTimestampHr, (unsigned long long)probes->queueTimestampFreq,
                 (unsigned)probes->testQueueHr, (unsigned)probes->testFenceHr,
                 (unsigned)probes->testAllocatorHr, (unsigned)probes->testListHr,
                 (unsigned)probes->testTextureHr, (unsigned)probes->heapHr,
                 (unsigned)probes->copyQueueHr, (unsigned)probes->computeQueueHr,
                 (unsigned)probes->sharedFenceHr, (unsigned)probes->getBufferHr,
                 (unsigned)probes->getParentHr);
        fwprintf(f, L"probes: swapchain3 hr=0x%08x backBufferIndex=%u; swapchain4 hr=0x%08x; fullscreen hr=0x%08x fullscreen=%d; containing output hr=0x%08x\n",
                 (unsigned)probes->sc3Hr, (unsigned)probes->backBufferIndex,
                 (unsigned)probes->sc4Hr, (unsigned)probes->fullscreenHr,
                 (int)probes->fullscreen, (unsigned)probes->outputHr);
    }
    fwprintf(f, L"wrap result: code=%d (%s)\n", resultCode, wrapped ? L"wrapped OK" : L"FAILED");
    fclose(f);
}

} // namespace

extern "C" {

int fsr3_bridge_version(void) { return FSR3_BRIDGE_ABI_VERSION; }

int fsr3_fg_create(
    void *device, void *gameQueue,
    uint32_t displayWidth, uint32_t displayHeight,
    uint32_t renderWidth, uint32_t renderHeight,
    uint32_t backBufferFormat, uint32_t flags,
    void **outFg) {
    if (!device || !gameQueue || !outFg || displayWidth == 0 || displayHeight == 0) return -1;
    auto *fg = new (std::nothrow) Fsr3FgState{};
    if (!fg) return -1;
    if (!loadAmd(fg->api)) {
        delete fg;
        return -1;
    }
    fg->device = static_cast<ID3D12Device *>(device);
    fg->gameQueue = static_cast<ID3D12CommandQueue *>(gameQueue);
    fg->displayWidth = displayWidth;
    fg->displayHeight = displayHeight;
    fg->renderWidth = renderWidth;
    fg->renderHeight = renderHeight;
    fg->backBufferFormat = backBufferFormat;
    fg->flags = flags;

    int code = createFgContext(fg);
    if (code != 0) {
        delete fg;
        return code;
    }
    *outFg = fg;
    return 0;
}

int fsr3_fg_recreate(
    void *fgIn,
    uint32_t displayWidth, uint32_t displayHeight,
    uint32_t renderWidth, uint32_t renderHeight,
    uint32_t backBufferFormat, uint32_t flags) {
    auto *fg = static_cast<Fsr3FgState *>(fgIn);
    if (!fg || displayWidth == 0 || displayHeight == 0) return -1;
    // The old context goes first; the wrapped swap chain (its
    // swapchain context, whose callbacks point at this Fsr3FgState)
    // stays alive and is ResizeBuffers'd by the host right after.
    if (fg->fgContext) {
        fg->api.destroyContext(&fg->fgContext, nullptr);
        fg->fgContext = nullptr;
    }
    fg->displayWidth = displayWidth;
    fg->displayHeight = displayHeight;
    fg->renderWidth = renderWidth;
    fg->renderHeight = renderHeight;
    fg->backBufferFormat = backBufferFormat;
    fg->flags = flags;
    fg->lastFrameID = 0;
    fg->enabled = false;
    return createFgContext(fg);
}

int fsr3_fg_wrap_swapchain(void *fgIn, void **swapchainInOut, void **outWrapped) {
    auto *fg = static_cast<Fsr3FgState *>(fgIn);
    if (!fg || !swapchainInOut || !*swapchainInOut || !outWrapped) return -1;
    auto *swapchain = static_cast<IDXGISwapChain4 *>(*swapchainInOut);

    // Hold one extra reference on the chain across the call. The DLL
    // takes the chain by pointer-to-pointer, and a failed creation can
    // consume a reference to it in its error cleanup - that is the
    // prime suspect for the first FSR run's crash: the wrap failed
    // with RUNTIME_ERROR, and half a second later the host crashed in
    // dxgi.dll presenting through a chain whose reference was gone.
    // The extra reference is never released: one leaked reference on
    // a chain that lives for the process anyway.
    swapchain->AddRef();

    // Diagnostics for the wrap failure (code 3 = RUNTIME_ERROR: a
    // D3D12 call inside AMD's DLL failed). The chain's desc and
    // whether its device is the FG context's device decide the next
    // fix; they are written to fsr3_wrap_diag.txt beside this DLL.
    DXGI_SWAP_CHAIN_DESC1 desc1{};
    HRESULT hrDesc = swapchain->GetDesc1(&desc1);
    ID3D12Device *chainDevice = nullptr;
    HRESULT hrDev = swapchain->GetDevice(IID_PPV_ARGS(&chainDevice));
    bool deviceMatches = (hrDev == S_OK && chainDevice && chainDevice == fg->device);
    if (chainDevice) chainDevice->Release();

    // Probe before the call, so the diag file says which of the
    // D3D12 calls the wrap needs is the one that fails.
    WrapProbes probes = probeWrap(fg->device, fg->gameQueue, swapchain);

    ffxCreateContextDescFrameGenerationSwapChainWrapDX12 wrapDesc{};
    wrapDesc.header.type = FFX_API_CREATE_CONTEXT_DESC_TYPE_FRAMEGENERATIONSWAPCHAIN_WRAP_DX12;
    wrapDesc.swapchain = &swapchain;
    wrapDesc.gameQueue = fg->gameQueue;

    ffxReturnCode_t code = fg->api.createContext(&fg->swapContext, &wrapDesc.header, nullptr);
    bool ok = (code == FFX_API_RETURN_OK && fg->swapContext && swapchain);
    wchar_t folder[MAX_PATH];
    if (bridgeFolder(folder, MAX_PATH)) {
        writeWrapDiag(folder, &desc1, hrDesc, deviceMatches, hrDev, fg->gameQueue,
                      static_cast<int>(code), ok, &probes);
    }
    if (!ok) {
        // A failed creation can leave a half-made swapchain context
        // behind; destroy it so nothing half-initialised survives.
        if (fg->swapContext) {
            fg->api.destroyContext(&fg->swapContext, nullptr);
            fg->swapContext = nullptr;
        }
        return static_cast<int>(code);
    }
    // The wrap replaces the chain in place; `swapchain` now points at
    // the frame-interpolation proxy. Hand it back with a new reference.
    swapchain->AddRef();
    *outWrapped = swapchain;
    return 0;
}

int fsr3_fg_prepare(
    void *fgIn, void *commandList,
    void *depth, uint32_t depthState,
    void *motionVectors, uint32_t motionVectorState,
    float jitterX, float jitterY,
    float motionVectorScaleX, float motionVectorScaleY,
    float frameTimeMs, int reset,
    float cameraNear, float cameraFar, float cameraFovAngleVertical,
    uint64_t frameID) {
    auto *fg = static_cast<Fsr3FgState *>(fgIn);
    if (!fg || !commandList || !depth || !motionVectors) return -1;
    if (fg->lastFrameID != 0 && frameID != fg->lastFrameID + 1) {
        // A step other than +1 resets FSR 3's logic; that is expected
        // after a scene cut, so it is not an error here - the reset
        // flag carries it.
    }
    (void)reset; // FSR 3.1.4's prepare has only `unused_reset`, which
                 // the implementation ignores (see the file header):
                 // there is no working reset flag in this SDK.

    ffxDispatchDescFrameGenerationPrepare prepare{};
    prepare.header.type = FFX_API_DISPATCH_DESC_TYPE_FRAMEGENERATION_PREPARE;
    prepare.frameID = static_cast<uint32_t>(frameID);
    prepare.flags = 0;
    prepare.commandList = commandList;
    prepare.renderSize = {fg->renderWidth, fg->renderHeight};
    prepare.jitterOffset = {jitterX, jitterY};
    prepare.motionVectorScale = {motionVectorScaleX, motionVectorScaleY};
    prepare.frameTimeDelta = frameTimeMs;
    prepare.unused_reset = false;
    prepare.cameraNear = cameraNear;
    prepare.cameraFar = cameraFar;
    prepare.cameraFovAngleVertical = cameraFovAngleVertical;
    prepare.viewSpaceToMetersFactor = 1.0f;
    prepare.depth = apiResource(static_cast<ID3D12Resource *>(depth), depthState);
    prepare.motionVectors = apiResource(static_cast<ID3D12Resource *>(motionVectors), motionVectorState);

    // The camera info is a linked struct after the prepare desc -
    // a required input in FSR 3.1.4. The title gives no camera
    // matrices (see the file header).
    ffxDispatchDescFrameGenerationPrepareCameraInfo cameraInfo{};
    cameraInfo.cameraPosition[0] = 0.0f;
    cameraInfo.cameraPosition[1] = 0.0f;
    cameraInfo.cameraPosition[2] = 0.0f;
    cameraInfo.cameraUp[0] = 0.0f;
    cameraInfo.cameraUp[1] = 1.0f;
    cameraInfo.cameraUp[2] = 0.0f;
    cameraInfo.cameraRight[0] = 1.0f;
    cameraInfo.cameraRight[1] = 0.0f;
    cameraInfo.cameraRight[2] = 0.0f;
    cameraInfo.cameraForward[0] = 0.0f;
    cameraInfo.cameraForward[1] = 0.0f;
    cameraInfo.cameraForward[2] = -1.0f;
    prepare.header.pNext = reinterpret_cast<decltype(prepare.header.pNext)>(
        linkHeader(cameraInfo, FFX_API_DISPATCH_DESC_TYPE_FRAMEGENERATION_PREPARE_CAMERAINFO));

    ffxReturnCode_t code = fg->api.dispatch(&fg->fgContext, &prepare.header);
    if (code == FFX_API_RETURN_OK) fg->lastFrameID = frameID;
    return static_cast<int>(code);
}

int fsr3_fg_configure(
    void *fgIn, void *swapChain,
    void *hudlessColor, uint32_t hudlessState,
    uint64_t frameID, int enabled) {
    auto *fg = static_cast<Fsr3FgState *>(fgIn);
    if (!fg || !swapChain) return -1;

    ffxConfigureDescFrameGeneration config{};
    config.header.type = FFX_API_CONFIGURE_DESC_TYPE_FRAMEGENERATION;
    config.swapChain = swapChain;
    config.frameGenerationCallback = &frameGenerationCallback;
    config.frameGenerationCallbackUserContext = fg;
    config.presentCallback = &presentCallback;
    config.presentCallbackUserContext = fg;
    config.frameGenerationEnabled = enabled != 0;
    config.allowAsyncWorkloads = (fg->flags & FSR3_FG_FLAG_ASYNC_WORKLOADS) != 0;
    config.HUDLessColor = hudlessColor
        ? apiResource(static_cast<ID3D12Resource *>(hudlessColor), hudlessState)
        : FfxApiResource{};
    config.flags = 0;
    config.onlyPresentGenerated = false;
    config.generationRect = {0, 0, static_cast<int32_t>(fg->displayWidth), static_cast<int32_t>(fg->displayHeight)};
    config.frameID = static_cast<uint32_t>(frameID);

    ffxReturnCode_t code = fg->api.configure(&fg->fgContext, &config.header);
    if (code == FFX_API_RETURN_OK) fg->enabled = enabled != 0;
    return static_cast<int>(code);
}

void fsr3_fg_destroy(void *fgIn) {
    auto *fg = static_cast<Fsr3FgState *>(fgIn);
    if (!fg) return;
    if (fg->swapContext) fg->api.destroyContext(&fg->swapContext, nullptr);
    if (fg->fgContext) fg->api.destroyContext(&fg->fgContext, nullptr);
    if (fg->api.module) FreeLibrary(fg->api.module);
    delete fg;
}

} // extern "C"
