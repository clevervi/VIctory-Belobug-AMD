//! A debug overlay drawn into each presented frame. It is off unless
//! `VICTORY_OVERLAY` is set: `VICTORY_OVERLAY=1` turns it on, and Ctrl+F10,
//! while our window has focus, shows and hides it. `VICTORY_OVERLAY=show`
//! also shows it from the start.
//!
//! It shows the frame rate and frame times (with a graph of the last
//! frames and the 1% low), the presenter's last second (draws per frame,
//! time preparing submissions and waiting for the GPU), the process's CPU
//! use and memory, video memory against its budget, and the controller as
//! the title last read it. The title keeps its keyboard and controller
//! input while it shows.
//!
//! The panel is drawn on the CPU into upload memory in the back buffer's
//! format, and copied into its top left corner after the frame, before the
//! present: no shaders or pipelines, and nothing at all while it is hidden.
//! Text is Consolas, antialiased, through GDI (our own 5x7 font where GDI
//! makes none); the graphs have labelled axes and reference lines.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, Once};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::host::Host;

static VISIBLE: AtomicBool = AtomicBool::new(false);

/// Whether the overlay shows.
pub fn shown() -> bool {
    VISIBLE.load(Relaxed)
}

/// A controller as the title reads it: GameInput's buttons, triggers and
/// sticks (-1 to 1, y up).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Pad {
    pub buttons: u32,
    pub triggers: [f32; 2],
    pub left: [f32; 2],
    pub right: [f32; 2],
}

static PAD: Mutex<Option<Pad>> = Mutex::new(None);

/// The controller the title reads now (`None`: none).
pub fn show_pad(pad: Option<Pad>) {
    *PAD.lock().unwrap_or_else(|e| e.into_inner()) = pad;
}

/// The presenter's last second, per frame (`crate::host::Meter`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Second {
    pub(crate) draws: f64,
    pub(crate) prepare_ms: f64,
    pub(crate) gpu_wait_ms: f64,
}

static SECOND: Mutex<Second> = Mutex::new(Second {
    draws: 0.0,
    prepare_ms: 0.0,
    gpu_wait_ms: 0.0,
});

pub(crate) fn second(s: Second) {
    *SECOND.lock().unwrap_or_else(|e| e.into_inner()) = s;
}

/// GPU time of the submissions since the last present, in nanoseconds:
/// a timestamp before and after each one ([`time_lists`]), while the
/// overlay shows.
static GPU_NS: AtomicU64 = AtomicU64::new(0);

/// The submission timer: two small lists that write a timestamp each, the
/// second also resolving both into the read-back buffer.
struct Timer {
    allocator: ID3D12CommandAllocator,
    begin: ID3D12GraphicsCommandList,
    end: ID3D12GraphicsCommandList,
    heap: ID3D12QueryHeap,
    readback: ID3D12Resource,
    /// Ticks a second.
    frequency: u64,
    /// A timed submission was sent and not read yet.
    pending: bool,
}

// SAFETY: D3D12 objects are free-threaded; the timer is used under its lock.
unsafe impl Send for Timer {}

static TIMER: Mutex<Option<Option<Timer>>> = Mutex::new(None);

impl Timer {
    fn new(device: &ID3D12Device, queue: &ID3D12CommandQueue) -> Option<Timer> {
        // SAFETY: plain object creation; the lists start closed.
        unsafe {
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .ok()?;
            let list = || -> Option<ID3D12GraphicsCommandList> {
                let l: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                l.Close().ok()?;
                Some(l)
            };
            let (begin, end) = (list()?, list()?);
            let mut heap: Option<ID3D12QueryHeap> = None;
            device
                .CreateQueryHeap(
                    &D3D12_QUERY_HEAP_DESC {
                        Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                        Count: 2,
                        NodeMask: 0,
                    },
                    &mut heap,
                )
                .ok()?;
            let mut readback: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_READBACK,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 16,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    None,
                    &mut readback,
                )
                .ok()?;
            Some(Timer {
                allocator,
                begin,
                end,
                heap: heap?,
                readback: readback?,
                frequency: queue.GetTimestampFrequency().ok()?.max(1),
                pending: false,
            })
        }
    }

    /// Records the two lists afresh (the last submission was waited for).
    fn record(&mut self) -> Option<()> {
        // SAFETY: the allocator's lists ran (every submission is waited
        // for); the heap and buffer are ours.
        unsafe {
            self.allocator.Reset().ok()?;
            self.begin.Reset(&self.allocator, None).ok()?;
            self.begin
                .EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, 0);
            self.begin.Close().ok()?;
            self.end.Reset(&self.allocator, None).ok()?;
            self.end.EndQuery(&self.heap, D3D12_QUERY_TYPE_TIMESTAMP, 1);
            self.end.ResolveQueryData(
                &self.heap,
                D3D12_QUERY_TYPE_TIMESTAMP,
                0,
                2,
                &self.readback,
                0,
            );
            self.end.Close().ok()?;
        }
        Some(())
    }
}

/// While the overlay shows: `group` between the timer's two lists, to
/// submit in its place ([`time_read`] once it has run); `None` otherwise.
pub(crate) fn time_lists(
    device: &ID3D12Device,
    queue: &ID3D12CommandQueue,
    group: &[Option<ID3D12CommandList>],
) -> Option<Vec<Option<ID3D12CommandList>>> {
    if !shown() {
        return None;
    }
    let mut slot = TIMER.lock().unwrap_or_else(|e| e.into_inner());
    let timer = slot
        .get_or_insert_with(|| Timer::new(device, queue))
        .as_mut()?;
    timer.record()?;
    timer.pending = true;
    let mut lists = Vec::with_capacity(group.len() + 2);
    lists.push(Some(timer.begin.clone().into()));
    lists.extend_from_slice(group);
    lists.push(Some(timer.end.clone().into()));
    Some(lists)
}

/// After a submission [`time_lists`] timed has run: adds its GPU time to
/// the frame's.
pub(crate) fn time_read() {
    let mut slot = TIMER.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(timer)) = slot.as_mut() else {
        return;
    };
    if !std::mem::take(&mut timer.pending) {
        return;
    }
    let mut mapped = std::ptr::null_mut();
    let range = D3D12_RANGE { Begin: 0, End: 16 };
    // SAFETY: the resolve ran (the submission was waited for); the buffer
    // holds two timestamps.
    let ticks = unsafe {
        if timer
            .readback
            .Map(0, Some(&range), Some(&mut mapped))
            .is_err()
        {
            return;
        }
        let [a, b] = mapped.cast::<[u64; 2]>().read();
        timer.readback.Unmap(0, Some(&D3D12_RANGE::default()));
        b.saturating_sub(a)
    };
    let ns = u128::from(ticks) * 1_000_000_000 / u128::from(timer.frequency);
    GPU_NS.fetch_add(ns as u64, Relaxed);
}

/// When the title first read input since the last present, in nanoseconds
/// past [`epoch`] (0: not yet).
static FIRST_READ: AtomicU64 = AtomicU64::new(0);

fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// The title reads its input (GameInput's current reading): the first read
/// after a present starts the input-to-present time of the next frame.
pub fn note_input_read() {
    if shown() {
        let at = (Instant::now() - epoch()).as_nanos().max(1) as u64;
        let _ = FIRST_READ.compare_exchange(0, at, Relaxed, Relaxed);
    }
}

/// Frame times kept for the graph and the 1% low.
const HISTORY: usize = 240;
/// The panel is redrawn at most this often; it is copied every frame.
const REDRAW: Duration = Duration::from_millis(250);
/// The process and video memory are asked at most this often.
const SAMPLE: Duration = Duration::from_secs(1);
/// The panel's distance from the corner, in pixels at scale 1.
const MARGIN: u32 = 12;
/// Seconds of CPU and video memory history.
const SECONDS: usize = 60;

/// The panel's scale for a back buffer `height` pixels high: 1 at 1080
/// lines, 0.75 at 720, up to 2 at 2160.
fn scale_for(height: u32) -> f32 {
    (height as f32 / 1080.0).clamp(0.75, 2.0)
}

