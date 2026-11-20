//! The one video stream: media types, frame copies and 30 fps pacing.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};

use shared::{Format, NV12, Shared, YUY2};
use windows::Win32::Foundation::{CloseHandle, E_NOTIMPL, E_POINTER, E_UNEXPECTED, HANDLE, S_OK};
use windows::Win32::Media::KernelStreaming::PINNAME_VIDEO_CAPTURE;
use windows::Win32::Media::MediaFoundation::{
    IMF2DBuffer2, IMFAsyncCallback, IMFAsyncCallback_Impl, IMFAsyncResult, IMFMediaEvent,
    IMFMediaEventGenerator_Impl, IMFMediaEventQueue, IMFMediaSource, IMFMediaStream_Impl,
    IMFMediaStream2_Impl, IMFMediaType, IMFSample, IMFStreamDescriptor, IMFVideoSampleAllocator,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MEMediaSample, MEStreamStarted, MEStreamStopped,
    MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES, MF_DEVICESTREAM_FRAMESERVER_SHARED,
    MF_DEVICESTREAM_STREAM_CATEGORY, MF_DEVICESTREAM_STREAM_ID, MF_E_INVALID_STATE_TRANSITION,
    MF_E_INVALIDMEDIATYPE, MF_E_INVALIDREQUEST, MF_E_SHUTDOWN, MF_MT_ALL_SAMPLES_INDEPENDENT,
    MF_MT_AVG_BITRATE, MF_MT_DEFAULT_STRIDE, MF_MT_FIXED_SIZE_SAMPLES, MF_MT_FRAME_RATE,
    MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO,
    MF_MT_SAMPLE_SIZE, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_VIDEO_PRIMARIES,
    MF_MT_YUV_MATRIX, MF_STREAM_STATE, MF_STREAM_STATE_PAUSED, MF_STREAM_STATE_RUNNING,
    MF_STREAM_STATE_STOPPED, MF2DBuffer_LockFlags_Write, MFCancelWorkItem, MFCreateAsyncResult,
    MFCreateEventQueue, MFCreateMediaType, MFCreateStreamDescriptor,
    MFCreateVideoSampleAllocatorEx, MFFrameSourceTypes_Color, MFGetSystemTime, MFMediaType_Video,
    MFNominalRange_16_235, MFPutWaitingWorkItem, MFSampleExtension_Token, MFVideoFormat_NV12,
    MFVideoFormat_YUY2, MFVideoInterlace_Progressive, MFVideoPrimaries_BT709,
    MFVideoTransferMatrix_BT709,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Threading::{
    CREATE_WAITABLE_TIMER_HIGH_RESOLUTION, CancelWaitableTimer, CreateWaitableTimerExW,
    SetWaitableTimer, TIMER_ALL_ACCESS,
};
use windows_core::{GUID, HRESULT, IUnknown, Interface, PCWSTR, Ref, Result};

use crate::source::time_var;
use crate::{Live, guard, lock};

const FPS: u32 = 30;
/// One frame in 100 ns units.
const INTERVAL: i64 = 10_000_000_i64.div_euclid(FPS as i64);
/// How long a frame may go without a newer one before the placeholder.
const STALE_MS: u64 = 1000;
/// The app writes 15 to 25 frames a second, so a feed this quiet has stopped.
const FEED_MS: u64 = 250;
const POOL: u32 = 10;
/// Dark grey, so a camera with no feed still looks alive.
const PLACEHOLDER_Y: u8 = 40;
const NEUTRAL_UV: u8 = 128;

/// Every format offered, the default first. 720p is what calls send, and apps that took 1080p
/// and scaled it down themselves came out softer for the same bits.
pub(crate) const FORMATS: [Format; 7] = [
    Format {
        width: 1280,
        height: 720,
        fourcc: NV12,
    },
    Format {
        width: 1920,
        height: 1080,
        fourcc: NV12,
    },
    Format {
        width: 960,
        height: 540,
        fourcc: NV12,
    },
    Format {
        width: 640,
        height: 360,
        fourcc: NV12,
    },
    Format {
        width: 1280,
        height: 720,
        fourcc: YUY2,
    },
    Format {
        width: 1920,
        height: 1080,
        fourcc: YUY2,
    },
    Format {
        width: 640,
        height: 360,
        fourcc: YUY2,
    },
];

fn pack(hi: u32, lo: u32) -> u64 {
    (u64::from(hi) << 32) | u64::from(lo)
}

#[expect(unsafe_code, reason = "media types are built through COM calls")]
fn media_type(f: Format) -> Result<IMFMediaType> {
    let (subtype, stride) = if f.fourcc == YUY2 {
        (MFVideoFormat_YUY2, f.width * 2)
    } else {
        (MFVideoFormat_NV12, f.width)
    };
    let bytes = f.bytes() as u32;
    // SAFETY: every GUID passed is a static or a live local.
    unsafe {
        let ty = MFCreateMediaType()?;
        ty.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        ty.SetGUID(&MF_MT_SUBTYPE, &raw const subtype)?;
        ty.SetUINT64(&MF_MT_FRAME_SIZE, pack(f.width, f.height))?;
        ty.SetUINT64(&MF_MT_FRAME_RATE, pack(FPS, 1))?;
        ty.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
        ty.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        ty.SetUINT32(&MF_MT_DEFAULT_STRIDE, stride)?;
        ty.SetUINT32(&MF_MT_SAMPLE_SIZE, bytes)?;
        ty.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
        ty.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
        ty.SetUINT32(&MF_MT_AVG_BITRATE, bytes * 8 * FPS)?;
        ty.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
        ty.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT709.0 as u32)?;
        Ok(ty)
    }
}

