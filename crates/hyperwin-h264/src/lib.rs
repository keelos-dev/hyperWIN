//! H.264 from raw frames, by the encoders Windows has: Media Foundation transforms.
//!
//! [`Encoder::new`] takes the graphics card's encoder when there is one (Intel Quick Sync, the
//! others' equivalents: "asynchronous" transforms) and Microsoft's software encoder otherwise.
//! `HYPERWIN_H264=software` or `=hardware` insists on one kind.
//!
//! Set up for a screen that is watched live: low-latency mode (a frame in, that frame out, no
//! reordering), no periodic keyframes (the caller asks for one when a client needs it), Main
//! profile. The output is Annex B (start codes) with SPS and PPS before every keyframe, which
//! is what codec 3 of the keelOS::Video channel means; that the encoders write it so is what
//! the tests at the end check on the machine they run on.
//!
//! The frames come as 32-bit pixels, blue first, and are turned into NV12 here (BT.709, limited
//! range: what kRDPclient's decoder assumes).

#![cfg(windows)]

use anyhow::{anyhow, bail, Context as _};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::core::{Interface as _, GUID};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED};

/// One encoded picture, Annex B.
pub struct Encoded {
    pub data: Vec<u8>,
    pub key: bool,
}

pub struct Encoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    /// An asynchronous transform's events (hardware encoders); None: a synchronous one.
    events: Option<IMFMediaEventGenerator>,
    /// asynchronous: how many inputs the transform has asked for and not got yet
    wanted: u32,
    ready: VecDeque<Encoded>,
    name: String,
    width: u32,
    height: u32,
    frame: i64,
    /// 100 ns units
    frame_time: i64,
    nv12: Vec<u8>,
}

// The COM objects are used from one thread at a time (the encoder is owned by its thread), and
// Media Foundation's transforms are free-threaded.
unsafe impl Send for Encoder {}

/// A VARIANT holding a 32-bit number or a boolean: all the encoder's settings need.
#[repr(C)]
struct Variant {
    vt: u16,
    reserved: [u16; 3],
    value: u64,
    more: u64,
}

impl Variant {
    fn u32(v: u32) -> Self {
        Variant { vt: 19, reserved: [0; 3], value: u64::from(v), more: 0 } // VT_UI4
    }
    fn bool(v: bool) -> Self {
        Variant { vt: 11, reserved: [0; 3], value: if v { 0xffff } else { 0 }, more: 0 } // VT_BOOL
    }
}

fn pack(high: u32, low: u32) -> u64 {
    u64::from(high) << 32 | u64::from(low)
}

fn start() -> anyhow::Result<()> {
    // per thread; "already initialised" (in either mode) is fine
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        MFStartup(MF_VERSION, MFSTARTUP_LITE).context("MFStartup")
    }
}

