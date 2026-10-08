//! Native PipeWire audio output (Linux). cpal's ALSA backend runs through the pipewire-alsa
//! bridge, where the stream dies on the first underrun (`alsa::poll() returned POLLERR`) and
//! only ALSA hardware devices are listed, so Bluetooth sinks never appear. This backend talks
//! to PipeWire directly: the stream survives underruns and device moves, follows the system
//! default sink, and Settings ▸ Audio Hardware lists every PipeWire `Audio/Sink`.
//!
//! [`SystemOut`] picks the backend: PipeWire when its socket is reachable (Device Class empty
//! or "PipeWire"), otherwise [`CpalOut`]; "ALSA"/"JACK" force cpal.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use filmcraft_engine::settings::AudioHardwarePrefs;
use filmcraft_ui_egui::{AudioDevices, AudioOut};
use pipewire as pw;
use pw::spa;

use crate::audio::CpalOut;

/// The one host name this backend adds to Settings ▸ Audio Hardware ▸ Device Class.
const HOST_NAME: &str = "PipeWire";
/// A driver asking for more than this many frames per callback is treated as broken.
const MAX_FRAMES: usize = 1 << 20;

/// Whether a PipeWire daemon looks reachable without connecting to it.
pub fn available() -> bool {
    if std::env::var_os("PIPEWIRE_REMOTE").is_some() {
        return true;
    }
    std::env::var_os("XDG_RUNTIME_DIR").is_some_and(|d| std::path::Path::new(&d).join("pipewire-0").exists())
}

fn init_once() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(pw::init);
}

/// One output sink: (display name shown in the settings, `node.name` used as the stream target).
type Sink = (String, String);

pub struct PipeWireOut {
    worker: Option<Worker>,
    played: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    rate: u32,
    hw: AudioHardwarePrefs,
    document_rate: Option<u32>,
    /// Sinks seen by the last `devices()` call, to map the chosen display name to a node name.
    sinks: Arc<Mutex<Vec<Sink>>>,
}

struct Worker {
    quit: pw::channel::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

/// Everything the stream thread needs, resolved before it starts.
struct StreamConfig {
    rate: u32,
    channels: usize,
    buffer_size: u32,
    /// `node.name` of the chosen sink; empty follows the system default.
    target: String,
}

impl PipeWireOut {
    pub fn new() -> Self {
        Self {
            worker: None,
            played: Arc::new(AtomicU64::new(0)),
            failed: Arc::new(AtomicBool::new(false)),
            rate: 48_000,
            hw: AudioHardwarePrefs::default(),
            document_rate: None,
            sinks: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The rate playback mixes at: the settings' rate, or the sequence's when forced. PipeWire
    /// resamples to whatever the device runs at, so the asked-for rate is always granted.
    fn desired_rate(&self) -> u32 {
        let want = if self.hw.force_document_rate { self.document_rate.unwrap_or(self.hw.sample_rate) } else { self.hw.sample_rate };
        if want == 0 { 48_000 } else { want }
    }

    /// The `node.name` for the output chosen in the settings (empty = follow the default sink).
    fn target(&self) -> String {
        if self.hw.default_output.is_empty() {
            return String::new();
        }
        let find = |sinks: &[Sink]| sinks.iter().find(|(d, n)| *d == self.hw.default_output || *n == self.hw.default_output).map(|(_, n)| n.clone());
        let cached = self.sinks.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if let Some(n) = find(&cached) {
            return n;
        }
        find(&enumerate("Audio/Sink")).unwrap_or_default()
    }
}

impl AudioOut for PipeWireOut {
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
        self.stop();
        init_once();
        let config = StreamConfig { rate: self.desired_rate(), channels: 2, buffer_size: self.hw.buffer_size, target: self.target() };
        self.rate = config.rate;
        self.played.store(0, Ordering::SeqCst);
        self.failed.store(false, Ordering::SeqCst);
        let played = self.played.clone();
        let failed = self.failed.clone();
        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        let thread = std::thread::Builder::new()
            .name("filmcraft-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_stream(fill, &config, &played, &failed, quit_rx, &ready_tx) {
                    // Reported through `ready_tx` when start is still waiting, or stderr after.
                    let _ = ready_tx.send(Err(e));
                }
            })
            .map_err(|e| e.to_string())?;
        match ready_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(Ok(())) => {
                self.worker = Some(Worker { quit: quit_tx, thread });
                Ok(self.rate)
            }
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(e)
            }
            Err(_) => {
                let _ = quit_tx.send(());
                Err("the PipeWire daemon did not answer".into())
            }
        }
    }

    fn stop(&mut self) {
        if let Some(w) = self.worker.take() {
            let _ = w.quit.send(());
            let _ = w.thread.join();
        }
    }

    fn sample_rate(&self) -> u32 {
        self.rate
    }

    fn channels(&self) -> usize {
        2
    }

    fn played_frames(&self) -> Option<u64> {
        self.worker.as_ref().filter(|_| !self.failed.load(Ordering::SeqCst)).map(|_| self.played.load(Ordering::SeqCst))
    }

    fn devices(&self) -> AudioDevices {
        init_once();
        let sinks = enumerate("Audio/Sink");
        let sources = enumerate("Audio/Source");
        let outputs = sinks.iter().map(|(d, _)| d.clone()).collect();
        let inputs = sources.iter().map(|(d, _)| d.clone()).collect();
        *self.sinks.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = sinks;
        AudioDevices { hosts: vec![HOST_NAME.into()], inputs, outputs, output_channels: 2 }
    }

    fn configure(&mut self, hw: &AudioHardwarePrefs, document_rate: Option<u32>) {
        self.hw = hw.clone();
        self.document_rate = document_rate;
        self.rate = self.desired_rate();
    }
}