/// Which of [`FORMATS`] a media type asks for.
#[expect(unsafe_code, reason = "reading attributes is a COM call")]
fn format_of(ty: &IMFMediaType) -> Result<Format> {
    // SAFETY: `ty` is a live media type and the keys are statics.
    let (subtype, size) = unsafe {
        (
            ty.GetGUID(&MF_MT_SUBTYPE)?,
            ty.GetUINT64(&MF_MT_FRAME_SIZE)?,
        )
    };
    let fourcc = match subtype {
        s if s == MFVideoFormat_NV12 => NV12,
        s if s == MFVideoFormat_YUY2 => YUY2,
        _ => return Err(MF_E_INVALIDMEDIATYPE.into()),
    };
    let want = Format {
        width: (size >> 32) as u32,
        height: size as u32,
        fourcc,
    };
    FORMATS
        .into_iter()
        .find(|&f| f == want)
        .ok_or_else(|| MF_E_INVALIDMEDIATYPE.into())
}

fn fill_placeholder(buf: &mut [u8], f: Format) {
    if f.fourcc == YUY2 {
        buf.as_chunks_mut::<2>().0.fill([PLACEHOLDER_Y, NEUTRAL_UV]);
    } else {
        let luma = (f.width as usize * f.height as usize).min(buf.len());
        let (y, uv) = buf.split_at_mut(luma);
        y.fill(PLACEHOLDER_Y);
        uv.fill(NEUTRAL_UV);
    }
}

/// Copies a tightly packed frame into a locked 2D buffer. MF lays NV12 out
/// as the Y rows followed by the UV rows at the same pitch, so both formats
/// are just `rows` rows of `row` bytes.
#[expect(unsafe_code, reason = "the destination is a raw locked buffer")]
fn copy_frame(
    src: &[u8],
    f: Format,
    scan0: *mut u8,
    pitch: i32,
    start: *mut u8,
    len: u32,
) -> Result<()> {
    let (w, h) = (f.width as usize, f.height as usize);
    let (row, rows) = if f.fourcc == YUY2 {
        (w * 2, h)
    } else {
        (w, (h * 3).div_euclid(2))
    };
    let offset = (scan0 as usize).checked_sub(start as usize);
    let pitch = usize::try_from(pitch).unwrap_or(0);
    let fits = offset.is_some_and(|o| o + (rows - 1) * pitch + row <= len as usize);
    if src.len() != row * rows || pitch < row || !fits {
        return Err(E_UNEXPECTED.into());
    }
    // SAFETY: the check above keeps every row inside the `len` bytes locked at `start`.
    unsafe {
        if pitch == row {
            std::ptr::copy_nonoverlapping(src.as_ptr(), scan0, src.len());
        } else {
            for (i, line) in src.chunks_exact(row).enumerate() {
                std::ptr::copy_nonoverlapping(line.as_ptr(), scan0.add(i * pitch), row);
            }
        }
    }
    Ok(())
}