impl Encoder {
    /// `width` and `height` must be even.
    pub fn new(width: u32, height: u32, fps: u32, kbps: u32) -> anyhow::Result<Encoder> {
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            bail!("H.264 needs an even size, not {width}x{height}");
        }
        start()?;
        let kind = std::env::var("HYPERWIN_H264").unwrap_or_default();
        if kind != "software" {
            match Self::hardware(width, height, fps, kbps) {
                Ok(e) => return Ok(e),
                Err(e) if kind == "hardware" => return Err(e.context("the hardware H.264 encoder")),
                Err(e) => tracing::info!("no hardware H.264 encoder ({e:#}): Microsoft's software one"),
            }
        }
        let transform: IMFTransform = unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER) }.context("Microsoft's H.264 encoder")?;
        Self::set_up(transform, "Microsoft H264 Encoder MFT (software)".into(), false, width, height, fps, kbps)
    }

    /// The first hardware encoder that takes our settings.
    fn hardware(width: u32, height: u32, fps: u32, kbps: u32) -> anyhow::Result<Encoder> {
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        unsafe { MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, None, Some(&output), &mut list, &mut count) }.context("MFTEnumEx")?;
        let activates: Vec<IMFActivate> = (0..count as usize).filter_map(|i| unsafe { (*list.add(i)).take() }).collect();
        unsafe { CoTaskMemFree(Some(list as *const _)) };
        let mut last = anyhow!("none on this machine");
        for activate in activates {
            let name = friendly_name(&activate);
            let made = unsafe { activate.ActivateObject::<IMFTransform>() }.context("activate").and_then(|t| Self::set_up(t, name.clone(), true, width, height, fps, kbps));
            match made {
                Ok(e) => return Ok(e),
                Err(e) => {
                    tracing::info!(encoder = %name, "hardware encoder not usable: {e:#}");
                    unsafe {
                        let _ = activate.ShutdownObject();
                    }
                    last = e.context(name);
                }
            }
        }
        Err(last)
    }

    fn set_up(transform: IMFTransform, name: String, hardware: bool, width: u32, height: u32, fps: u32, kbps: u32) -> anyhow::Result<Encoder> {
        unsafe {
            let mut events = None;
            if hardware {
                let attributes = transform.GetAttributes().context("attributes")?;
                if attributes.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0 {
                    attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1).context("unlock the asynchronous transform")?;
                    events = Some(transform.cast::<IMFMediaEventGenerator>().context("event generator")?);
                }
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let codec = transform.cast::<ICodecAPI>().ok();
            let set = |what: &str, key: &GUID, value: Variant| {
                if let Some(codec) = &codec {
                    if let Err(e) = codec.SetValue(key, &value as *const Variant as *const _) {
                        tracing::debug!(encoder = %name, "{what}: {e}");
                    }
                }
            };
            // before the types: the rate control mode decides what the encoder sets up
            set("rate control", &CODECAPI_AVEncCommonRateControlMode, Variant::u32(eAVEncCommonRateControlMode_CBR.0 as u32));
            set("bit rate", &CODECAPI_AVEncCommonMeanBitRate, Variant::u32(kbps * 1000));
            set("low latency", &CODECAPI_AVLowLatencyMode, Variant::bool(true));
            set("no B pictures", &CODECAPI_AVEncMPVDefaultBPictureCount, Variant::u32(0));
            // keyframes only when asked for (a client that joins, a lost frame)
            set("GOP", &CODECAPI_AVEncMPVGOPSize, Variant::u32(0x7fff_ffff));

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            output.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            output.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetOutputType(0, &output, 0).context("output type (H.264)")?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            input.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
            input.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
            transform.SetInputType(0, &input, 0).context("input type (NV12)")?;

            transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0).ok();
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0).context("begin streaming")?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0).context("start of stream")?;
            tracing::info!(encoder = %name, width, height, fps, kbps, asynchronous = events.is_some(), "H.264 encoder");
            Ok(Encoder {
                transform,
                codec,
                events,
                wanted: 0,
                ready: VecDeque::new(),
                name,
                width,
                height,
                frame: 0,
                frame_time: 10_000_000 / i64::from(fps.max(1)),
                nv12: vec![0; (width * height * 3 / 2) as usize],
            })
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// Encodes one frame: `bgrx` is `height` rows of `stride` bytes, 4 bytes a pixel, blue
    /// first. Returns that frame encoded (low-latency mode: one in, one out); None when the
    /// encoder kept it.
    pub fn encode(&mut self, bgrx: &[u8], stride: usize, key: bool) -> anyhow::Result<Option<Encoded>> {
        let (w, h) = (self.width as usize, self.height as usize);
        if stride < w * 4 || bgrx.len() < stride * (h - 1) + w * 4 {
            bail!("a frame of {} bytes is not {w}x{h}", bgrx.len());
        }
        nv12(bgrx, stride, w, h, &mut self.nv12);
        unsafe {
            let buffer = MFCreateMemoryBuffer(self.nv12.len() as u32)?;
            let mut p = std::ptr::null_mut();
            buffer.Lock(&mut p, None, None)?;
            std::ptr::copy_nonoverlapping(self.nv12.as_ptr(), p, self.nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(self.nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.frame * self.frame_time)?;
            sample.SetSampleDuration(self.frame_time)?;
            self.frame += 1;
            if key {
                match &self.codec {
                    Some(codec) => codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &Variant::u32(1) as *const Variant as *const _).context("force a keyframe")?,
                    None => bail!("{}: no way to ask for a keyframe", self.name),
                }
            }
            if self.events.is_some() {
                // asynchronous: give the frame when the transform asks for one, then wait for
                // its output
                let deadline = Instant::now() + Duration::from_secs(2);
                while self.wanted == 0 {
                    self.event(deadline)?;
                }
                self.wanted -= 1;
                self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")?;
                while self.ready.is_empty() {
                    self.event(deadline)?;
                }
            } else {
                self.transform.ProcessInput(0, &sample, 0).context("ProcessInput")?;
                while let Some(out) = self.output()? {
                    self.ready.push_back(out);
                }
            }
        }
        Ok(self.ready.pop_front())
    }

    /// Waits for the asynchronous transform's next event and acts on it.
    unsafe fn event(&mut self, deadline: Instant) -> anyhow::Result<()> {
        let events = self.events.clone().expect("asynchronous");
        loop {
            match events.GetEvent(MF_EVENT_FLAG_NO_WAIT) {
                Ok(event) => {
                    let ty = event.GetType()?;
                    if ty == METransformNeedInput.0 as u32 {
                        self.wanted += 1;
                    } else if ty == METransformHaveOutput.0 as u32 {
                        if let Some(out) = self.output()? {
                            self.ready.push_back(out);
                        }
                    }
                    return Ok(());
                }
                Err(e) if e.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() > deadline {
                        bail!("{}: no answer for 2 s", self.name);
                    }
                    std::thread::sleep(Duration::from_micros(300));
                }
                Err(e) => return Err(e).context("GetEvent"),
            }
        }
    }

    /// One ProcessOutput: an encoded picture, or None when the transform wants more input.
    unsafe fn output(&mut self) -> anyhow::Result<Option<Encoded>> {
        loop {
            let info = self.transform.GetOutputStreamInfo(0).context("GetOutputStreamInfo")?;
            let provides = info.dwFlags & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32 != 0;
            let mut out = MFT_OUTPUT_DATA_BUFFER { dwStreamID: 0, pSample: ManuallyDrop::new(None), dwStatus: 0, pEvents: ManuallyDrop::new(None) };
            if !provides {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(info.cbSize.max(1 << 20))?)?;
                out.pSample = ManuallyDrop::new(Some(sample));
            }
            let mut status = 0u32;
            let mut outs = [out];
            let result = self.transform.ProcessOutput(0, &mut outs, &mut status);
            let sample = ManuallyDrop::take(&mut outs[0].pSample);
            drop(ManuallyDrop::take(&mut outs[0].pEvents));
            match result {
                Ok(()) => {}
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // the encoder settled its output type: take it and ask again. (An
                    // asynchronous transform announces the output anew: asking at once is
                    // E_UNEXPECTED, seen with Intel's.)
                    let ty = self.transform.GetOutputAvailableType(0, 0).context("the changed output type")?;
                    self.transform.SetOutputType(0, &ty, 0).context("set the changed output type")?;
                    if self.events.is_some() {
                        return Ok(None);
                    }
                    continue;
                }
                Err(e) => return Err(e).context("ProcessOutput"),
            }
            let Some(sample) = sample else { return Ok(None) };
            let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
            let buffer = sample.ConvertToContiguousBuffer()?;
            let (mut p, mut len) = (std::ptr::null_mut(), 0u32);
            buffer.Lock(&mut p, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(p, len as usize).to_vec();
            buffer.Unlock()?;
            if data.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Encoded { data, key }));
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            if self.events.is_some() {
                if let Ok(shutdown) = self.transform.cast::<IMFShutdown>() {
                    let _ = shutdown.Shutdown();
                }
            }
        }
    }
}

