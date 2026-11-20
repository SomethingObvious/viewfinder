//! The media source: one live video stream, no controls, no services.

use std::ffi::c_void;
use std::sync::Mutex;

use windows::Win32::Foundation::{E_INVALIDARG, E_POINTER, ERROR_SET_NOT_FOUND, S_OK};
use windows::Win32::Media::KernelStreaming::{
    IKsControl_Impl, KSCAMERAPROFILE_HighFrameRate, KSCAMERAPROFILE_Legacy, KSIDENTIFIER,
};
use windows::Win32::Media::MediaFoundation::{
    IMFAsyncCallback, IMFAsyncResult, IMFAttributes, IMFGetService_Impl, IMFMediaEvent,
    IMFMediaEventGenerator_Impl, IMFMediaSource_Impl, IMFMediaSourceEx, IMFMediaSourceEx_Impl,
    IMFPresentationDescriptor, IMFSampleAllocatorControl_Impl,
    MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, MENewStream, MESourceStarted, MESourceStopped,
    MEUpdatedStream, MF_DEVICEMFT_SENSORPROFILE_COLLECTION, MF_E_INVALID_STATE_TRANSITION,
    MF_E_INVALIDSTREAMNUMBER, MF_E_SHUTDOWN, MF_E_UNSUPPORTED_SERVICE,
    MF_E_UNSUPPORTED_TIME_FORMAT, MFCreateAttributes, MFCreateEventQueue,
    MFCreatePresentationDescriptor, MFCreateSensorProfile, MFCreateSensorProfileCollection,
    MFGetSystemTime, MFMEDIASOURCE_IS_LIVE, MFSampleAllocatorUsage,
    MFSampleAllocatorUsage_UsesProvidedAllocator,
};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Variant::VT_I8;
use windows_core::{BOOL, ComObject, GUID, HRESULT, IUnknown, Interface, Ref, Result, w};

pub(crate) use object::{Source, Source_Impl};

use crate::stream::Stream;
use crate::{Live, guard, lock};

#[expect(unsafe_code, reason = "MFCreateAttributes is a COM call")]
pub(crate) fn new_attributes() -> Result<IMFAttributes> {
    let mut attributes = None;
    // SAFETY: `attributes` is a live out slot.
    unsafe { MFCreateAttributes(&raw mut attributes, 4)? };
    attributes.ok_or_else(|| E_POINTER.into())
}

/// The `VT_I8` payload of the started and stopped events.
#[expect(unsafe_code, reason = "PROPVARIANT is a C union")]
pub(crate) fn time_var(time: i64) -> PROPVARIANT {
    let mut var = PROPVARIANT::default();
    // SAFETY: the union is empty, and setting the tag with the matching field makes a valid VT_I8.
    unsafe {
        let inner = &mut *var.Anonymous.Anonymous;
        inner.vt = VT_I8;
        inner.Anonymous.hVal = time;
    }
    var
}

#[derive(Debug, Default)]
struct State {
    shut: bool,
    /// Set by an earlier Start, so the next one sends `MEUpdatedStream`.
    active: bool,
}

#[expect(
    unsafe_code,
    trivial_casts,
    clippy::ref_as_ptr,
    clippy::inline_always,
    reason = "the COM glue #[implement] generates casts vtables and implements unsafe methods"
)]
mod object {
    use std::sync::Mutex;

    use windows::Win32::Media::KernelStreaming::IKsControl;
    use windows::Win32::Media::MediaFoundation::{
        IMFAttributes, IMFGetService, IMFMediaEventQueue, IMFMediaSourceEx,
        IMFPresentationDescriptor, IMFSampleAllocatorControl,
    };
    use windows_core::{ComObject, implement};

    use super::State;
    use crate::Live;
    use crate::stream::Stream;

    #[derive(Debug)]
    #[implement(IMFMediaSourceEx, IMFGetService, IKsControl, IMFSampleAllocatorControl)]
    pub struct Source {
        pub(super) queue: IMFMediaEventQueue,
        pub(super) attributes: IMFAttributes,
        pub(super) descriptor: IMFPresentationDescriptor,
        pub(super) stream: ComObject<Stream>,
        pub(super) state: Mutex<State>,
        pub(super) _live: Live,
    }
}