/// The presenter's overlay state.
pub(crate) struct Overlay {
    enabled: bool,
    /// Ctrl+F10 was down at the last frame (only without the key thread,
    /// [`watch_keys`]).
    down: bool,
    last: Option<Instant>,
    times: VecDeque<f32>,
    /// Per frame: GPU busy time and input-to-present time, ms (`None`:
    /// not measured that frame).
    gpu: VecDeque<Option<f32>>,
    latency: VecDeque<Option<f32>>,
    sampled: Option<(Instant, System)>,
    /// CPU use, GPU busy time of the frame time, and video memory used of
    /// its budget, in percent, a sample a second, for the history graph.
    history: VecDeque<[Option<f32>; 3]>,
    cpu: Option<(Instant, u64)>,
    adapter: Option<Option<IDXGIAdapter3>>,
    /// Upload memory (its address as a number: the presenter lives behind
    /// a lock other threads take), its size, and when it was last drawn.
    upload: Option<(ID3D12Resource, usize, u64)>,
    drawn: Option<(Instant, Image, i32)>,
}

impl Default for Overlay {
    fn default() -> Self {
        let switch = std::env::var_os("VICTORY_OVERLAY");
        if switch.as_deref().is_some_and(|v| v == "show") {
            VISIBLE.store(true, Relaxed);
        }
        Overlay {
            enabled: switch.as_deref().is_some_and(|v| v != "0"),
            down: false,
            last: None,
            times: VecDeque::with_capacity(HISTORY),
            gpu: VecDeque::with_capacity(HISTORY),
            latency: VecDeque::with_capacity(HISTORY),
            sampled: None,
            history: VecDeque::with_capacity(SECONDS),
            cpu: None,
            adapter: None,
            upload: None,
            drawn: None,
        }
    }
}

/// How often the key thread looks at the keys. A world frame can take a
/// second or more, and a key pressed and let go between two frames was
/// lost when the keys were looked at once a frame.
const POLL: Duration = Duration::from_millis(15);

/// The window whose keys the overlay takes, as a number (0: none yet).
static WINDOW: AtomicUsize = AtomicUsize::new(0);
/// The key thread runs.
static KEY_THREAD: AtomicBool = AtomicBool::new(false);

/// Starts the key thread the first time, and points it at `hwnd`; whether
/// it runs (if not, the caller looks at the keys itself, once a frame).
fn watch_keys(hwnd: HWND) -> bool {
    static START: Once = Once::new();
    WINDOW.store(hwnd.0 as usize, Relaxed);
    START.call_once(|| {
        let spawned = std::thread::Builder::new()
            .name("victory overlay keys".into())
            .spawn(|| {
                let mut down = false;
                loop {
                    down = poll_keys(down, HWND(WINDOW.load(Relaxed) as *mut _));
                    std::thread::sleep(POLL);
                }
            });
        match spawned {
            Ok(_) => KEY_THREAD.store(true, Relaxed),
            Err(e) => tracing::warn!(%e, "overlay: no key thread; keys are read once a frame"),
        }
    });
    KEY_THREAD.load(Relaxed)
}

/// Ctrl+F10 shows and hides the panel, only while `hwnd` has focus, when it
/// goes down: `before` is whether it was down at the last look, and
/// whether it is down now is returned for the next.
fn poll_keys(before: bool, hwnd: HWND) -> bool {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
    // SAFETY: a plain query.
    let focused = !hwnd.is_invalid() && unsafe { GetForegroundWindow() } == hwnd;
    // SAFETY: a plain query.
    let down = toggle_down(|vk| focused && unsafe { GetAsyncKeyState(vk) } < 0);
    if down && !before {
        let shown = !VISIBLE.fetch_xor(true, Relaxed);
        tracing::info!(shown, "overlay");
    }
    down
}

/// Whether Ctrl+F10 is held.
fn toggle_down(held: impl Fn(i32) -> bool) -> bool {
    const CTRL: i32 = 0x11;
    const F10: i32 = 0x79;
    held(CTRL) && held(F10)
}

/// What is sampled once a second.
#[derive(Clone, Copy, Debug, Default)]
struct System {
    cpu: Option<f64>,
    private: u64,
    working: u64,
    ram: (u64, u64),
    vram: Option<(u64, u64, u64)>,
}

impl Overlay {
    /// Counts the frame, polls the keys and, if the panel shows (`showing`,
    /// as the present read it, so both agree for the frame) and fits,
    /// records its copy into `back` (`width` x `height` of `format`, in
    /// `state`) on `list`, moving `back` to the copy destination state
    /// first. Returns the state `back` is left in.
    pub(crate) fn record(
        &mut self,
        host: &Host,
        list: &ID3D12GraphicsCommandList,
        back: &ID3D12Resource,
        (width, height, format): (u32, u32, DXGI_FORMAT),
        hwnd: HWND,
        state: D3D12_RESOURCE_STATES,
        showing: bool,
    ) -> D3D12_RESOURCE_STATES {
        if !self.enabled {
            return state;
        }
        let now = Instant::now();
        let gpu_ns = GPU_NS.swap(0, Relaxed);
        let read = FIRST_READ.swap(0, Relaxed);
        if let Some(last) = self.last.replace(now) {
            let push = |q: &mut VecDeque<Option<f32>>, v| {
                if q.len() == HISTORY {
                    q.pop_front();
                }
                q.push_back(v);
            };
            if self.times.len() == HISTORY {
                self.times.pop_front();
            }
            self.times.push_back((now - last).as_secs_f32() * 1000.0);
            let gpu = (showing && gpu_ns > 0).then(|| gpu_ns as f32 / 1e6);
            push(&mut self.gpu, gpu);
            let latency = (read > 0).then(|| {
                let at = epoch() + Duration::from_nanos(read);
                now.saturating_duration_since(at).as_secs_f32() * 1000.0
            });
            push(&mut self.latency, latency);
        }
        if !watch_keys(hwnd) {
            self.down = poll_keys(self.down, hwnd);
        }
        if !showing {
            self.drawn = None;
            return state;
        }
        let Some(pixel) = Pixel::of(format) else {
            return state;
        };
        if self
            .sampled
            .as_ref()
            .is_none_or(|(at, _)| now - *at >= SAMPLE)
        {
            let system = self.sample(host);
            if self.history.len() == SECONDS {
                self.history.pop_front();
            }
            let vram = system
                .vram
                .map(|(used, budget, _)| (used as f64 / budget.max(1) as f64 * 100.0) as f32);
            let gpu = busy_percent(&self.times, &self.gpu);
            self.history
                .push_back([system.cpu.map(|c| c as f32), gpu, vram]);
            self.sampled = Some((now, system));
        }
        let scale = scale_for(height);
        let fresh = self.drawn.as_ref().is_some_and(|(at, image, f)| {
            now - *at < REDRAW && (image.scale - scale).abs() < 1e-3 && *f == format.0
        });
        if !fresh {
            let system = self.sampled.map(|(_, s)| s).unwrap_or_default();
            let image = self.panel(scale, &system);
            self.drawn = Some((now, image, -1));
        }
        let Some((_, image, written)) = self.drawn.as_mut() else {
            return state;
        };
        let margin = (MARGIN as f32 * image.scale).round() as u32;
        if image.width + margin > width || image.height + margin > height {
            return state;
        }
        let row_pitch =
            (image.width * pixel.bytes()).next_multiple_of(D3D12_TEXTURE_DATA_PITCH_ALIGNMENT);
        let size = u64::from(row_pitch) * u64::from(image.height);
        if self.upload.as_ref().is_none_or(|&(_, _, have)| have < size) {
            *written = -1;
            self.upload = host
                .upload_buffer(size)
                .map(|(buffer, data)| (buffer, data as usize, size));
        }
        let Some((buffer, data, have)) = &self.upload else {
            return state;
        };
        if *written != format.0 {
            // SAFETY: the buffer is mapped for `have` bytes, and the GPU is
            // done with it: every present's submission is waited for.
            let out = unsafe { std::slice::from_raw_parts_mut(*data as *mut u8, *have as usize) };
            image.write(pixel, row_pitch as usize, out);
            *written = format.0;
        }
        let dst = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location does not release.
            pResource: unsafe { std::mem::transmute_copy(back) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                SubresourceIndex: 0,
            },
        };
        let src = D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                    Offset: 0,
                    Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                        Format: format,
                        Width: image.width,
                        Height: image.height,
                        Depth: 1,
                        RowPitch: row_pitch,
                    },
                },
            },
        };
        // SAFETY: an open list; the back buffer is made a copy destination
        // and the upload buffer lives in `self` past the submission.
        unsafe {
            if state != D3D12_RESOURCE_STATE_COPY_DEST {
                list.ResourceBarrier(&[crate::host::transition(
                    back,
                    state,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                )]);
            }
            list.CopyTextureRegion(&dst, margin, margin, 0, &src, None);
        }
        D3D12_RESOURCE_STATE_COPY_DEST
    }

    /// The process's CPU use and memory, the machine's memory and the
    /// device's video memory.
    fn sample(&mut self, host: &Host) -> System {
        use windows_sys::Win32::System::ProcessStatus::{
            K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
        };
        use windows_sys::Win32::System::SystemInformation::{
            GetSystemInfo, GlobalMemoryStatusEx, MEMORYSTATUSEX, SYSTEM_INFO,
        };
        use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let mut s = System::default();
        // SAFETY: plain queries into structs of ours.
        unsafe {
            let (mut created, mut exited, mut kernel, mut user) = std::mem::zeroed();
            if GetProcessTimes(
                GetCurrentProcess(),
                &mut created,
                &mut exited,
                &mut kernel,
                &mut user,
            ) != 0
            {
                let ticks = |f: windows_sys::Win32::Foundation::FILETIME| {
                    u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime)
                };
                let busy = ticks(kernel) + ticks(user);
                let now = Instant::now();
                if let Some((at, before)) = self.cpu.replace((now, busy)) {
                    let mut info: SYSTEM_INFO = std::mem::zeroed();
                    GetSystemInfo(&mut info);
                    let wall = (now - at).as_nanos() as f64 / 100.0;
                    let cores = f64::from(info.dwNumberOfProcessors.max(1));
                    s.cpu = Some(busy.saturating_sub(before) as f64 / wall / cores * 100.0);
                }
            }
            let mut m: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
            m.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
            if K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut m).cast(), m.cb) != 0 {
                (s.private, s.working) = (m.PrivateUsage as u64, m.WorkingSetSize as u64);
            }
            let mut g: MEMORYSTATUSEX = std::mem::zeroed();
            g.dwLength = size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut g) != 0 {
                s.ram = (g.ullTotalPhys - g.ullAvailPhys, g.ullTotalPhys);
            }
        }
        let adapter = self.adapter.get_or_insert_with(|| {
            // SAFETY: plain DXGI queries.
            unsafe {
                let factory: IDXGIFactory4 =
                    CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
                factory
                    .EnumAdapterByLuid::<IDXGIAdapter3>(host.device.GetAdapterLuid())
                    .ok()
            }
        });
        if let Some(adapter) = adapter {
            let query = |group| {
                let mut info = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
                // SAFETY: a live adapter and a struct of ours.
                unsafe { adapter.QueryVideoMemoryInfo(0, group, &mut info) }
                    .ok()
                    .map(|()| info)
            };
            if let (Some(local), Some(shared)) = (
                query(DXGI_MEMORY_SEGMENT_GROUP_LOCAL),
                query(DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL),
            ) {
                s.vram = Some((local.CurrentUsage, local.Budget, shared.CurrentUsage));
            }
        }
        s
    }

    /// The panel as it is now.
    fn panel(&self, scale: f32, system: &System) -> Image {
        let second = *SECOND.lock().unwrap_or_else(|e| e.into_inner());
        let pad = *PAD.lock().unwrap_or_else(|e| e.into_inner());
        let fonts = Fonts::new();
        draw_panel(
            &fonts,
            scale,
            &Facts {
                times: &self.times,
                history: &self.history,
                gpu: &self.gpu,
                latency: &self.latency,
                second,
                system: *system,
                pad,
            },
        )
    }
}