#[expect(unsafe_code, reason = "locking a sample buffer is a COM call")]
fn write_sample(sample: &IMFSample, src: &[u8], f: Format) -> Result<()> {
    // SAFETY: pooled samples carry one buffer.
    let buffer: IMF2DBuffer2 = unsafe { sample.GetBufferByIndex(0)? }.cast()?;
    let (mut scan0, mut pitch, mut start, mut len) =
        (std::ptr::null_mut(), 0, std::ptr::null_mut(), 0);
    // SAFETY: the out slots are live locals.
    unsafe {
        buffer.Lock2DSize(
            MF2DBuffer_LockFlags_Write,
            &raw mut scan0,
            &raw mut pitch,
            &raw mut start,
            &raw mut len,
        )?;
    };
    let copied = copy_frame(src, f, scan0, pitch, start, len);
    // SAFETY: the buffer was locked above.
    unsafe { buffer.Unlock2D()? };
    copied
}

#[derive(Debug, Default)]
struct State {
    stream: MF_STREAM_STATE,
    shut: bool,
    source: Option<IMFMediaSource>,
    allocator: Option<IMFVideoSampleAllocator>,
    format: Option<Format>,
    shared: Option<Shared>,
    /// Where `read_latest` copies to. A torn read leaves it half written,
    /// which is why the good frame is kept in `last`.
    scratch: Vec<u8>,
    last: Vec<u8>,
    /// `shared::now_ms` of the last fresh frame.
    fresh_ms: u64,
    placeholder: bool,
    /// Request tokens waiting for their frame slot.
    pending: VecDeque<Option<IUnknown>>,
    /// `MFGetSystemTime` the next frame is due, or 0 to send at once.
    due: i64,
    timer: HANDLE,
    armed: Option<(IMFAsyncResult, u64)>,
}

impl State {
    #[expect(unsafe_code, reason = "cancelling work items and timers is Win32")]
    fn disarm(&mut self) {
        if let Some((_, key)) = self.armed.take() {
            // SAFETY: the key came from MFPutWaitingWorkItem.
            unsafe { MFCancelWorkItem(key).ok() };
        }
        if !self.timer.is_invalid() {
            // SAFETY: the timer is ours and still open.
            unsafe { CancelWaitableTimer(self.timer).ok() };
        }
    }

    #[expect(unsafe_code, reason = "the allocator is a COM object")]
    fn halt(&mut self) {
        self.stream = MF_STREAM_STATE_STOPPED;
        self.pending.clear();
        self.disarm();
        if let Some(a) = &self.allocator {
            // SAFETY: the allocator is live.
            unsafe { a.UninitializeSampleAllocator().ok() };
        }
        if let Some(s) = &self.shared {
            s.touch_consumer(false);
        }
    }

    /// Brings `last` up to date: the newest frame, the previous one for up
    /// to a second, then the placeholder.
    fn refresh(&mut self, f: Format) {
        let now = shared::now_ms();
        let fresh = self
            .shared
            .as_ref()
            .and_then(|s| s.read_latest(f, FEED_MS, &mut self.scratch));
        if fresh.is_some() {
            std::mem::swap(&mut self.scratch, &mut self.last);
            self.fresh_ms = now;
            self.placeholder = false;
        } else if !self.placeholder && now.saturating_sub(self.fresh_ms) > STALE_MS {
            fill_placeholder(&mut self.last, f);
            self.placeholder = true;
        }
    }
}