impl Source {
    /// Frame Server also builds one just to list the camera, so nothing expensive happens here.
    #[expect(
        unsafe_code,
        reason = "building the descriptors and profiles is all COM calls"
    )]
    pub fn new(activate: &IMFAttributes) -> Result<ComObject<Self>> {
        let attributes = new_attributes()?;
        // SAFETY: every argument is a live COM object or a local that outlives the call.
        unsafe {
            activate.CopyAllItems(&attributes)?;
            let profiles = MFCreateSensorProfileCollection()?;
            for (kind, filter) in [
                (KSCAMERAPROFILE_Legacy, w!("((RES==;FRT<=30,1;SUT==))")),
                (
                    KSCAMERAPROFILE_HighFrameRate,
                    w!("((RES==;FRT>=60,1;SUT==))"),
                ),
            ] {
                let profile = MFCreateSensorProfile(&raw const kind, 0, None)?;
                profile.AddProfileFilter(0, filter)?;
                profiles.AddProfile(&profile)?;
            }
            attributes.SetUnknown(&MF_DEVICEMFT_SENSORPROFILE_COLLECTION, &profiles)?;
        }
        let stream = ComObject::new(Stream::new()?);
        // SAFETY: the slice of descriptors outlives the call.
        let descriptor =
            unsafe { MFCreatePresentationDescriptor(Some(&[Some(stream.descriptor())]))? };
        // SAFETY: stream 0 is the one descriptor just added.
        unsafe { descriptor.SelectStream(0)? };
        // SAFETY: takes nothing.
        let queue = unsafe { MFCreateEventQueue()? };
        let source = ComObject::new(Self {
            queue,
            attributes,
            descriptor,
            stream: stream.clone(),
            state: Mutex::new(State::default()),
            _live: Live::new(),
        });
        stream.set_source(&source.to_interface::<IMFMediaSourceEx>().into());
        Ok(source)
    }

    pub fn is_shut_down(&self) -> bool {
        lock(&self.state).shut
    }

    fn check(&self) -> Result<std::sync::MutexGuard<'_, State>> {
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
}

forward_events!(Source_Impl);

#[expect(
    unsafe_code,
    reason = "starting and stopping is all COM calls on raw arguments"
)]
impl IMFMediaSource_Impl for Source_Impl {
    fn GetCharacteristics(&self) -> Result<u32> {
        guard(|| {
            self.alive()?;
            Ok(MFMEDIASOURCE_IS_LIVE.0 as u32)
        })
    }

    fn CreatePresentationDescriptor(&self) -> Result<IMFPresentationDescriptor> {
        guard(|| {
            self.alive()?;
            // SAFETY: the descriptor is live.
            unsafe { self.descriptor.Clone() }
        })
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "holding the state lock keeps Start, Stop and Shutdown from interleaving"
    )]
    fn Start(
        &self,
        pd: Ref<'_, IMFPresentationDescriptor>,
        time_format: *const GUID,
        start: *const PROPVARIANT,
    ) -> Result<()> {
        guard(|| {
            let pd = pd.ok()?;
            if start.is_null() {
                return Err(E_INVALIDARG.into());
            }
            // SAFETY: not null, and Frame Server passes a GUID when it passes anything.
            if !time_format.is_null() && unsafe { *time_format } != GUID::zeroed() {
                return Err(MF_E_UNSUPPORTED_TIME_FORMAT.into());
            }
            let mut state = self.check()?;
            // SAFETY: `pd` is a live descriptor Frame Server passed in.
            if unsafe { pd.GetStreamDescriptorCount()? } != 1 {
                return Err(E_INVALIDARG.into());
            }
            let mut selected = BOOL(0);
            let mut sd = None;
            // SAFETY: index 0 exists, and both out slots are locals.
            unsafe { pd.GetStreamDescriptorByIndex(0, &raw mut selected, &raw mut sd)? };
            let sd = sd.ok_or(E_POINTER)?;
            // SAFETY: `sd` is a live descriptor.
            if unsafe { sd.GetStreamIdentifier()? } != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            // SAFETY: takes nothing.
            let now = unsafe { MFGetSystemTime() };
            if selected.as_bool() {
                // SAFETY: `sd` is a live descriptor.
                let ty = unsafe { sd.GetMediaTypeHandler()?.GetCurrentMediaType()? };
                self.stream.start(&ty, now, true)?;
                let met = if state.active {
                    MEUpdatedStream
                } else {
                    MENewStream
                };
                let unk: IUnknown = self.stream.to_interface();
                // SAFETY: stream 0 exists, and the event's arguments are live locals.
                unsafe {
                    self.descriptor.SelectStream(0)?;
                    self.queue
                        .QueueEventParamUnk(met.0 as u32, &GUID::zeroed(), S_OK, &unk)?;
                }
                state.active = true;
            } else if state.active {
                self.stream.stop(true)?;
                // SAFETY: stream 0 exists.
                unsafe { self.descriptor.DeselectStream(0)? };
                state.active = false;
            }
            let var = time_var(now);
            // SAFETY: the event's arguments are live locals.
            unsafe {
                self.queue.QueueEventParamVar(
                    MESourceStarted.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    &raw const var,
                )
            }
        })
    }

    fn Stop(&self) -> Result<()> {
        guard(|| {
            let _state = self.check()?;
            self.stream.stop(true)?;
            // SAFETY: takes nothing.
            let var = time_var(unsafe { MFGetSystemTime() });
            // SAFETY: the event's arguments are live locals.
            unsafe {
                self.queue.QueueEventParamVar(
                    MESourceStopped.0 as u32,
                    &GUID::zeroed(),
                    S_OK,
                    &raw const var,
                )
            }
        })
    }

    fn Pause(&self) -> Result<()> {
        guard(|| {
            self.alive()?;
            Err(MF_E_INVALID_STATE_TRANSITION.into())
        })
    }

    #[expect(
        clippy::significant_drop_tightening,
        reason = "holding the state lock keeps Start, Stop and Shutdown from interleaving"
    )]
    fn Shutdown(&self) -> Result<()> {
        guard(|| {
            let mut state = self.check()?;
            state.shut = true;
            self.stream.shutdown();
            // SAFETY: the queue is live, and shutting it down twice is allowed.
            unsafe { self.queue.Shutdown() }
        })
    }
}