/// What the panel shows.
struct Facts<'a> {
    times: &'a VecDeque<f32>,
    history: &'a VecDeque<[Option<f32>; 3]>,
    gpu: &'a VecDeque<Option<f32>>,
    latency: &'a VecDeque<Option<f32>>,
    second: Second,
    system: System,
    pad: Option<Pad>,
}

/// Mean frame time, its rate, and the rate of the slowest 1% of frames.
fn frame_stats(times: &VecDeque<f32>) -> (f32, f32, f32) {
    if times.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = times.iter().sum::<f32>() / times.len() as f32;
    let mut sorted: Vec<f32> = times.iter().copied().collect();
    sorted.sort_by(|a, b| b.total_cmp(a));
    let n = (sorted.len() / 100).max(1);
    let worst = sorted[..n].iter().sum::<f32>() / n as f32;
    let rate = |ms: f32| if ms > 0.0 { 1000.0 / ms } else { 0.0 };
    (mean, rate(mean), rate(worst))
}

/// The mean of the last `n` measured values (`None`: none measured), and
/// the largest.
fn recent(values: &VecDeque<Option<f32>>, n: usize) -> Option<(f32, f32)> {
    let measured: Vec<f32> = values.iter().rev().take(n).flatten().copied().collect();
    let max = measured
        .iter()
        .copied()
        .fold(None, |m: Option<f32>, v| Some(m.map_or(v, |m| m.max(v))))?;
    Some((measured.iter().sum::<f32>() / measured.len() as f32, max))
}

/// GPU busy time over the last second of frames, percent of their time.
fn busy_percent(times: &VecDeque<f32>, gpu: &VecDeque<Option<f32>>) -> Option<f32> {
    let (mut wall, mut busy, mut measured) = (0.0, 0.0, false);
    for (&t, g) in times.iter().rev().zip(gpu.iter().rev()) {
        if wall >= 1000.0 {
            break;
        }
        wall += t;
        if let Some(g) = g {
            busy += g;
            measured = true;
        }
    }
    (measured && wall > 0.0).then(|| (busy / wall * 100.0).min(100.0))
}

/// Gigabytes, one decimal.
fn gb(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1u64 << 30) as f64)
}

/// An sRGB colour.
type Rgb = [u8; 3];

/// The panel's colours.
mod colour {
    use super::Rgb;
    pub(super) const BACK: Rgb = [16, 18, 24];
    pub(super) const BORDER: Rgb = [70, 78, 96];
    pub(super) const HEADER: Rgb = [110, 165, 255];
    pub(super) const LABEL: Rgb = [150, 157, 172];
    pub(super) const VALUE: Rgb = [236, 239, 246];
    pub(super) const GOOD: Rgb = [92, 214, 112];
    pub(super) const WARN: Rgb = [240, 196, 72];
    pub(super) const BAD: Rgb = [240, 92, 82];
    pub(super) const GRID: Rgb = [46, 51, 64];
    pub(super) const AXIS: Rgb = [116, 123, 140];
    pub(super) const CPU: Rgb = [80, 170, 255];
    pub(super) const GPU: Rgb = [255, 150, 70];
    pub(super) const VRAM: Rgb = [206, 128, 255];
    pub(super) const CHIP: Rgb = [38, 42, 54];
    pub(super) const LIT: Rgb = [70, 190, 255];
}

/// A frame rate's colour: 30 and up good, 20 and up a warning.
fn rate_colour(fps: f32) -> Rgb {
    if fps >= 29.0 {
        colour::GOOD
    } else if fps >= 20.0 {
        colour::WARN
    } else {
        colour::BAD
    }
}

/// The back buffer's pixel formats the presenter makes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pixel {
    Rgba8,
    Bgra8,
    Rgb10a2,
    Rgba16f,
}

impl Pixel {
    fn of(format: DXGI_FORMAT) -> Option<Pixel> {
        Some(match format {
            DXGI_FORMAT_R8G8B8A8_UNORM => Pixel::Rgba8,
            DXGI_FORMAT_B8G8R8A8_UNORM => Pixel::Bgra8,
            DXGI_FORMAT_R10G10B10A2_UNORM => Pixel::Rgb10a2,
            DXGI_FORMAT_R16G16B16A16_FLOAT => Pixel::Rgba16f,
            _ => return None,
        })
    }

    fn bytes(self) -> u32 {
        if self == Pixel::Rgba16f { 8 } else { 4 }
    }

