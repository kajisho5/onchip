//! A thin driver for Media Foundation transforms that hides the difference between
//! synchronous MFTs (`ProcessInput` / `ProcessOutput` until `NEED_MORE_INPUT`) and the
//! asynchronous protocol hardware MFTs use (`METransformNeedInput` /
//! `METransformHaveOutput` events, unlocked with `MF_TRANSFORM_ASYNC_UNLOCK`).
//!
//! Only the pieces the decoder and encoder need are here. Every call is `unsafe` COM
//! underneath; the safety comments state what each call relies on.

use std::mem::ManuallyDrop;

use onchip_core::backend::BackendError;
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFAttributes, IMFMediaEvent, IMFMediaEventGenerator, IMFMediaType, IMFSample,
    IMFTransform, METransformDrainComplete, METransformHaveOutput, METransformNeedInput,
    MF_E_NO_MORE_TYPES, MF_E_NOTACCEPTING, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_EVENT_FLAG_NO_WAIT, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE,
    MF_TRANSFORM_ASYNC, MF_TRANSFORM_ASYNC_UNLOCK, MFCreateMediaType, MFMediaType_Video,
    MFT_CATEGORY_VIDEO_DECODER, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_COMMAND_DRAIN, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_MESSAGE_SET_D3D_MANAGER, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_PROVIDES_SAMPLES, MFT_REGISTER_TYPE_INFO, MFTEnumEx,
    MFVideoInterlace_Progressive,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::core::{GUID, Interface, PWSTR};

use crate::d3d::werr;

/// Pack two u32 into the u64 layout `MF_MT_FRAME_SIZE` / `MF_MT_FRAME_RATE` use.
pub fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}

pub fn unpack2(v: u64) -> (u32, u32) {
    ((v >> 32) as u32, v as u32)
}

/// Enumerate hardware MFTs of a category for a compressed subtype.
pub fn enumerate_hw(
    category: &GUID,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
) -> Result<Vec<(String, IMFActivate)>, BackendError> {
    enumerate(
        category,
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
        input,
        output,
    )
}

/// Enumerate every MFT of a category (hardware, sync and async software ones), sorted
/// by Media Foundation's merit order.
pub fn enumerate_any(
    category: &GUID,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
) -> Result<Vec<(String, IMFActivate)>, BackendError> {
    enumerate(
        category,
        MFT_ENUM_FLAG_HARDWARE
            | MFT_ENUM_FLAG_SYNCMFT
            | MFT_ENUM_FLAG_ASYNCMFT
            | MFT_ENUM_FLAG_SORTANDFILTER,
        input,
        output,
    )
}

fn enumerate(
    category: &GUID,
    flags: MFT_ENUM_FLAG,
    input: Option<&MFT_REGISTER_TYPE_INFO>,
    output: Option<&MFT_REGISTER_TYPE_INFO>,
) -> Result<Vec<(String, IMFActivate)>, BackendError> {
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    // SAFETY: out-pointers valid; the array is freed below.
    let r = unsafe {
        MFTEnumEx(
            *category,
            flags,
            input.map(|p| p as *const _),
            output.map(|p| p as *const _),
            &mut activates,
            &mut count,
        )
    };
    r.map_err(|e| werr("MFTEnumEx", e))?;
    let mut out = Vec::with_capacity(count as usize);
    if !activates.is_null() {
        for i in 0..count as usize {
            // SAFETY: `count` entries were allocated by MFTEnumEx.
            let slot = unsafe { &mut *activates.add(i) };
            if let Some(act) = slot.take() {
                let name = friendly_name(&act).unwrap_or_else(|| format!("<unnamed MFT #{i}>"));
                out.push((name, act));
            }
        }
        // SAFETY: allocated with CoTaskMemAlloc by MFTEnumEx.
        unsafe { CoTaskMemFree(Some(activates as *const _)) };
    }
    Ok(out)
}

pub fn friendly_name(act: &IMFActivate) -> Option<String> {
    let mut pw = PWSTR::null();
    let mut len: u32 = 0;
    // SAFETY: GetAllocatedString allocates with CoTaskMemAlloc; freed below.
    let ok =
        unsafe { act.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut pw, &mut len) }.is_ok();
    if !ok || pw.is_null() {
        return None;
    }
    // SAFETY: valid NUL-terminated wide string.
    let s = unsafe { pw.to_string() }.ok();
    // SAFETY: allocated by GetAllocatedString.
    unsafe { CoTaskMemFree(Some(pw.as_ptr() as *const _)) };
    s
}