impl IMFMediaSourceEx_Impl for Source_Impl {
    fn GetSourceAttributes(&self) -> Result<IMFAttributes> {
        guard(|| {
            self.alive()?;
            Ok(self.attributes.clone())
        })
    }

    fn GetStreamAttributes(&self, id: u32) -> Result<IMFAttributes> {
        guard(|| {
            self.alive()?;
            if id != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            self.stream.descriptor().cast()
        })
    }

    // Samples stay in system memory, as in Microsoft's SimpleMediaSource. Frames arrive on the
    // CPU, and an allocator backed by Frame Server's device uploads them in Lock2DSize anyway.
    fn SetD3DManager(&self, _manager: Ref<'_, IUnknown>) -> Result<()> {
        guard(|| self.alive())
    }
}

#[expect(unsafe_code, reason = "COM hands over a raw out pointer")]
impl IMFGetService_Impl for Source_Impl {
    fn GetService(
        &self,
        _service: *const GUID,
        _riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> Result<()> {
        guard(|| {
            if !ppv.is_null() {
                // SAFETY: not null, and COM says it points at writable storage.
                unsafe { *ppv = std::ptr::null_mut() };
            }
            Err(MF_E_UNSUPPORTED_SERVICE.into())
        })
    }
}

fn not_found() -> Result<()> {
    Err(ERROR_SET_NOT_FOUND.to_hresult().into())
}

impl IKsControl_Impl for Source_Impl {
    fn KsProperty(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        not_found()
    }

    fn KsMethod(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        not_found()
    }

    fn KsEvent(
        &self,
        _: *const KSIDENTIFIER,
        _: u32,
        _: *mut c_void,
        _: u32,
        _: *mut u32,
    ) -> Result<()> {
        not_found()
    }
}

#[expect(unsafe_code, reason = "COM hands over raw out pointers")]
impl IMFSampleAllocatorControl_Impl for Source_Impl {
    fn SetDefaultAllocator(&self, id: u32, allocator: Ref<'_, IUnknown>) -> Result<()> {
        guard(|| {
            self.alive()?;
            if id != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            self.stream.set_allocator(allocator.ok()?.cast()?)
        })
    }

    fn GetAllocatorUsage(
        &self,
        id: u32,
        input: *mut u32,
        usage: *mut MFSampleAllocatorUsage,
    ) -> Result<()> {
        guard(|| {
            self.alive()?;
            if input.is_null() || usage.is_null() {
                return Err(E_POINTER.into());
            }
            if id != 0 {
                return Err(MF_E_INVALIDSTREAMNUMBER.into());
            }
            // SAFETY: both checked for null, and COM says they point at writable storage.
            unsafe {
                *input = id;
                *usage = MFSampleAllocatorUsage_UsesProvidedAllocator;
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use windows::Win32::Media::MediaFoundation::{
        IMFMediaSource, IMFMediaStream, IMFMediaStream2, MF_VERSION, MFSTARTUP_FULL, MFStartup,
    };
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx};

    #[test]
    #[expect(unsafe_code, reason = "Media Foundation is a COM API")]
    fn a_source_frame_server_never_shuts_down_is_still_freed() {
        // SAFETY: called on the test thread before any other COM or Media Foundation call.
        unsafe {
            CoInitializeEx(None, COINIT_MULTITHREADED).ok().unwrap();
            MFStartup(MF_VERSION, MFSTARTUP_FULL).unwrap();
        }
        let before = crate::LIVE.load(Ordering::Acquire);
        let source = Source::new(&new_attributes().unwrap()).unwrap();
        let stream: IMFMediaStream = source.stream.to_interface::<IMFMediaStream2>().into();
        // SAFETY: a plain COM call on a live stream.
        let back = unsafe { stream.GetMediaSource() }.unwrap();
        let ours: IMFMediaSource = source.to_interface::<IMFMediaSourceEx>().into();
        assert_eq!(back, ours);
        drop(ours);
        drop((back, stream, source));
        assert_eq!(crate::LIVE.load(Ordering::Acquire), before);
    }
}