    /// Colour `c` in this format, opaque: the unorm formats take the sRGB
    /// values as they are (the back buffer's bytes are shown as they are),
    /// the float format linear values.
    fn encode(self, c: Rgb) -> [u8; 8] {
        let [r, g, b] = c;
        let mut out = [0u8; 8];
        match self {
            Pixel::Rgba8 => out[..4].copy_from_slice(&[r, g, b, 255]),
            Pixel::Bgra8 => out[..4].copy_from_slice(&[b, g, r, 255]),
            Pixel::Rgb10a2 => {
                let ten = |x: u8| u32::from(x) * 1023 / 255;
                let v = ten(r) | ten(g) << 10 | ten(b) << 20 | 3 << 30;
                out[..4].copy_from_slice(&v.to_le_bytes());
            }
            Pixel::Rgba16f => {
                let linear = |x: u8| (f32::from(x) / 255.0).powf(2.2);
                for (k, x) in [linear(r), linear(g), linear(b), 1.0]
                    .into_iter()
                    .enumerate()
                {
                    out[2 * k..2 * k + 2].copy_from_slice(&half(x).to_le_bytes());
                }
            }
        }
        out
    }
}

/// `x` (0 to 1) as a half float.
fn half(x: f32) -> u16 {
    if x <= 0.0 {
        return 0;
    }
    let bits = x.to_bits();
    let exp = ((bits >> 23) & 0xFF) as i32 - 127 + 15;
    if exp <= 0 {
        return 0;
    }
    ((exp as u32) << 10 | (bits >> 13 & 0x3FF)) as u16
}

/// The panel in sRGB.
#[derive(Clone, Debug, PartialEq)]
struct Image {
    width: u32,
    height: u32,
    scale: f32,
    pixels: Vec<Rgb>,
}

impl Image {
    fn new(width: u32, height: u32, scale: f32) -> Image {
        Image {
            width,
            height,
            scale,
            pixels: vec![colour::BACK; (width * height) as usize],
        }
    }

    /// `c` over pixel (`x`, `y`) with coverage `a` (0 to 255).
    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: u8) {
        if x < 0 || y < 0 || x as u32 >= self.width || y as u32 >= self.height || a == 0 {
            return;
        }
        let p = &mut self.pixels[(y as u32 * self.width + x as u32) as usize];
        for k in 0..3 {
            let (from, to) = (u32::from(p[k]), u32::from(c[k]));
            p[k] = ((from * (255 - u32::from(a)) + to * u32::from(a) + 127) / 255) as u8;
        }
    }

    fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
        for yy in y..y + h {
            for xx in x..x + w {
                self.blend(xx, yy, c, 255);
            }
        }
    }

    fn outline(&mut self, x: i32, y: i32, w: i32, h: i32, c: Rgb) {
        self.rect(x, y, w, 1, c);
        self.rect(x, y + h - 1, w, 1, c);
        self.rect(x, y, 1, h, c);
        self.rect(x + w - 1, y, 1, h, c);
    }

    /// A line from (`x0`, `y0`) to (`x1`, `y1`), `t` pixels thick.
    fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32), t: i32, c: Rgb) {
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).ceil().max(1.0) as i32;
        for k in 0..=steps {
            let f = k as f32 / steps as f32;
            let (x, y) = (x0 + (x1 - x0) * f, y0 + (y1 - y0) * f);
            self.rect(x.round() as i32 - t / 2, y.round() as i32 - t / 2, t, t, c);
        }
    }

    /// A dashed horizontal line.
    fn dashes(&mut self, x: i32, y: i32, w: i32, dash: i32, c: Rgb) {
        let mut at = 0;
        while at < w {
            self.rect(x + at, y, dash.min(w - at), 1, c);
            at += dash * 2;
        }
    }

    /// `mask` in `c` with its top left at (`x`, `y`).
    fn stamp(&mut self, mask: &Mask, x: i32, y: i32, c: Rgb) {
        for yy in 0..mask.height {
            for xx in 0..mask.width {
                let a = mask.coverage[(yy * mask.width + xx) as usize];
                self.blend(x + xx as i32, y + yy as i32, c, a);
            }
        }
    }

    /// The image's rows in `pixel`'s format, `pitch` bytes apart.
    fn write(&self, pixel: Pixel, pitch: usize, out: &mut [u8]) {
        let n = pixel.bytes() as usize;
        let mut last = None;
        for (y, row) in self.pixels.chunks(self.width as usize).enumerate() {
            let line = &mut out[y * pitch..y * pitch + row.len() * n];
            for (px, c) in line.chunks_mut(n).zip(row) {
                let encoded = match last {
                    Some((was, e)) if was == *c => e,
                    _ => pixel.encode(*c),
                };
                last = Some((*c, encoded));
                px.copy_from_slice(&encoded[..n]);
            }
        }
    }
}

/// A string's coverage, 0 to 255 per pixel, as it is drawn.
#[derive(Clone, Debug, PartialEq)]
struct Mask {
    width: u32,
    height: u32,
    coverage: Vec<u8>,
}

/// How text is drawn: Consolas through GDI, or our 5x7 font where GDI
/// makes no device context.
enum Fonts {
    Gdi(gdi::Text),
    Bitmap,
}

impl Fonts {
    fn new() -> Fonts {
        gdi::Text::new().map_or(Fonts::Bitmap, Fonts::Gdi)
    }

    /// `text` at `size` pixels high (the line's height).
    fn mask(&self, text: &str, size: u32, bold: bool) -> Mask {
        match self {
            Fonts::Gdi(g) => g
                .mask(text, size, bold)
                .unwrap_or_else(|| bitmap_mask(text, size, bold)),
            Fonts::Bitmap => bitmap_mask(text, size, bold),
        }
    }
}

/// `text` in our 5x7 font (upper case), scaled to about `size` pixels.
fn bitmap_mask(text: &str, size: u32, bold: bool) -> Mask {
    let s = (size / 9).max(1);
    let chars: Vec<char> = text.chars().collect();
    let width = (chars.len() as u32 * 6 * s).max(1);
    let height = 9 * s;
    let mut coverage = vec![0u8; (width * height) as usize];
    for (k, ch) in chars.iter().enumerate() {
        for (row, bits) in glyph(ch.to_ascii_uppercase()).iter().enumerate() {
            for col in 0..5u32 {
                let on =
                    bits >> (4 - col) & 1 != 0 || bold && col > 0 && bits >> (5 - col) & 1 != 0;
                if !on {
                    continue;
                }
                for dy in 0..s {
                    for dx in 0..s {
                        let (x, y) = (k as u32 * 6 * s + col * s + dx, (row as u32 + 1) * s + dy);
                        coverage[(y * width + x) as usize] = 255;
                    }
                }
            }
        }
    }
    Mask {
        width,
        height,
        coverage,
    }
}

/// Text through GDI: Consolas, antialiased in grey (no colour fringes, which
/// the panel's own colours would not match).
mod gdi {
    use windows::Win32::Foundation::{COLORREF, SIZE};
    use windows::Win32::Graphics::Gdi::*;
    use windows::core::w;

    pub(super) struct Text {
        dc: HDC,
    }

    impl Drop for Text {
        fn drop(&mut self) {
            // SAFETY: our own device context.
            unsafe {
                let _ = DeleteDC(self.dc);
            }
        }
    }

    impl Text {
        pub(super) fn new() -> Option<Text> {
            // SAFETY: a memory device context of our own.
            let dc = unsafe { CreateCompatibleDC(None) };
            (!dc.is_invalid()).then_some(Text { dc })
        }

        pub(super) fn mask(&self, text: &str, size: u32, bold: bool) -> Option<super::Mask> {
            let wide: Vec<u16> = text.encode_utf16().collect();
            // SAFETY: GDI objects of ours, selected into our context while
            // drawing and deleted after; the section's bits are read after a
            // flush, while it lives.
            unsafe {
                let font = CreateFontW(
                    -(size as i32),
                    0,
                    0,
                    0,
                    if bold {
                        FW_BOLD.0 as i32
                    } else {
                        FW_NORMAL.0 as i32
                    },
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_TT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    ANTIALIASED_QUALITY,
                    (FIXED_PITCH.0 | FF_MODERN.0) as u32,
                    w!("Consolas"),
                );
                if font.is_invalid() {
                    return None;
                }
                let old_font = SelectObject(self.dc, font.into());
                let mut extent = SIZE::default();
                let measured = GetTextExtentPoint32W(self.dc, &wide, &mut extent).as_bool();
                let (width, height) = (extent.cx.max(1) as u32, extent.cy.max(1) as u32);
                let mut result = None;
                if measured {
                    let info = BITMAPINFO {
                        bmiHeader: BITMAPINFOHEADER {
                            biSize: size_of::<BITMAPINFOHEADER>() as u32,
                            biWidth: width as i32,
                            biHeight: -(height as i32),
                            biPlanes: 1,
                            biBitCount: 32,
                            biCompression: BI_RGB.0,
                            ..Default::default()
                        },
                        ..Default::default()
                    };
                    let mut bits = std::ptr::null_mut();
                    if let Ok(bitmap) =
                        CreateDIBSection(Some(self.dc), &info, DIB_RGB_COLORS, &mut bits, None, 0)
                        && !bits.is_null()
                    {
                        let old_bitmap = SelectObject(self.dc, bitmap.into());
                        std::ptr::write_bytes(bits.cast::<u8>(), 0, (width * height * 4) as usize);
                        SetTextColor(self.dc, COLORREF(0x00FF_FFFF));
                        SetBkMode(self.dc, TRANSPARENT);
                        let _ = TextOutW(self.dc, 0, 0, &wide);
                        let _ = GdiFlush();
                        let pixels = std::slice::from_raw_parts(
                            bits.cast::<[u8; 4]>(),
                            (width * height) as usize,
                        );
                        result = Some(super::Mask {
                            width,
                            height,
                            coverage: pixels.iter().map(|p| p[1]).collect(),
                        });
                        SelectObject(self.dc, old_bitmap);
                        let _ = DeleteObject(bitmap.into());
                    }
                }
                SelectObject(self.dc, old_font);
                let _ = DeleteObject(font.into());
                result
            }
        }
    }
}