/// State owned by the stream's process callback.
struct Playing {
    fill: Box<dyn FnMut(&mut [f32], usize) + Send>,
    scratch: Vec<f32>,
    channels: usize,
    played: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
}

/// Build the stream and run its loop until `quit_rx` fires. Sends on `ready_tx` once the
/// stream is connected, so `start` can report connection errors synchronously.
fn run_stream(
    fill: Box<dyn FnMut(&mut [f32], usize) + Send>,
    config: &StreamConfig,
    played: &Arc<AtomicU64>,
    failed: &Arc<AtomicBool>,
    quit_rx: pw::channel::Receiver<()>,
    ready_tx: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
    let core = context.connect_rc(None).map_err(|e| e.to_string())?;
    let _quit = quit_rx.attach(mainloop.loop_(), {
        let mainloop = mainloop.clone();
        move |_| mainloop.quit()
    });

    let mut props = pw::properties::properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Playback",
        *pw::keys::MEDIA_ROLE => "Production",
        *pw::keys::APP_NAME => "FilmCraft",
        *pw::keys::NODE_NAME => "FilmCraft",
    };
    if config.buffer_size > 0 {
        props.insert(*pw::keys::NODE_LATENCY, format!("{}/{}", config.buffer_size, config.rate));
    }
    if !config.target.is_empty() {
        props.insert(*pw::keys::TARGET_OBJECT, config.target.clone());
    }

    let stream = pw::stream::StreamBox::new(&core, "FilmCraft Playback", props).map_err(|e| e.to_string())?;
    let state = Playing { fill, scratch: Vec::new(), channels: config.channels, played: played.clone(), failed: failed.clone() };
    let _listener = stream
        .add_local_listener_with_user_data(state)
        .process(|stream, st| {
            let Some(mut buffer) = stream.dequeue_buffer() else { return };
            // Fill only what this cycle asks for: the buffer itself is usually much larger,
            // and filling it whole drains the mixer's lead, playing silence until it catches up.
            let requested = buffer.requested() as usize;
            let datas = buffer.datas_mut();
            let Some(data) = datas.get_mut(0) else { return };
            let stride = st.channels * 4;
            let mut written = 0;
            if let Some(slice) = data.data() {
                let fits = slice.len() / stride;
                let frames = if requested > 0 { requested.min(fits) } else { fits }.min(MAX_FRAMES);
                let samples = frames * st.channels;
                if st.scratch.len() < samples && st.scratch.try_reserve(samples - st.scratch.len()).is_err() {
                    slice.fill(0);
                } else {
                    st.scratch.resize(samples, 0.0);
                    (st.fill)(&mut st.scratch[..samples], st.channels);
                    for (sample, out) in st.scratch[..samples].iter().zip(slice.chunks_exact_mut(4)) {
                        let v = if sample.is_finite() { sample.clamp(-1.0, 1.0) } else { 0.0 };
                        out.copy_from_slice(&v.to_le_bytes());
                    }
                    st.played.fetch_add(frames as u64, Ordering::SeqCst);
                    written = frames * stride;
                }
            }
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as u32 as _;
            *chunk.size_mut() = written as u32 as _;
        })
        .state_changed(|_, st, _, new| {
            if let pw::stream::StreamState::Error(e) = new
                && !st.failed.swap(true, Ordering::SeqCst)
            {
                eprintln!("filmcraft: PipeWire stream error: {e}");
            }
        })
        .register()
        .map_err(|e| e.to_string())?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F32LE);
    info.set_rate(config.rate);
    info.set_channels(config.channels as u32);
    let mut position = [0; spa::param::audio::MAX_CHANNELS];
    position[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    position[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(position);
    let values = spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(spa::pod::Object { type_: spa::sys::SPA_TYPE_OBJECT_Format, id: spa::sys::SPA_PARAM_EnumFormat, properties: info.into() }),
    )
    .map_err(|e| format!("audio format: {e:?}"))?
    .0
    .into_inner();
    let pod = spa::pod::Pod::from_bytes(&values).ok_or("audio format did not serialize to a pod")?;
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS | pw::stream::StreamFlags::RT_PROCESS,
            &mut [pod],
        )
        .map_err(|e| e.to_string())?;

    let _ = ready_tx.send(Ok(()));
    mainloop.run();
    Ok(())
}

