//! A bounded capture in every build. The title's frame token, host list pointer,
//! recording ID and CPU clock let the command, queue and Present rows be joined.
//! GPU timestamps are in ticks. `execute` reports the queue's frequency and a
//! pair of GPU and CPU performance-counter timestamps for calibration.
//! Set `VICTORY_RENDER_TRACE` to a new file path, `VICTORY_RENDER_TRACE_AT` to
//! the start time in seconds (default 60), and `VICTORY_RENDER_TRACE_SECONDS`
//! to the duration (default 3). The extra GPU queries affect timings during
//! capture. Use `list_trace_lost` and `capture_loss` when checking coverage.

use std::fmt::Write as _;
use std::io::{BufWriter, Write};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::time::Duration;

use super::{now_us, thread_id};
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandList, ID3D12CommandQueue};
use windows::core::Interface;

struct Capture {
    start_us: u64,
    end_us: u64,
    sender: SyncSender<Output>,
}

enum Output {
    Lines(String),
    #[cfg(test)]
    Flush(std::sync::mpsc::Sender<()>),
}

static DROPPED: AtomicU64 = AtomicU64::new(0);

fn capture() -> Option<&'static Capture> {
    static CAPTURE: OnceLock<Option<Capture>> = OnceLock::new();
    CAPTURE
        .get_or_init(|| {
            let path =
                std::env::var_os("VICTORY_RENDER_TRACE").filter(|p| !p.is_empty() && p != "0")?;
            let seconds = |key, default| {
                std::env::var(key)
                    .ok()
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(default)
            };
            let start_us = seconds("VICTORY_RENDER_TRACE_AT", 60).saturating_mul(1_000_000);
            let end_us = start_us.saturating_add(
                seconds("VICTORY_RENDER_TRACE_SECONDS", 3).clamp(1, 30) * 1_000_000,
            );
            let file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(file) => file,
                Err(error) => {
                    tracing::warn!(?path, %error, "could not open VICTORY_RENDER_TRACE");
                    return None;
                }
            };
            let (sender, receiver) = sync_channel::<Output>(1024);
            let worker = std::thread::Builder::new()
                .name("victory-render-trace".into())
                .spawn(move || -> std::io::Result<()> {
                    let mut out = BufWriter::with_capacity(256 * 1024, file);
                    writeln!(
                        out,
                        "# VIctory render trace v1, tab-separated event and key=value fields"
                    )?;
                    loop {
                        match receiver.recv_timeout(Duration::from_millis(100)) {
                            Ok(Output::Lines(batch)) => out.write_all(batch.as_bytes())?,
                            #[cfg(test)]
                            Ok(Output::Flush(done)) => {
                                out.flush()?;
                                let _ = done.send(());
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                let dropped = DROPPED.swap(0, Relaxed);
                                if dropped != 0 {
                                    writeln!(out, "capture_loss\tbatches={dropped}")?;
                                }
                                out.flush()?;
                            }
                            Err(RecvTimeoutError::Disconnected) => return out.flush(),
                        }
                    }
                });
            match worker {
                Ok(_) => {
                    tracing::info!(?path, start_us, end_us, "VIctory render trace enabled");
                    Some(Capture {
                        start_us,
                        end_us,
                        sender,
                    })
                }
                Err(error) => {
                    tracing::warn!(%error, "could not start the render trace writer");
                    None
                }
            }
        })
        .as_ref()
}

pub(crate) fn active() -> bool {
    capture().is_some_and(|c| (c.start_us..c.end_us).contains(&now_us()))
}

/// Used also for GPU results that arrive after the capture window.
pub(super) fn write(batch: String) {
    if let Some(c) = capture() {
        match c.sender.try_send(Output::Lines(batch)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                DROPPED.fetch_add(1, Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => {
                static SAID: std::sync::Once = std::sync::Once::new();
                SAID.call_once(|| tracing::warn!("the render trace writer stopped"));
            }
        }
    }
}

#[cfg(test)]
pub(super) fn flush() {
    if let Some(c) = capture() {
        let (done, wait) = std::sync::mpsc::channel();
        c.sender.send(Output::Flush(done)).unwrap();
        wait.recv_timeout(Duration::from_secs(1)).unwrap();
    }
}

pub(crate) fn event(kind: &str, fields: impl std::fmt::Display) {
    write(format!(
        "{kind}\tcpu_us={}\tthread={}\tframe={}\t{fields}\n",
        now_us(),
        thread_id(),
        crate::frame::current()
    ));
}

pub(crate) fn execute(
    queue: &ID3D12CommandQueue,
    lists: &[Option<ID3D12CommandList>],
    signal: Option<(usize, u64)>,
) {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let batch = NEXT.fetch_add(1, Relaxed);
    let mut gpu = 0;
    let mut qpc = 0;
    // SAFETY: plain clock queries on a live queue.
    let (hz, kind) = unsafe {
        let _ = queue.GetClockCalibration(&mut gpu, &mut qpc);
        (
            queue.GetTimestampFrequency().unwrap_or(0),
            queue.GetDesc().Type.0,
        )
    };
    let at = now_us();
    let mut rows = format!(
        "execute\tbatch={batch}\tcpu_us={at}\tthread={}\tframe={}\tqueue={:#x}\tqueue_type={kind}\tgpu_tick={gpu}\tqpc={qpc}\tgpu_hz={hz}\tfence={signal:?}\n",
        thread_id(),
        crate::frame::current(),
        queue.as_raw() as usize
    );
    for (index, list) in lists.iter().enumerate() {
        if let Some(list) = list {
            let _ = writeln!(
                rows,
                "execute_list\tbatch={batch}\tcpu_us={at}\tindex={index}\tlist={:#x}",
                list.as_raw() as usize
            );
        }
    }
    write(rows);
}

/// A command's actual host arguments. The pipeline's readable name is held
/// separately in the GPU timing module.
#[derive(Clone, Copy)]
pub(crate) struct Command(pub &'static str, pub [i64; 5]);

/// The translated draw/dispatch can return before there is a GPU command.
pub(crate) struct DrawCall {
    start: Option<u64>,
    list: usize,
    pipeline: usize,
    command: Command,
}

impl DrawCall {
    pub(crate) fn begin(list: &super::HostList, pipeline: usize, command: Command) -> Self {
        Self {
            start: active().then(now_us),
            list: list.list.as_raw() as usize,
            pipeline,
            command,
        }
    }

    pub(crate) fn recorded(mut self) {
        if let Some(start) = self.start.take() {
            let duration_us = now_us().saturating_sub(start);
            if duration_us >= 500 {
                self.report("slow_draw_translation", start, duration_us);
            }
        }
    }

    fn report(&self, event_name: &str, start: u64, duration_us: u64) {
        let Command(op, args) = self.command;
        event(
            event_name,
            format_args!(
                "list={:#x}\tpipeline={:#x}\top={op}\targs={args:?}\tstart_us={start}\tduration_us={duration_us}",
                self.list, self.pipeline
            ),
        );
    }
}

impl Drop for DrawCall {
    fn drop(&mut self) {
        if let Some(start) = self.start {
            self.report("draw_not_recorded", start, now_us().saturating_sub(start));
        }
    }
}