/// Text alignment at a point.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Right,
    Centre,
}

/// Draws the panel at `scale` (pixels per pixel at 1080 lines).
struct Painter<'a> {
    image: Image,
    fonts: &'a Fonts,
    scale: f32,
}

impl Painter<'_> {
    /// `v` pixels at scale 1, scaled.
    fn px(&self, v: f32) -> i32 {
        (v * self.scale).round() as i32
    }

    /// `text` at (`x`, `y`) (its top), `size` pixels high at scale 1;
    /// returns its width.
    fn text(
        &mut self,
        x: i32,
        y: i32,
        text: &str,
        (size, bold): (f32, bool),
        c: Rgb,
        align: Align,
    ) -> i32 {
        let mask = self.fonts.mask(text, self.px(size).max(6) as u32, bold);
        let x = match align {
            Align::Left => x,
            Align::Right => x - mask.width as i32,
            Align::Centre => x - mask.width as i32 / 2,
        };
        self.image.stamp(&mask, x, y, c);
        mask.width as i32
    }

    /// A section's header and the rule under it; returns the y below.
    fn header(&mut self, x: i32, y: i32, w: i32, title: &str) -> i32 {
        self.text(x, y, title, (12.0, true), colour::HEADER, Align::Left);
        let below = y + self.px(16.0);
        self.image.rect(x, below, w, 1, colour::BORDER);
        below + self.px(6.0)
    }

    /// A label and its value on one row, the value right-aligned at `right`.
    fn row(&mut self, x: i32, right: i32, y: i32, label: &str, value: &str, c: Rgb) -> i32 {
        self.text(x, y, label, (14.0, false), colour::LABEL, Align::Left);
        self.text(right, y, value, (14.0, false), c, Align::Right);
        y + self.px(19.0)
    }
}

/// The panel's width at scale 1.
const WIDTH: f32 = 470.0;
/// Frame times at and past this fill the frame-time graph.
const GRAPH_MS: f32 = 66.7;

fn draw_panel(fonts: &Fonts, scale: f32, f: &Facts) -> Image {
    // Drawn on a tall canvas, then cut to what was used.
    let width = (WIDTH * scale).round() as u32;
    let mut p = Painter {
        image: Image::new(width, (1200.0 * scale) as u32, scale),
        fonts,
        scale,
    };
    let pad = p.px(12.0);
    let (left, right) = (pad, width as i32 - pad);
    let inner = right - left;
    let mut y = pad;

    // Title.
    p.text(
        left,
        y,
        "VIctory overlay",
        (15.0, true),
        colour::VALUE,
        Align::Left,
    );
    p.text(
        right,
        y + p.px(2.0),
        "Ctrl+F10 hides",
        (12.0, false),
        colour::LABEL,
        Align::Right,
    );
    y += p.px(26.0);

    // Performance.
    y = p.header(left, y, inner, "PERFORMANCE");
    let (mean, fps, low) = frame_stats(f.times);
    let big = p.text(
        left,
        y - p.px(2.0),
        &format!("{fps:.1}"),
        (30.0, true),
        rate_colour(fps),
        Align::Left,
    );
    p.text(
        left + big + p.px(6.0),
        y + p.px(12.0),
        "FPS",
        (13.0, true),
        colour::LABEL,
        Align::Left,
    );
    p.text(
        right,
        y,
        &format!("{mean:.1} ms per frame"),
        (14.0, false),
        colour::VALUE,
        Align::Right,
    );
    p.text(
        right,
        y + p.px(18.0),
        &format!("1% low  {low:.1} FPS"),
        (14.0, false),
        rate_colour(low),
        Align::Right,
    );
    y += p.px(42.0);
    y = frame_time_graph(&mut p, left, y, inner, f.times);
    y += p.px(6.0);
    let s = f.second;
    y = p.row(
        left,
        right,
        y,
        "Draws per frame",
        &format!("{:.0}", s.draws),
        colour::VALUE,
    );
    y = p.row(
        left,
        right,
        y,
        "Preparing submissions",
        &format!("{:.1} ms / frame", s.prepare_ms),
        colour::VALUE,
    );
    let gpu = match recent(f.gpu, 60) {
        Some((ms, _)) => format!("{ms:.1} ms / frame ({:.0} %)", ms / mean.max(0.001) * 100.0),
        None => "...".to_owned(),
    };
    y = p.row(left, right, y, "GPU busy (timestamps)", &gpu, colour::GPU);
    let latency = match recent(f.latency, 60) {
        Some((ms, max)) => format!("{ms:.1} ms (slowest {max:.1})"),
        None => "no input read".to_owned(),
    };
    y = p.row(left, right, y, "Input to present", &latency, colour::VALUE);
    y = p.row(
        left,
        right,
        y,
        "Waiting for the GPU",
        &format!("{:.1} ms / frame", s.gpu_wait_ms),
        colour::VALUE,
    );
    y += p.px(8.0);

    // System.
    y = p.header(left, y, inner, "SYSTEM");
    let sys = f.system;
    let cpu = sys.cpu.map_or("...".to_owned(), |c| format!("{c:.1} %"));
    y = p.row(left, right, y, "CPU (this process)", &cpu, colour::CPU);
    y = p.row(
        left,
        right,
        y,
        "Memory (this process)",
        &format!(
            "{} GB private, {} GB in use",
            gb(sys.private),
            gb(sys.working)
        ),
        colour::VALUE,
    );
    y = p.row(
        left,
        right,
        y,
        "System memory",
        &format!("{} of {} GB", gb(sys.ram.0), gb(sys.ram.1)),
        colour::VALUE,
    );
    let vram = match sys.vram {
        Some((used, budget, shared)) => format!(
            "{} of {} GB ({:.0} %), shared {} GB",
            gb(used),
            gb(budget),
            used as f64 / budget.max(1) as f64 * 100.0,
            gb(shared)
        ),
        None => "...".to_owned(),
    };
    y = p.row(left, right, y, "Video memory", &vram, colour::VRAM);
    y += p.px(4.0);
    y = history_graph(&mut p, left, y, inner, f.history);
    y += p.px(8.0);

    // Controller.
    y = p.header(left, y, inner, "CONTROLLER");
    y = match &f.pad {
        Some(pad) => controller(&mut p, left, y, inner, pad),
        None => {
            p.text(
                left,
                y,
                "No controller",
                (14.0, false),
                colour::LABEL,
                Align::Left,
            );
            y + p.px(20.0)
        }
    };
    y += p.px(8.0);

    y += pad;

    // Cut to size and frame it.
    let height = (y.max(1) as u32).min(p.image.height);
    let mut image = p.image;
    image.pixels.truncate((width * height) as usize);
    image.height = height;
    image.outline(0, 0, width as i32, height as i32, colour::BORDER);
    image
}

