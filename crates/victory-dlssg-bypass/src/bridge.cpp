#include "app/loader.h"
#include "core/log.h"
#include "kernels/substitute.h"
#include "provider/multi_frame.h"
#include "spoof/nvapi.h"
#include "streamline/plugin_patch.h"
#include "streamline/streamline.h"
#include "version.h"

#include <atomic>
#include <string>
#include <vector>

namespace {

std::atomic<bool> g_initialized{false};

} // namespace

extern "C" bool victory_dlssg_bypass_init(bool multi_frame, const wchar_t* log_dir) {
    bool expected = false;
    if (!g_initialized.compare_exchange_strong(expected, true))
        return true;

    if (log_dir && log_dir[0] != L'\0') {
        odg::log::Open(log_dir, odg::log::Level::Info, L"dlssg_bypass");
        odg::log::Header("attach",
                         {odg::log::Field::Str("version", ODG_VERSION),
                          odg::log::Field::Str("mode", "victory_builtin"),
                          odg::log::Field::Bool("multi_frame", multi_frame)});
    }

    odg::spoof::nvapi::SetSpoofEnabled(true);
    odg::spoof::nvapi::SetSpoofCallers(
        {L"sl.common.dll", L"_nvngx.dll", L"nvngx_dlssg.dll"});
    odg::spoof::nvapi::SetStubScgPriority(true);

    odg::streamline::SetPatchesEnabled(true, false);
    odg::streamline::SetFlipMeteringValue(-1);
    odg::streamline::SetForceMultiplier(0);
    odg::streamline::SetDiagnostics(false);
    odg::provider::SetMultiFrameEnabled(multi_frame);

    odg::kernels::Options options;
    options.enabled = true;
    options.target_sm = 0; // query CUDA driver for the host GPU's actual SM
    options.multi_frame = multi_frame;
    options.dump = false;
    odg::kernels::Configure(options);

    odg::app::loader::SetVulkanHooksEnabled(false);
    odg::app::loader::Start();
    return true;
}