impl Drop for State {
    #[expect(unsafe_code, reason = "closing a handle is Win32")]
    fn drop(&mut self) {
        self.disarm();
        if !self.timer.is_invalid() {
            // SAFETY: the timer is ours, and nothing uses it after drop.
            unsafe { CloseHandle(self.timer).ok() };
        }
    }
}

/// The stream's state and queue, shared with the pacing timer's callback.
#[derive(Debug)]
pub struct Inner {
    queue: IMFMediaEventQueue,
    descriptor: IMFStreamDescriptor,
    tick: IMFAsyncCallback,
    state: Mutex<State>,
}

#[expect(
    clippy::significant_drop_tightening,
    reason = "holding the state lock for the whole call keeps Frame Server's threads from interleaving"
)]
impl Inner {
    fn check(&self) -> Result<MutexGuard<'_, State>> {
        let state = lock(&self.state);
        if state.shut {
            Err(MF_E_SHUTDOWN.into())
        } else {
            Ok(state)
        }
    }

    fn alive(&self) -> Result<()> {
        self.check().map(drop)
    }

    /// The stream descriptor, which also holds the stream attributes.
    pub fn descriptor(&self) -> IMFStreamDescriptor {
        self.descriptor.clone()
    }

    pub fn set_source(&self, source: IMFMediaSource) {
        lock(&self.state).source = Some(source);
    }

    pub fn set_allocator(&self, allocator: IMFVideoSampleAllocator) -> Result<()> {
        let mut s = self.check()?;
        if s.stream == MF_STREAM_STATE_RUNNING {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        s.allocator = Some(allocator);
        Ok(())
    }

    /// Starts streaming `ty`, or switches to it if already running.
    #[expect(
        unsafe_code,
        reason = "the allocator, timer and queue are Win32 and COM calls"
    )]
    pub fn start(&self, ty: &IMFMediaType, time: i64, event: bool) -> Result<()> {
        let f = format_of(ty)?;
        let mut s = self.check()?;
        if s.stream != MF_STREAM_STATE_RUNNING || s.format != Some(f) {
            s.disarm();
            let allocator = if let Some(a) = &s.allocator {
                a.clone()
            } else {
                let mut a = None;
                // SAFETY: the IID matches the out slot's type.
                unsafe {
                    MFCreateVideoSampleAllocatorEx(
                        &IMFVideoSampleAllocator::IID,
                        (&raw mut a).cast::<*mut std::ffi::c_void>(),
                    )?;
                };
                let a: IMFVideoSampleAllocator = a.ok_or(E_POINTER)?;
                s.allocator = Some(a.clone());
                a
            };
            // SAFETY: the allocator and `ty` are live.
            unsafe {
                allocator.UninitializeSampleAllocator().ok();
                allocator.InitializeSampleAllocator(POOL, ty)?;
            }
            if s.timer.is_invalid() {
                // SAFETY: no name and no security attributes, which Windows allows.
                s.timer = unsafe {
                    CreateWaitableTimerExW(
                        None,
                        PCWSTR::null(),
                        CREATE_WAITABLE_TIMER_HIGH_RESOLUTION,
                        TIMER_ALL_ACCESS.0,
                    )?
                };
            }
            s.scratch.resize(f.bytes(), 0);
            s.last.resize(f.bytes(), 0);
            fill_placeholder(&mut s.last, f);
            s.placeholder = true;
            s.format = Some(f);
            s.due = 0;
        }
        if s.shared.is_none() {
            s.shared = Shared::create().ok();
        }
        if let Some(shared) = &s.shared {
            shared.set_wanted(f);
        }
        s.stream = MF_STREAM_STATE_RUNNING;
        if event {
            let var = time_var(time);
            // SAFETY: the event's arguments are live locals.
            unsafe {
                self.queue.QueueEventParamVar(
                    MEStreamStarted.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    &raw const var,
                )?;
            };
        }
        self.pump(&mut s)
    }

    #[expect(unsafe_code, reason = "queueing an event is a COM call")]
    pub fn stop(&self, event: bool) -> Result<()> {
        let mut s = self.check()?;
        s.halt();
        if event {
            // SAFETY: a stopped event carries no value, so null is allowed.
            unsafe {
                self.queue.QueueEventParamVar(
                    MEStreamStopped.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    std::ptr::null(),
                )?;
            };
        }
        Ok(())
    }

    #[expect(unsafe_code, reason = "shutting the queue down is a COM call")]
    pub fn shutdown(&self) {
        let mut s = lock(&self.state);
        if s.shut {
            return;
        }
        s.halt();
        s.shut = true;
        s.source = None;
        s.allocator = None;
        s.shared = None;
        // SAFETY: the queue is live.
        unsafe { self.queue.Shutdown().ok() };
    }

    #[expect(unsafe_code, reason = "reading the current media type is a COM call")]
    fn set_state(&self, value: MF_STREAM_STATE) -> Result<()> {
        let current = self.check()?.stream;
        if current == value {
            return Ok(());
        }
        match value {
            MF_STREAM_STATE_RUNNING => {
                // SAFETY: the descriptor is live.
                let ty = unsafe {
                    self.descriptor
                        .GetMediaTypeHandler()?
                        .GetCurrentMediaType()?
                };
                // SAFETY: takes nothing.
                let now = unsafe { MFGetSystemTime() };
                self.start(&ty, now, false)
            }
            MF_STREAM_STATE_STOPPED => self.stop(false),
            MF_STREAM_STATE_PAUSED if current == MF_STREAM_STATE_RUNNING => {
                let mut s = self.check()?;
                s.disarm();
                s.stream = MF_STREAM_STATE_PAUSED;
                Ok(())
            }
            _ => Err(MF_E_INVALID_STATE_TRANSITION.into()),
        }
    }

    fn request(&self, token: Option<IUnknown>) -> Result<()> {
        let mut s = self.check()?;
        if s.stream != MF_STREAM_STATE_RUNNING {
            return Err(MF_E_INVALIDREQUEST.into());
        }
        if let Some(shared) = &s.shared {
            shared.touch_consumer(true);
        }
        s.pending.push_back(token);
        self.pump(&mut s)
    }

    /// Sends what is due and sets the timer for the rest. Frame Server asks
    /// again as soon as a sample lands, so without the timer the camera would
    /// run as fast as the copy. Callers never wait here.
    #[expect(unsafe_code, reason = "MFGetSystemTime is a Win32 call")]
    fn pump(&self, s: &mut State) -> Result<()> {
        while s.armed.is_none() && s.stream == MF_STREAM_STATE_RUNNING {
            let Some(token) = s.pending.pop_front() else {
                break;
            };
            // SAFETY: takes nothing.
            let now = unsafe { MFGetSystemTime() };
            if now < s.due {
                s.pending.push_front(token);
                return self.arm(s, s.due - now);
            }
            if self.deliver(s, token.as_ref(), now).is_err() {
                // Usually the pool is empty because the reader holds every
                // sample. Try again a frame later rather than lose the token.
                s.pending.push_front(token);
                s.due = now + INTERVAL;
            }
        }
        Ok(())
    }

    #[expect(
        unsafe_code,
        reason = "the timer and work item are Win32 and COM calls"
    )]
    fn arm(&self, s: &mut State, delay: i64) -> Result<()> {
        // SAFETY: the timer is open, and `key` and the callback outlive the calls.
        unsafe {
            SetWaitableTimer(s.timer, &-delay, 0, None, None, false)?;
            let result = MFCreateAsyncResult(None::<&IUnknown>, &self.tick, None::<&IUnknown>)?;
            let mut key = 0;
            MFPutWaitingWorkItem(s.timer, 0, &result, Some(&raw mut key))?;
            s.armed = Some((result, key));
        }
        Ok(())
    }

    fn on_tick(&self, result: &IMFAsyncResult) {
        let mut s = lock(&self.state);
        if s.armed
            .as_ref()
            .is_some_and(|(r, _)| r.as_raw() == result.as_raw())
        {
            s.armed = None;
            self.pump(&mut s).ok();
        }
    }

    #[expect(unsafe_code, reason = "samples and events are COM calls")]
    fn deliver(&self, s: &mut State, token: Option<&IUnknown>, now: i64) -> Result<()> {
        let (Some(f), Some(allocator)) = (s.format, s.allocator.clone()) else {
            return Err(E_UNEXPECTED.into());
        };
        // On schedule, stamp the slot time so consumers see an even 30 fps.
        // After a gap, restart the schedule from now.
        let time = if s.due == 0 || now - s.due >= INTERVAL {
            now
        } else {
            s.due
        };
        s.refresh(f);
        // SAFETY: the allocator was initialized by start.
        let sample = unsafe { allocator.AllocateSample()? };
        write_sample(&sample, &s.last, f)?;
        // SAFETY: the sample, token and keys are live.
        unsafe {
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(INTERVAL)?;
            match token {
                Some(token) => sample.SetUnknown(&MFSampleExtension_Token, token)?,
                // Pooled samples come back with whatever token they last carried.
                None => {
                    sample.DeleteItem(&MFSampleExtension_Token).ok();
                }
            }
            self.queue.QueueEventParamUnk(
                MEMediaSample.0 as u32,
                &GUID::zeroed(),
                S_OK,
                &sample,
            )?;
        }
        s.due = time + INTERVAL;
        if let Some(shared) = &s.shared {
            shared.set_next_read(s.due);
        }
        Ok(())
    }
}