/// The frame-time graph, labelled: milliseconds up (gridlines at 60, 30
/// and 20 frames a second), frames back to the right edge (now). Returns
/// the y below it.
fn frame_time_graph(p: &mut Painter, x: i32, y: i32, w: i32, times: &VecDeque<f32>) -> i32 {
    p.text(
        x,
        y,
        "Frame time (ms), last 240 frames",
        (13.0, false),
        colour::LABEL,
        Align::Left,
    );
    let axis = p.px(38.0);
    let (left, top) = (x + axis, y + p.px(20.0));
    let (gw, gh) = (w - axis - p.px(52.0), p.px(96.0));
    let bottom = top + gh;
    let to_y = |ms: f32| bottom as f32 - (ms / GRAPH_MS).min(1.0) * gh as f32;
    // Gridlines, labelled with their time (left) and rate (right).
    for (ms, label) in [(16.7, "60 FPS"), (33.3, "30 FPS"), (50.0, "20 FPS")] {
        let gy = to_y(ms).round() as i32;
        let c = if ms == 33.3 {
            colour::AXIS
        } else {
            colour::GRID
        };
        p.image.dashes(left, gy, gw, p.px(4.0).max(2), c);
        p.text(
            left - p.px(5.0),
            gy - p.px(7.0),
            &format!("{ms:.1}"),
            (11.0, false),
            colour::LABEL,
            Align::Right,
        );
        p.text(
            left + gw + p.px(5.0),
            gy - p.px(7.0),
            label,
            (11.0, false),
            colour::LABEL,
            Align::Left,
        );
    }
    p.text(
        left - p.px(5.0),
        bottom - p.px(7.0),
        "0",
        (11.0, false),
        colour::LABEL,
        Align::Right,
    );
    p.text(
        left - p.px(5.0),
        top - p.px(7.0),
        &format!("{GRAPH_MS:.0}+"),
        (11.0, false),
        colour::LABEL,
        Align::Right,
    );
    // Bars, newest on the right.
    // Whole-pixel columns, each frame from its slot's start to the next's.
    let column = |slot: usize| left + (slot as i64 * i64::from(gw) / HISTORY as i64) as i32;
    let shown = times.len().min(HISTORY);
    for (k, &ms) in times.iter().skip(times.len() - shown).enumerate() {
        let slot = HISTORY - shown + k;
        let (x0, x1) = (column(slot), column(slot + 1).max(column(slot) + 1));
        let top_y = to_y(ms);
        let c = if ms <= 34.0 {
            colour::GOOD
        } else if ms <= 50.0 {
            colour::WARN
        } else {
            colour::BAD
        };
        let top_y = top_y.round() as i32;
        p.image.rect(x0, top_y, x1 - x0, bottom - top_y, c);
    }
    // Axes.
    p.image.rect(left, top, 1, gh + 1, colour::AXIS);
    p.image.rect(left, bottom, gw + 1, 1, colour::AXIS);
    for (frac, label) in [(0.0, "-240"), (0.5, "-120"), (1.0, "now")] {
        let tx = left + (gw as f32 * frac) as i32;
        p.image.rect(tx, bottom, 1, p.px(4.0), colour::AXIS);
        let align = if frac == 0.0 {
            Align::Left
        } else if frac == 1.0 {
            Align::Right
        } else {
            Align::Centre
        };
        p.text(
            tx,
            bottom + p.px(5.0),
            label,
            (11.0, false),
            colour::LABEL,
            align,
        );
    }
    p.text(
        left + gw / 2,
        bottom + p.px(18.0),
        "frames",
        (11.0, false),
        colour::LABEL,
        Align::Centre,
    );
    bottom + p.px(34.0)
}

/// CPU use and video memory, percent, over the last minute. Returns the y
/// below it.
fn history_graph(
    p: &mut Painter,
    x: i32,
    y: i32,
    w: i32,
    history: &VecDeque<[Option<f32>; 3]>,
) -> i32 {
    p.text(
        x,
        y,
        "Last 60 s (%)",
        (13.0, false),
        colour::LABEL,
        Align::Left,
    );
    // Legend.
    let mut lx = x + w;
    for (name, c) in [
        ("Video memory", colour::VRAM),
        ("GPU busy", colour::GPU),
        ("CPU", colour::CPU),
    ] {
        let tw = p.text(lx, y, name, (12.0, false), colour::LABEL, Align::Right);
        lx -= tw + p.px(6.0);
        p.image
            .rect(lx - p.px(10.0), y + p.px(6.0), p.px(10.0), p.px(3.0), c);
        lx -= p.px(20.0);
    }
    let axis = p.px(38.0);
    let (left, top) = (x + axis, y + p.px(20.0));
    let (gw, gh) = (w - axis - p.px(52.0), p.px(64.0));
    let bottom = top + gh;
    for pct in [50.0f32, 100.0] {
        let gy = bottom - (pct / 100.0 * gh as f32).round() as i32;
        p.image.dashes(left, gy, gw, p.px(4.0).max(2), colour::GRID);
        p.text(
            left - p.px(5.0),
            gy - p.px(7.0),
            &format!("{pct:.0}"),
            (11.0, false),
            colour::LABEL,
            Align::Right,
        );
    }
    p.text(
        left - p.px(5.0),
        bottom - p.px(7.0),
        "0",
        (11.0, false),
        colour::LABEL,
        Align::Right,
    );
    let step = gw as f32 / (SECONDS - 1) as f32;
    let start = SECONDS - history.len().min(SECONDS);
    for (series, c) in [(0, colour::CPU), (1, colour::GPU), (2, colour::VRAM)] {
        let mut last: Option<(f32, f32)> = None;
        for (k, sample) in history.iter().enumerate() {
            let Some(v) = sample[series] else {
                last = None;
                continue;
            };
            let pt = (
                left as f32 + (start + k) as f32 * step,
                bottom as f32 - v.clamp(0.0, 100.0) / 100.0 * gh as f32,
            );
            if let Some(from) = last {
                p.image.line(from, pt, p.px(2.0).max(1), c);
            }
            last = Some(pt);
        }
    }
    p.image.rect(left, top, 1, gh + 1, colour::AXIS);
    p.image.rect(left, bottom, gw + 1, 1, colour::AXIS);
    for (frac, label) in [(0.0, "-60 s"), (0.5, "-30 s"), (1.0, "now")] {
        let tx = left + (gw as f32 * frac) as i32;
        p.image.rect(tx, bottom, 1, p.px(4.0), colour::AXIS);
        let align = if frac == 0.0 {
            Align::Left
        } else if frac == 1.0 {
            Align::Right
        } else {
            Align::Centre
        };
        p.text(
            tx,
            bottom + p.px(5.0),
            label,
            (11.0, false),
            colour::LABEL,
            align,
        );
    }
    bottom + p.px(22.0)
}

/// Both sticks (a box each, the dot where the stick is), the triggers as
/// bars, and the buttons lit while held. Returns the y below.
fn controller(p: &mut Painter, x: i32, y: i32, _w: i32, pad: &Pad) -> i32 {
    let size = p.px(56.0);
    let mut cx = x;
    for (name, [sx, sy]) in [("Left stick", pad.left), ("Right stick", pad.right)] {
        p.image.rect(cx, y, size, size, colour::CHIP);
        p.image.outline(cx, y, size, size, colour::BORDER);
        p.image
            .rect(cx + size / 2, y + 1, 1, size - 2, colour::GRID);
        p.image
            .rect(cx + 1, y + size / 2, size - 2, 1, colour::GRID);
        let (dx, dy) = (
            cx as f32 + (sx.clamp(-1.0, 1.0) + 1.0) / 2.0 * (size - 1) as f32,
            y as f32 + (1.0 - (sy.clamp(-1.0, 1.0) + 1.0) / 2.0) * (size - 1) as f32,
        );
        let d = p.px(8.0).max(3);
        p.image.rect(
            dx.round() as i32 - d / 2,
            dy.round() as i32 - d / 2,
            d,
            d,
            colour::LIT,
        );
        p.text(
            cx + size / 2,
            y + size + p.px(3.0),
            name,
            (11.0, false),
            colour::LABEL,
            Align::Centre,
        );
        p.text(
            cx + size / 2,
            y + size + p.px(16.0),
            &format!("{sx:+.2} {sy:+.2}"),
            (11.0, false),
            colour::VALUE,
            Align::Centre,
        );
        cx += size + p.px(30.0);
    }
    for (name, v) in [("LT", pad.triggers[0]), ("RT", pad.triggers[1])] {
        let bw = p.px(14.0);
        p.image.rect(cx, y, bw, size, colour::CHIP);
        p.image.outline(cx, y, bw, size, colour::BORDER);
        let fill = (v.clamp(0.0, 1.0) * (size - 2) as f32).round() as i32;
        p.image
            .rect(cx + 1, y + size - 1 - fill, bw - 2, fill, colour::LIT);
        p.text(
            cx + bw / 2,
            y + size + p.px(3.0),
            name,
            (11.0, false),
            colour::LABEL,
            Align::Centre,
        );
        p.text(
            cx + bw / 2,
            y + size + p.px(16.0),
            &format!("{v:.2}"),
            (11.0, false),
            colour::VALUE,
            Align::Centre,
        );
        cx += bw + p.px(18.0);
    }
    // Buttons: GameInput's bit order.
    const NAMES: [&str; 14] = [
        "Menu", "View", "A", "B", "X", "Y", "Up", "Down", "Left", "Right", "LB", "RB", "LS", "RS",
    ];
    let (cw, ch) = (p.px(44.0), p.px(17.0));
    let gap = p.px(4.0);
    for (k, name) in NAMES.iter().enumerate() {
        let (col, row) = ((k % 4) as i32, (k / 4) as i32);
        let (bx, by) = (cx + col * (cw + gap), y + row * (ch + gap));
        let held = pad.buttons >> k & 1 != 0;
        p.image.rect(
            bx,
            by,
            cw,
            ch,
            if held { colour::LIT } else { colour::CHIP },
        );
        p.image.outline(bx, by, cw, ch, colour::BORDER);
        p.text(
            bx + cw / 2,
            by + p.px(2.0),
            name,
            (11.0, held),
            if held { colour::BACK } else { colour::LABEL },
            Align::Centre,
        );
    }
    y + size + p.px(34.0)
}