fn friendly_name(activate: &IMFActivate) -> String {
    unsafe {
        let mut p = windows::core::PWSTR::null();
        let mut len = 0u32;
        match activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut p, &mut len) {
            Ok(()) => {
                let name = p.to_string().unwrap_or_default();
                CoTaskMemFree(Some(p.0 as *const _));
                name
            }
            Err(_) => "hardware encoder".into(),
        }
    }
}

/// The NAL unit types in an Annex B stream, in order (5 = IDR slice, 7 = SPS, 8 = PPS).
pub fn nal_types(annex_b: &[u8]) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0;
    while i + 3 < annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            v.push(annex_b[i + 3] & 0x1f);
            i += 3;
        } else {
            i += 1;
        }
    }
    v
}

/// 32-bit pixels (blue first) to NV12, BT.709 limited range; the colour of each 2x2 block is
/// its average.
fn nv12(bgrx: &[u8], stride: usize, w: usize, h: usize, out: &mut [u8]) {
    let (luma, chroma) = out.split_at_mut(w * h);
    for (y, row) in luma.chunks_exact_mut(w).enumerate() {
        let src = &bgrx[y * stride..y * stride + w * 4];
        for (d, p) in row.iter_mut().zip(src.chunks_exact(4)) {
            let (b, g, r) = (u32::from(p[0]), u32::from(p[1]), u32::from(p[2]));
            *d = ((47 * r + 157 * g + 16 * b + 128) >> 8) as u8 + 16;
        }
    }
    for (y, row) in chroma.chunks_exact_mut(w).enumerate() {
        let a = &bgrx[2 * y * stride..2 * y * stride + w * 4];
        let c = &bgrx[(2 * y + 1) * stride..(2 * y + 1) * stride + w * 4];
        for ((d, p), q) in row.chunks_exact_mut(2).zip(a.chunks_exact(8)).zip(c.chunks_exact(8)) {
            let b = i32::from(p[0]) + i32::from(p[4]) + i32::from(q[0]) + i32::from(q[4]);
            let g = i32::from(p[1]) + i32::from(p[5]) + i32::from(q[1]) + i32::from(q[5]);
            let r = i32::from(p[2]) + i32::from(p[6]) + i32::from(q[2]) + i32::from(q[6]);
            // the sums are of four pixels: >> 10 instead of >> 8
            d[0] = (128 + ((-26 * r - 86 * g + 112 * b + 512) >> 10)).clamp(16, 240) as u8;
            d[1] = (128 + ((112 * r - 102 * g - 10 * b + 512) >> 10)).clamp(16, 240) as u8;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours() {
        // 2x2 blocks of white, black, red, blue
        let px = |b: u8, g: u8, r: u8| [b, g, r, 0];
        let colours = [px(255, 255, 255), px(0, 0, 0), px(0, 0, 255), px(255, 0, 0)];
        let (w, h) = (8, 2);
        let mut frame = Vec::new();
        for _ in 0..h {
            for c in colours {
                frame.extend_from_slice(&c);
                frame.extend_from_slice(&c);
            }
        }
        let mut out = vec![0u8; w * h * 3 / 2];
        nv12(&frame, w * 4, w, h, &mut out);
        // BT.709 limited: white 235/128/128, black 16/128/128, red 63/102/240, blue 32/240/118
        assert_eq!(&out[0..8], &[235, 235, 16, 16, 63, 63, 32, 32]);
        assert_eq!(&out[16..24], &[128, 128, 128, 128, 102, 240, 240, 118]);
    }

    /// The machine's encoders, for real: a keyframe carries SPS, PPS and an IDR slice, and a
    /// keyframe asked for later is one.
    fn run(kind: &str) {
        std::env::set_var("HYPERWIN_H264", kind);
        let (w, h) = (640usize, 480usize);
        let mut e = match Encoder::new(w as u32, h as u32, 30, 4000) {
            Ok(e) => e,
            Err(e) if kind == "hardware" => return eprintln!("no hardware encoder here: {e:#}"),
            Err(e) => panic!("{e:#}"),
        };
        eprintln!("{kind}: {}", e.name());
        let mut frame = vec![0u8; w * h * 4];
        let mut keys = 0;
        for n in 0..20usize {
            for (i, p) in frame.chunks_exact_mut(4).enumerate() {
                p[0] = (i + n * 7) as u8;
                p[1] = (i / w + n * 3) as u8;
                p[2] = (n * 11) as u8;
            }
            let key = n == 0 || n == 10;
            let out = e.encode(&frame, w * 4, key).unwrap().expect("one frame in, one out");
            let nals = nal_types(&out.data);
            eprintln!("  frame {n}: {} bytes, key {}, NAL units {:?}", out.data.len(), out.key, nals);
            assert!(out.data[..3] == [0, 0, 1] || out.data[..4] == [0, 0, 0, 1], "Annex B");
            if key {
                assert!(nals.contains(&7) && nals.contains(&8) && nals.contains(&5), "a keyframe with SPS and PPS");
            }
            keys += usize::from(nals.contains(&5));
        }
        assert_eq!(keys, 2, "keyframes only when asked for");
    }

    #[test]
    fn software_encoder() {
        run("software");
    }

    #[test]
    fn hardware_encoder() {
        run("hardware");
    }
}