/// The COM objects on their own, since everything `#[implement]` generates lands beside them.
#[expect(
    unsafe_code,
    trivial_casts,
    clippy::ref_as_ptr,
    clippy::inline_always,
    reason = "the COM glue #[implement] generates casts vtables and implements unsafe methods"
)]
mod objects {
    use std::sync::{Arc, Weak};

    use windows::Win32::Media::MediaFoundation::{IMFAsyncCallback, IMFMediaStream2};
    use windows_core::implement;

    use super::Inner;
    use crate::Live;

    /// The one video stream Frame Server pulls samples from.
    #[derive(Debug)]
    #[implement(IMFMediaStream2)]
    pub struct Stream {
        pub(super) inner: Arc<Inner>,
        pub(super) _live: Live,
    }

    /// Fires when the pacing timer comes due. Holds the stream weakly so a
    /// pending work item never keeps it alive.
    #[derive(Debug)]
    #[implement(IMFAsyncCallback)]
    pub(super) struct Tick {
        pub(super) inner: Weak<Inner>,
        pub(super) _live: Live,
    }
}

pub(crate) use objects::{Stream, Stream_Impl};
use objects::{Tick, Tick_Impl};

impl Stream {
    /// Builds the stream descriptor with every media type. Cheap.
    #[expect(unsafe_code, reason = "descriptors and queues are COM calls")]
    pub fn new() -> Result<Self> {
        let types = FORMATS
            .into_iter()
            .map(|f| media_type(f).map(Some))
            .collect::<Result<Vec<_>>>()?;
        // SAFETY: `types` outlives the call.
        let descriptor = unsafe { MFCreateStreamDescriptor(0, &types)? };
        // SAFETY: the descriptor is live and the keys are statics.
        unsafe {
            descriptor
                .GetMediaTypeHandler()?
                .SetCurrentMediaType(types.first().and_then(Option::as_ref))?;
            descriptor.SetGUID(&MF_DEVICESTREAM_STREAM_CATEGORY, &PINNAME_VIDEO_CAPTURE)?;
            descriptor.SetUINT32(&MF_DEVICESTREAM_STREAM_ID, 0)?;
            descriptor.SetUINT32(&MF_DEVICESTREAM_FRAMESERVER_SHARED, 1)?;
            descriptor.SetUINT32(
                &MF_DEVICESTREAM_ATTRIBUTE_FRAMESOURCE_TYPES,
                MFFrameSourceTypes_Color.0 as u32,
            )?;
        }
        // SAFETY: takes nothing.
        let queue = unsafe { MFCreateEventQueue()? };
        let inner = Arc::new_cyclic(|weak| Inner {
            queue,
            descriptor,
            tick: Tick {
                inner: weak.clone(),
                _live: Live::new(),
            }
            .into(),
            state: Mutex::new(State::default()),
        });
        Ok(Self {
            inner,
            _live: Live::new(),
        })
    }
}