pub fn video_type_info(subtype: GUID) -> MFT_REGISTER_TYPE_INFO {
    MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    }
}

pub fn decoder_category() -> &'static GUID {
    &MFT_CATEGORY_VIDEO_DECODER
}

pub fn encoder_category() -> &'static GUID {
    &MFT_CATEGORY_VIDEO_ENCODER
}

/// Build a video media type with the common attributes set.
pub fn video_media_type(
    subtype: GUID,
    width: u32,
    height: u32,
    framerate: (u32, u32),
) -> Result<IMFMediaType, BackendError> {
    // SAFETY: fresh media type; setters take valid GUID pointers.
    unsafe {
        let t = MFCreateMediaType().map_err(|e| werr("MFCreateMediaType", e))?;
        t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| werr("MF_MT_MAJOR_TYPE", e))?;
        t.SetGUID(&MF_MT_SUBTYPE, &subtype)
            .map_err(|e| werr("MF_MT_SUBTYPE", e))?;
        t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(width, height))
            .map_err(|e| werr("MF_MT_FRAME_SIZE", e))?;
        t.SetUINT64(&MF_MT_FRAME_RATE, pack2(framerate.0, framerate.1))
            .map_err(|e| werr("MF_MT_FRAME_RATE", e))?;
        t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))
            .map_err(|e| werr("MF_MT_PIXEL_ASPECT_RATIO", e))?;
        t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .map_err(|e| werr("MF_MT_INTERLACE_MODE", e))?;
        Ok(t)
    }
}

/// What came out of one `pull`.
pub enum Pulled {
    Sample(IMFSample),
    /// The MFT wants more input before it can produce output.
    NeedInput,
    /// The output type changed (resolution / format); caller re-negotiates.
    StreamChange,
    /// Drain finished (async MFTs only).
    DrainComplete,
}

/// One transform plus the protocol state needed to drive it.
pub struct Transform {
    pub name: String,
    pub mft: IMFTransform,
    pub attributes: IMFAttributes,
    pub is_async: bool,
    events: Option<IMFMediaEventGenerator>,
    pub in_stream: u32,
    pub out_stream: u32,
    /// True when the MFT allocates its own output samples (`PROVIDES_SAMPLES`).
    pub provides_samples: bool,
    pub output_buffer_size: u32,
    /// Async protocol: number of `NeedInput` credits not yet spent.
    need_input_credits: u32,
    pending_output: bool,
    drain_complete: bool,
    streaming: bool,
}

unsafe impl Send for Transform {}