/// A character's 5x7 glyph, a row per byte (bit 4 leftmost); `?` for
/// those it has none of.
fn glyph(c: char) -> [u8; 7] {
    match c {
        ' ' => [0; 7],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1F, 0x02, 0x04, 0x02, 0x01, 0x11, 0x0E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        'A' => [0x0E, 0x11, 0x11, 0x11, 0x1F, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1C, 0x12, 0x11, 0x11, 0x11, 0x12, 0x1C],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x02, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x11, 0x19, 0x15, 0x13, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x15, 0x0A],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x11, 0x0A, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '.' => [0, 0, 0, 0, 0, 0x0C, 0x0C],
        ',' => [0, 0, 0, 0, 0x0C, 0x04, 0x08],
        ':' => [0, 0x0C, 0x0C, 0, 0x0C, 0x0C, 0],
        '%' => [0x18, 0x19, 0x02, 0x04, 0x08, 0x13, 0x03],
        '/' => [0, 0x01, 0x02, 0x04, 0x08, 0x10, 0],
        '-' => [0, 0, 0, 0x1F, 0, 0, 0],
        '+' => [0, 0x04, 0x04, 0x1F, 0x04, 0x04, 0],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '[' => [0x0E, 0x08, 0x08, 0x08, 0x08, 0x08, 0x0E],
        ']' => [0x0E, 0x02, 0x02, 0x02, 0x02, 0x02, 0x0E],
        '=' => [0, 0, 0x1F, 0, 0x1F, 0, 0],
        '<' => [0x02, 0x04, 0x08, 0x10, 0x08, 0x04, 0x02],
        '>' => [0x08, 0x04, 0x02, 0x01, 0x02, 0x04, 0x08],
        '!' => [0x04, 0x04, 0x04, 0x04, 0, 0, 0x04],
        '_' => [0, 0, 0, 0, 0, 0, 0x1F],
        '|' => [0x04; 7],
        '*' => [0, 0x04, 0x15, 0x0E, 0x15, 0x04, 0],
        _ => [0x0E, 0x11, 0x01, 0x02, 0x04, 0, 0x04],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ctrl+F10 only with Ctrl held.
    #[test]
    fn the_toggle_needs_ctrl() {
        let with = |keys: &'static [u8]| toggle_down(move |vk| keys.contains(&(vk as u8)));
        assert!(!with(&[0x79]));
        assert!(!with(&[0x11]));
        assert!(with(&[0x11, 0x79]));
        assert!(with(&[0x11, 0x79, b'1']));
    }

    /// The key thread starts once and stays pointed at the latest window;
    /// keys are then not read on the presenting thread.
    #[test]
    fn keys_are_read_on_a_thread_of_their_own() {
        assert!(watch_keys(HWND(std::ptr::null_mut())));
        assert!(watch_keys(HWND(8 as *mut _)));
        assert_eq!(WINDOW.load(Relaxed), 8);
        WINDOW.store(0, Relaxed);
    }

    /// Sample facts: a minute of history, frame times around 30 fps with
    /// some slow ones, and a pad with A and RB held.
    fn sample_facts<'a>(
        times: &'a VecDeque<f32>,
        history: &'a VecDeque<[Option<f32>; 3]>,
        series: &'a (VecDeque<Option<f32>>, VecDeque<Option<f32>>),
    ) -> Facts<'a> {
        Facts {
            times,
            history,
            gpu: &series.0,
            latency: &series.1,
            second: Second {
                draws: 2513.0,
                prepare_ms: 12.3,
                gpu_wait_ms: 18.2,
            },
            system: System {
                cpu: Some(37.5),
                private: 14 << 30,
                working: 11 << 30,
                ram: (20 << 30, 64 << 30),
                vram: Some((8 << 30, 15 << 30, 1 << 29)),
            },
            pad: Some(Pad {
                buttons: 0x4 | 0x800,
                triggers: [0.0, 1.0],
                left: [0.0, 0.98],
                right: [-0.25, 0.0],
            }),
        }
    }

    /// GPU busy time and input-to-present time per frame.
    fn sample_frames() -> (VecDeque<Option<f32>>, VecDeque<Option<f32>>) {
        (
            (0..HISTORY).map(|k| Some(24.0 + (k % 5) as f32)).collect(),
            (0..HISTORY)
                .map(|k| (k % 3 != 0).then_some(48.0 + (k % 9) as f32))
                .collect(),
        )
    }

    fn sample_series() -> (VecDeque<f32>, VecDeque<[Option<f32>; 3]>) {
        let times = (0..HISTORY)
            .map(|k| 31.0 + (k % 7) as f32 + if k % 37 == 0 { 30.0 } else { 0.0 })
            .collect();
        let history = (0..SECONDS)
            .map(|k| {
                [
                    Some(30.0 + (k % 10) as f32 * 2.0),
                    Some(70.0 + (k % 6) as f32),
                    Some(50.0 + k as f32 / 6.0),
                ]
            })
            .collect();
        (times, history)
    }

    /// The mean, its rate, and the 1% low (the slowest 1% of frames).
    #[test]
    fn frame_stats_take_the_mean_and_the_slowest_frames() {
        let mut times: VecDeque<f32> = std::iter::repeat_n(20.0, 199).collect();
        times.extend([100.0, 100.0]);
        let (mean, fps, low) = frame_stats(&times);
        assert!((mean - (199.0 * 20.0 + 200.0) / 201.0).abs() < 1e-3);
        assert!((fps - 1000.0 / mean).abs() < 1e-3);
        assert_eq!(low, 10.0, "the two slowest frames");
        assert_eq!(frame_stats(&VecDeque::new()), (0.0, 0.0, 0.0));
    }

    /// The panel fits the back buffers it is drawn for (720, 1080 and 2160
    /// lines), in either font, with text, both graphs and the controller.
    #[test]
    fn the_panel_fits_the_frames_it_is_drawn_for() {
        let (times, history) = sample_series();
        let frames = sample_frames();
        for fonts in [Fonts::new(), Fonts::Bitmap] {
            for height in [720, 1080, 2160] {
                let scale = scale_for(height);
                let image = draw_panel(&fonts, scale, &sample_facts(&times, &history, &frames));
                let margin = (MARGIN as f32 * scale).round() as u32;
                assert!(
                    image.height + 2 * margin <= height,
                    "{height} lines: {} high",
                    image.height
                );
                assert!(image.width + 2 * margin <= height * 16 / 9);
                assert_eq!(image.pixels.len(), (image.width * image.height) as usize);
                for c in [
                    colour::VALUE,
                    colour::GOOD,
                    colour::BAD,
                    colour::CPU,
                    colour::GPU,
                    colour::VRAM,
                    colour::LIT,
                ] {
                    assert!(image.pixels.contains(&c), "{c:?} drawn at {height} lines");
                }
            }
        }
    }

    /// GDI draws antialiased text (partial coverage at the edges), wider
    /// for longer text; where it is missing the 5x7 font stands in.
    #[test]
    fn text_is_antialiased_where_gdi_draws_it() {
        let fonts = Fonts::new();
        let short = fonts.mask("Ag", 16, false);
        let long = fonts.mask("Agagag", 16, false);
        assert!(long.width > short.width);
        if matches!(fonts, Fonts::Gdi(_)) {
            assert!(
                short.coverage.iter().any(|&a| a > 0 && a < 255),
                "antialiased"
            );
        }
        let fallback = bitmap_mask("Ag", 18, false);
        assert_eq!((fallback.width, fallback.height), (24, 18));
    }

    /// The panel, shown, lands in the back buffer's top left corner on the
    /// host GPU: its background at the margin, its text in the text colour,
    /// the frame around it untouched.
    #[test]
    fn a_shown_panel_is_copied_into_the_back_buffer() {
        let Some(host) = crate::host::host() else {
            return; // no host GPU here
        };
        let (width, height) = (1920u32, 1080u32);
        let make = |heap: D3D12_HEAP_TYPE, desc: &D3D12_RESOURCE_DESC, state| {
            let mut r: Option<ID3D12Resource> = None;
            // SAFETY: plain resource creation.
            unsafe {
                host.device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: heap,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        desc,
                        state,
                        None,
                        &mut r,
                    )
                    .unwrap();
            }
            r.unwrap()
        };
        let back = make(
            D3D12_HEAP_TYPE_DEFAULT,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(width),
                Height: height,
                DepthOrArraySize: 1,
                MipLevels: 1,
                Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let pitch = width * 4;
        let readback = make(
            D3D12_HEAP_TYPE_READBACK,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: u64::from(pitch * height),
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_COPY_DEST,
        );
        let list = host.new_list().unwrap();
        let mut overlay = Overlay {
            enabled: true,
            ..Overlay::default()
        };
        VISIBLE.store(true, Relaxed);
        overlay.record(
            host,
            &list.list,
            &back,
            (width, height, DXGI_FORMAT_R8G8B8A8_UNORM),
            HWND::default(),
            D3D12_RESOURCE_STATE_COPY_DEST,
            true,
        );
        // SAFETY: an open list and live resources; footprint in the buffer.
        unsafe {
            list.list.ResourceBarrier(&[crate::host::transition(
                &back,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            let dst = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&readback),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
                            Width: width,
                            Height: height,
                            Depth: 1,
                            RowPitch: pitch,
                        },
                    },
                },
            };
            let src = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&back),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            list.list.CopyTextureRegion(&dst, 0, 0, 0, &src, None);
        }
        list.close();
        GPU_NS.store(0, Relaxed);
        host.execute(&[&list]);
        VISIBLE.store(false, Relaxed);
        // The submission ran between two timestamps while the panel showed.
        assert!(GPU_NS.swap(0, Relaxed) > 0, "GPU busy time measured");
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the copy finished (execute waits); the buffer holds the rows.
        let pixels = unsafe {
            readback.Map(0, None, Some(&mut mapped)).unwrap();
            std::slice::from_raw_parts(mapped.cast::<[u8; 4]>(), (width * height) as usize).to_vec()
        };
        let at = |x: u32, y: u32| pixels[(y * width + x) as usize];
        // Its border at the margin, a lit pixel of text inside, nothing
        // outside it.
        let margin = (MARGIN as f32 * scale_for(height)).round() as u32;
        let border = colour::BORDER;
        assert_eq!(
            at(margin, margin),
            [border[0], border[1], border[2], 255],
            "the panel's corner"
        );
        assert_eq!(at(0, 0), [0; 4], "outside the panel");
        let inside =
            (margin + 4..margin + 60).flat_map(|y| (margin + 4..margin + 200).map(move |x| (x, y)));
        assert!(
            inside.into_iter().any(|(x, y)| at(x, y)[0] > 200),
            "text in the panel"
        );
    }

    /// Writes sample panels (at 1080 and 1440 lines) to the BMP files
    /// `VICTORY_OVERLAY_PREVIEW` names with `-1080.bmp` and `-1440.bmp`
    /// added (`cargo test -p victory-gpu overlay -- --ignored`), to look at.
    #[test]
    #[ignore]
    fn preview() {
        let Some(path) = std::env::var_os("VICTORY_OVERLAY_PREVIEW") else {
            return;
        };
        let (times, history) = sample_series();
        let frames = sample_frames();
        let fonts = Fonts::new();
        for height in [1080, 1440] {
            let image = draw_panel(
                &fonts,
                scale_for(height),
                &sample_facts(&times, &history, &frames),
            );
            let (w, h) = (image.width, image.height);
            let mut rows = vec![0u8; (w * h * 4) as usize];
            image.write(Pixel::Bgra8, (w * 4) as usize, &mut rows);
            // A 32-bit BMP, rows bottom up.
            let mut bmp = Vec::new();
            let size = 54 + rows.len() as u32;
            bmp.extend(b"BM");
            for v in [size, 0, 54, 40] {
                bmp.extend(v.to_le_bytes());
            }
            bmp.extend((w as i32).to_le_bytes());
            bmp.extend((h as i32).to_le_bytes());
            bmp.extend(1u16.to_le_bytes());
            bmp.extend(32u16.to_le_bytes());
            for v in [0u32, rows.len() as u32, 2835, 2835, 0, 0] {
                bmp.extend(v.to_le_bytes());
            }
            for row in rows.chunks((w * 4) as usize).rev() {
                bmp.extend(row);
            }
            let mut name = path.clone();
            name.push(format!("-{height}.bmp"));
            std::fs::write(name, bmp).unwrap();
        }
    }

    /// Each back buffer format gets the colours in its own encoding.
    #[test]
    fn colours_are_encoded_per_format() {
        assert_eq!(Pixel::Rgba8.encode([235, 10, 20])[..4], [235, 10, 20, 255]);
        assert_eq!(
            Pixel::Bgra8.encode([235, 10, 20])[..4],
            [20, 10, 235, 255],
            "blue, green, red"
        );
        let packed = u32::from_le_bytes(
            Pixel::Rgb10a2.encode([255, 0, 255])[..4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(packed, 1023 | 1023 << 20 | 3 << 30);
        assert_eq!(half(1.0), 0x3C00);
        assert_eq!(half(0.5), 0x3800);
        let image = Image {
            width: 2,
            height: 1,
            scale: 1.0,
            pixels: vec![colour::BACK, colour::VALUE],
        };
        let mut out = vec![0u8; 256];
        image.write(Pixel::Rgba16f, 256, &mut out);
        assert_eq!(
            out[8..16],
            Pixel::Rgba16f.encode(colour::VALUE),
            "the second pixel"
        );
    }

    /// GPU busy percent is over the last second of frames; the recent mean
    /// and slowest skip frames not measured.
    #[test]
    fn busy_and_latency_figures() {
        let times: VecDeque<f32> = std::iter::repeat_n(50.0, 40).collect();
        let gpu: VecDeque<Option<f32>> = (0..40).map(|k| (k >= 20).then_some(25.0)).collect();
        assert_eq!(
            busy_percent(&times, &gpu),
            Some(50.0),
            "the last 20 frames, half busy"
        );
        assert_eq!(busy_percent(&times, &VecDeque::from(vec![None; 40])), None);
        let latency: VecDeque<Option<f32>> = VecDeque::from(vec![Some(40.0), None, Some(60.0)]);
        assert_eq!(recent(&latency, 60), Some((50.0, 60.0)));
        assert_eq!(recent(&VecDeque::from(vec![None]), 60), None);
    }

    /// Blending covers by coverage: none keeps the pixel, full replaces it.
    #[test]
    fn coverage_blends_over_the_background() {
        let mut image = Image::new(3, 1, 1.0);
        image.blend(0, 0, [255, 255, 255], 0);
        image.blend(1, 0, [255, 255, 255], 255);
        image.blend(2, 0, [255, 255, 255], 128);
        image.blend(5, 5, [255, 255, 255], 255);
        assert_eq!(image.pixels[0], colour::BACK);
        assert_eq!(image.pixels[1], [255, 255, 255]);
        assert!(image.pixels[2][0] > colour::BACK[0] && image.pixels[2][0] < 255);
    }
}