impl std::ops::Deref for Stream {
    type Target = Inner;

    fn deref(&self) -> &Inner {
        &self.inner
    }
}

#[expect(
    unsafe_code,
    reason = "every method forwards to the event queue's COM methods"
)]
impl IMFMediaEventGenerator_Impl for Stream_Impl {
    fn GetEvent(&self, flags: MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS) -> Result<IMFMediaEvent> {
        // SAFETY: forwards Frame Server's arguments untouched.
        guard(|| unsafe { self.queue.GetEvent(flags.0) })
    }

    fn BeginGetEvent(
        &self,
        callback: Ref<'_, IMFAsyncCallback>,
        state: Ref<'_, IUnknown>,
    ) -> Result<()> {
        // SAFETY: forwards Frame Server's arguments untouched.
        guard(|| unsafe { self.queue.BeginGetEvent(callback.as_ref(), state.as_ref()) })
    }

    fn EndGetEvent(&self, result: Ref<'_, IMFAsyncResult>) -> Result<IMFMediaEvent> {
        // SAFETY: forwards Frame Server's arguments untouched.
        guard(|| unsafe { self.queue.EndGetEvent(result.as_ref()) })
    }

    fn QueueEvent(
        &self,
        met: u32,
        extended: *const GUID,
        status: HRESULT,
        value: *const PROPVARIANT,
    ) -> Result<()> {
        // SAFETY: forwards Frame Server's arguments untouched.
        guard(|| unsafe { self.queue.QueueEventParamVar(met, extended, status, value) })
    }
}