impl Transform {
    /// Activate an MFT, unlock async mode if needed, and (optionally) hand it the DXGI
    /// device manager.
    pub fn activate(
        name: String,
        act: &IMFActivate,
        dxgi_manager: Option<&windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager>,
    ) -> Result<Transform, BackendError> {
        // SAFETY: ActivateObject creates the MFT; everything below uses it as a live object.
        let mft: IMFTransform =
            unsafe { act.ActivateObject() }.map_err(|e| werr("ActivateObject", e))?;
        let attributes =
            unsafe { mft.GetAttributes() }.map_err(|e| werr("IMFTransform::GetAttributes", e))?;
        let is_async = unsafe { attributes.GetUINT32(&MF_TRANSFORM_ASYNC) }.unwrap_or(0) == 1;
        let mut events = None;
        if is_async {
            unsafe {
                attributes
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .map_err(|e| werr("MF_TRANSFORM_ASYNC_UNLOCK", e))?;
            }
            events = Some(
                mft.cast::<IMFMediaEventGenerator>()
                    .map_err(|e| werr("IMFMediaEventGenerator", e))?,
            );
        }
        if let Some(mgr) = dxgi_manager {
            // SAFETY: the message carries the manager as an IUnknown pointer; the manager
            // outlives the transform (both owned by the backend).
            unsafe {
                mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, mgr.as_raw() as usize)
                    .map_err(|e| werr("MFT_MESSAGE_SET_D3D_MANAGER", e))?;
            }
        }
        // stream ids (most MFTs use 0/0; GetStreamIDs fails with E_NOTIMPL for those)
        let mut ins = [0u32; 1];
        let mut outs = [0u32; 1];
        // SAFETY: arrays of length 1 passed with their lengths.
        let _ = unsafe { mft.GetStreamIDs(&mut ins, &mut outs) };
        Ok(Transform {
            name,
            mft,
            attributes,
            is_async,
            events,
            in_stream: ins[0],
            out_stream: outs[0],
            provides_samples: false,
            output_buffer_size: 0,
            need_input_credits: 0,
            pending_output: false,
            drain_complete: false,
            streaming: false,
        })
    }

    pub fn set_input_type(&self, t: &IMFMediaType) -> Result<(), BackendError> {
        // SAFETY: live transform and media type.
        unsafe { self.mft.SetInputType(self.in_stream, t, 0) }.map_err(|e| werr("SetInputType", e))
    }

    pub fn set_output_type(&self, t: &IMFMediaType) -> Result<(), BackendError> {
        // SAFETY: live transform and media type.
        unsafe { self.mft.SetOutputType(self.out_stream, t, 0) }
            .map_err(|e| werr("SetOutputType", e))
    }

    /// Pick the first available output type whose subtype matches, set it, return it.
    pub fn choose_output_type(&self, subtype: GUID) -> Result<IMFMediaType, BackendError> {
        for i in 0.. {
            // SAFETY: index enumeration ends with MF_E_NO_MORE_TYPES.
            let t = match unsafe { self.mft.GetOutputAvailableType(self.out_stream, i) } {
                Ok(t) => t,
                Err(e) if e.code() == MF_E_NO_MORE_TYPES => break,
                Err(e) => return Err(werr("GetOutputAvailableType", e)),
            };
            let st = unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.map_err(|e| werr("MF_MT_SUBTYPE", e))?;
            if st == subtype {
                self.set_output_type(&t)?;
                return Ok(t);
            }
        }
        Err(BackendError::Platform(format!(
            "{}: no output type with the requested subtype",
            self.name
        )))
    }

    /// Read `MFT_OUTPUT_STREAM_INFO` after types are set.
    pub fn refresh_stream_info(&mut self) -> Result<(), BackendError> {
        // SAFETY: types are set, so the call is valid.
        let info = unsafe { self.mft.GetOutputStreamInfo(self.out_stream) }
            .map_err(|e| werr("GetOutputStreamInfo", e))?;
        self.provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        self.output_buffer_size = info.cbSize;
        Ok(())
    }

    pub fn start(&mut self) -> Result<(), BackendError> {
        // SAFETY: messages with no payload.
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .map_err(|e| werr("NOTIFY_BEGIN_STREAMING", e))?;
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .map_err(|e| werr("NOTIFY_START_OF_STREAM", e))?;
        }
        self.streaming = true;
        Ok(())
    }

    /// For async MFTs: pump events until an input credit is available, then push.
    /// For sync MFTs: push directly. Returns `Ok(false)` if the MFT refuses input right
    /// now (`MF_E_NOTACCEPTING`); the caller should pull output and retry.
    pub fn push(&mut self, sample: &IMFSample) -> Result<bool, BackendError> {
        if self.is_async {
            while self.need_input_credits == 0 {
                if self.pending_output {
                    return Ok(false);
                }
                self.pump_event(true)?;
            }
            self.need_input_credits -= 1;
        }
        // SAFETY: live transform; sample stays alive for the call.
        match unsafe { self.mft.ProcessInput(self.in_stream, sample, 0) } {
            Ok(()) => Ok(true),
            Err(e) if e.code() == MF_E_NOTACCEPTING => Ok(false),
            Err(e) => Err(werr("ProcessInput", e)),
        }
    }

    /// Pull one output sample if available. `provided` is a caller-allocated sample for
    /// MFTs that do not provide their own (plan A for decoders; memory buffers for
    /// encoders that need them).
    pub fn pull(&mut self, provided: Option<IMFSample>) -> Result<Pulled, BackendError> {
        if self.is_async {
            if self.drain_complete {
                return Ok(Pulled::DrainComplete);
            }
            if !self.pending_output {
                // non-blocking look for HaveOutput
                self.pump_event(false)?;
                if self.drain_complete {
                    return Ok(Pulled::DrainComplete);
                }
                if !self.pending_output {
                    return Ok(Pulled::NeedInput);
                }
            }
        }
        let out = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: self.out_stream,
            pSample: ManuallyDrop::new(if self.provides_samples {
                None
            } else {
                provided
            }),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        };
        let mut status = 0u32;
        // SAFETY: `out` is a valid buffer for one stream; ownership of pSample/pEvents is
        // taken back with ManuallyDrop::take below.
        let mut outs = [out];
        let r = unsafe { self.mft.ProcessOutput(0, &mut outs, &mut status) };
        let [mut out] = outs;
        let sample = unsafe { ManuallyDrop::take(&mut out.pSample) };
        let events = unsafe { ManuallyDrop::take(&mut out.pEvents) };
        drop(events);
        match r {
            Ok(()) => {
                if self.is_async {
                    self.pending_output = false;
                }
                match sample {
                    Some(s) => Ok(Pulled::Sample(s)),
                    None => Ok(Pulled::NeedInput),
                }
            }
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                if self.is_async {
                    self.pending_output = false;
                }
                Ok(Pulled::NeedInput)
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                if self.is_async {
                    self.pending_output = false;
                }
                Ok(Pulled::StreamChange)
            }
            Err(e) => Err(werr("ProcessOutput", e)),
        }
    }

    /// Blocking pull for async MFTs: wait for the next event that yields output.
    pub fn pull_blocking(&mut self, provided: Option<IMFSample>) -> Result<Pulled, BackendError> {
        if !self.is_async {
            return self.pull(provided);
        }
        loop {
            if self.drain_complete {
                return Ok(Pulled::DrainComplete);
            }
            if self.pending_output {
                return self.pull(provided);
            }
            if self.need_input_credits > 0 && !self.draining() {
                // the MFT wants input first; tell the caller
                return Ok(Pulled::NeedInput);
            }
            self.pump_event(true)?;
        }
    }

    fn draining(&self) -> bool {
        !self.streaming
    }

    /// Tell the MFT no more input is coming and ask it to flush out what it has.
    pub fn drain(&mut self) -> Result<(), BackendError> {
        // SAFETY: payload-less messages.
        unsafe {
            self.mft
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0)
                .map_err(|e| werr("NOTIFY_END_OF_STREAM", e))?;
            self.mft
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)
                .map_err(|e| werr("COMMAND_DRAIN", e))?;
        }
        self.streaming = false;
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), BackendError> {
        // SAFETY: payload-less message.
        unsafe { self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0) }
            .map_err(|e| werr("COMMAND_FLUSH", e))?;
        self.need_input_credits = 0;
        self.pending_output = false;
        Ok(())
    }

    /// Process one event from an async MFT. `block` waits (polling, with a deadline so a
    /// misbehaving MFT turns into an error instead of a hang); otherwise returns
    /// immediately when none is queued.
    fn pump_event(&mut self, block: bool) -> Result<(), BackendError> {
        let Some(events) = &self.events else {
            return Ok(());
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        // SAFETY: live event generator.
        let ev: IMFMediaEvent = loop {
            match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(ev) => break ev,
                Err(e)
                    if e.code()
                        == windows::Win32::Media::MediaFoundation::MF_E_NO_EVENTS_AVAILABLE =>
                {
                    if !block {
                        return Ok(());
                    }
                    if std::time::Instant::now() > deadline {
                        return Err(BackendError::Platform(format!(
                            "{}: no MFT event for 15 s (need_input_credits={}, pending_output={}, streaming={})",
                            self.name, self.need_input_credits, self.pending_output, self.streaming
                        )));
                    }
                    std::thread::sleep(std::time::Duration::from_micros(500));
                }
                Err(e) => return Err(werr("GetEvent", e)),
            }
        };
        let ty = unsafe { ev.GetType() }.map_err(|e| werr("IMFMediaEvent::GetType", e))?;
        crate::pipeline::trace(|| format!("{}: event {ty}", self.name));
        match ty {
            t if t == METransformNeedInput.0 as u32 => self.need_input_credits += 1,
            t if t == METransformHaveOutput.0 as u32 => self.pending_output = true,
            t if t == METransformDrainComplete.0 as u32 => self.drain_complete = true,
            _ => {}
        }
        Ok(())
    }
}