/// All PipeWire nodes of `media_class`, as (display name, node name), via one registry roundtrip.
fn enumerate(media_class: &str) -> Vec<Sink> {
    init_once();
    let found = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let ok = (|| -> Result<(), String> {
        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| e.to_string())?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| e.to_string())?;
        let core = context.connect_rc(None).map_err(|e| e.to_string())?;
        let registry = core.get_registry().map_err(|e| e.to_string())?;
        let media_class = media_class.to_string();
        let found = found.clone();
        let _reg = registry
            .add_listener_local()
            .global(move |global| {
                let Some(props) = global.props else { return };
                if props.get("media.class") != Some(media_class.as_str()) {
                    return;
                }
                let Some(name) = props.get("node.name") else { return };
                let display = props.get("node.description").or_else(|| props.get("node.nick")).unwrap_or(name);
                found.borrow_mut().push((display.to_string(), name.to_string()));
            })
            .register();
        // One sync roundtrip: every existing global has been announced when `done` comes back.
        let pending = core.sync(0).map_err(|e| e.to_string())?;
        let loop_ = mainloop.clone();
        let _core = core
            .add_listener_local()
            .done(move |id, seq| {
                if id == pw::core::PW_ID_CORE && seq == pending {
                    loop_.quit();
                }
            })
            .register();
        mainloop.run();
        Ok(())
    })();
    if let Err(e) = ok {
        eprintln!("filmcraft: PipeWire device listing failed: {e}");
    }
    found.take()
}

/// The desktop audio output: PipeWire when reachable and chosen (or by default), cpal otherwise.
pub struct SystemOut {
    pipewire: PipeWireOut,
    cpal: CpalOut,
    hw: AudioHardwarePrefs,
    pw_available: bool,
    /// Which backend `start` last used, so the clock and device queries stay consistent.
    active_pw: bool,
}

impl SystemOut {
    pub fn new() -> Self {
        let pw_available = available();
        Self { pipewire: PipeWireOut::new(), cpal: CpalOut::new(), hw: AudioHardwarePrefs::default(), pw_available, active_pw: pw_available }
    }

    fn use_pw(&self) -> bool {
        self.pw_available && (self.hw.device_class.is_empty() || self.hw.device_class == HOST_NAME)
    }

    fn active(&self) -> &dyn AudioOut {
        if self.active_pw { &self.pipewire } else { &self.cpal }
    }
}

impl AudioOut for SystemOut {
    fn start(&mut self, fill: Box<dyn FnMut(&mut [f32], usize) + Send>) -> Result<u32, String> {
        self.pipewire.stop();
        self.cpal.stop();
        self.active_pw = self.use_pw();
        if self.active_pw { self.pipewire.start(fill) } else { self.cpal.start(fill) }
    }
    fn stop(&mut self) {
        self.pipewire.stop();
        self.cpal.stop();
    }
    fn sample_rate(&self) -> u32 {
        self.active().sample_rate()
    }
    fn channels(&self) -> usize {
        self.active().channels()
    }
    fn played_frames(&self) -> Option<u64> {
        self.active().played_frames()
    }
    fn devices(&self) -> AudioDevices {
        let mut devices = if self.use_pw() { self.pipewire.devices() } else { self.cpal.devices() };
        let mut hosts = Vec::new();
        if self.pw_available {
            hosts.push(HOST_NAME.to_string());
        }
        hosts.extend(self.cpal.devices().hosts);
        devices.hosts = hosts;
        devices
    }
    fn configure(&mut self, hw: &AudioHardwarePrefs, document_rate: Option<u32>) {
        self.hw = hw.clone();
        self.pipewire.configure(hw, document_rate);
        self.cpal.configure(hw, document_rate);
    }
}