impl IMFMediaStream_Impl for Stream_Impl {
    fn GetMediaSource(&self) -> Result<IMFMediaSource> {
        guard(|| {
            self.check()?
                .source
                .clone()
                .ok_or_else(|| MF_E_SHUTDOWN.into())
        })
    }

    fn GetStreamDescriptor(&self) -> Result<IMFStreamDescriptor> {
        guard(|| {
            self.alive()?;
            Ok(self.descriptor.clone())
        })
    }

    fn RequestSample(&self, token: Ref<'_, IUnknown>) -> Result<()> {
        guard(|| self.request(token.cloned()))
    }
}

impl IMFMediaStream2_Impl for Stream_Impl {
    fn SetStreamState(&self, value: MF_STREAM_STATE) -> Result<()> {
        guard(|| self.set_state(value))
    }

    fn GetStreamState(&self) -> Result<MF_STREAM_STATE> {
        guard(|| Ok(self.check()?.stream))
    }
}

impl IMFAsyncCallback_Impl for Tick_Impl {
    fn GetParameters(&self, _flags: *mut u32, _queue: *mut u32) -> Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn Invoke(&self, result: Ref<'_, IMFAsyncResult>) -> Result<()> {
        guard(|| {
            if let (Some(inner), Some(result)) = (self.inner.upgrade(), result.as_ref()) {
                inner.on_tick(result);
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copy_frame_honours_pitch() {
        let f = Format {
            width: 4,
            height: 2,
            fourcc: NV12,
        };
        let src: Vec<u8> = (0..f.bytes() as u8).collect();
        let pitch = 8;
        let mut dst = vec![0xEEu8; pitch * 3];
        let p = dst.as_mut_ptr();
        let pitch32 = i32::try_from(pitch).unwrap();
        copy_frame(&src, f, p, pitch32, p, dst.len() as u32).unwrap();
        for (row, line) in dst.chunks(pitch).zip(src.chunks(4)) {
            let (copied, rest) = row.split_at(4);
            assert_eq!(copied, line);
            assert!(rest.iter().all(|&b| b == 0xEE));
        }
        assert!(copy_frame(&src, f, p, pitch32, p, (pitch * 2 + 3) as u32).is_err());
        assert!(copy_frame(&src, f, p, 3, p, dst.len() as u32).is_err());
    }
}
