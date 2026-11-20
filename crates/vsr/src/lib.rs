//! Safe binding to NVIDIA's Video Effects SDK. The declarations follow `nvVideoEffects.h`,
//! `nvCVImage.h` and `nvCVStatus.h` from github.com/NVIDIA-Maxine/Maxine-VFX-SDK, and the DLLs
//! come from the `nvidia-vfx` wheel.

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;
use std::ptr::null_mut;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LOAD_LIBRARY_SEARCH_DEFAULT_DIRS, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR,
    LoadLibraryExW,
};
use windows::core::{HSTRING, PCSTR};

/// What the effect does to each frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Settings {
    /// `QualityLevel`: 0 bicubic, 1 to 4 AI upscaling, 8 to 11 denoise, 12 to 15 deblur, 16 to 19 clean source, 21 and 23 streaming.
    pub quality: u32,
    /// `Strength` from 0 to 1.
    pub strength: f32,
    /// 8 to 11 for a pass at the input size before upscaling, since denoise refuses to change
    /// the size, or 0 for none. Turning it on or off needs a new `SuperRes`.
    pub denoise: u32,
}

/// One VideoSuperRes effect with its CUDA stream and GPU buffers, sized once.
#[derive(Debug)]
pub struct SuperRes {
    api: Api,
    effect: Handle,
    stream: Stream,
    /// Effect input, RGBA u8 on the GPU. Boxed because `NvVFX_SetImage` keeps the pointer rather
    /// than the shallow copy nvVideoEffects.h promises, so the descriptor must not move.
    src: Box<Image>,
    /// Boxed for the same reason.
    dst: Box<Image>,
    /// Null when denoise is off. Otherwise the upscaler reads `clean` instead of `src`.
    denoise: Handle,
    clean: Box<Image>,
    /// The output converted to NV12 on the GPU, so only 1.5 bytes a pixel come back over `PCIe`.
    nv12: Image,
    /// Staging that `NvCVImage_Transfer` grows once for the RGB24 upload.
    tmp: Image,
    input: (u32, u32),
    output: (u32, u32),
    /// What the loaded models were built for. The SDK only reads `QualityLevel` in
    /// `NvVFX_Load`, so a new level means loading again.
    loaded: Option<Settings>,
}

#[expect(unsafe_code, reason = "the SDK's raw handles aren't Send on their own")]
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the handles belong to CUDA's primary context, which any thread can use"
)]
// SAFETY: the effect and stream belong to the CUDA runtime's primary context, which any thread
// can use, and `&mut self` on every call keeps two threads from using them at once.
unsafe impl Send for SuperRes {}

impl SuperRes {
    /// Loads the SDK from `dir`, the folder holding NVIDIA's DLLs.
    /// # Errors
    /// When a size is unusable, a DLL won't load or the SDK refuses a step.
    #[expect(unsafe_code, reason = "every step is a call into NVIDIA's C API")]
    pub fn new(
        dir: &Path,
        input: (u32, u32),
        output: (u32, u32),
        settings: Settings,
    ) -> Result<Self, String> {
        if input.0 == 0 || input.1 == 0 {
            return Err("the input frame is empty".into());
        }
        if !output.0.is_multiple_of(2) || !output.1.is_multiple_of(2) {
            return Err(format!(
                "NV12 needs even sides, got {}x{}",
                output.0, output.1
            ));
        }
        let mut e = Self {
            api: Api::load(dir, "nvVFXVideoSuperRes.dll")?,
            effect: null_mut(),
            stream: null_mut(),
            src: Box::new(Image::empty()),
            dst: Box::new(Image::empty()),
            denoise: null_mut(),
            clean: Box::new(Image::empty()),
            nv12: Image::empty(),
            tmp: Image::empty(),
            input,
            output,
            loaded: None,
        };
        let a = &e.api;
        // SAFETY: every pointer is a live field of `e`, and Drop releases what succeeded.
        unsafe {
            a.check(
                (a.create_effect)(c"VideoSuperRes".as_ptr(), &raw mut e.effect),
                "NvVFX_CreateEffect",
            )?;
            a.check(
                (a.stream_create)(&raw mut e.stream),
                "NvVFX_CudaStreamCreate",
            )?;
            a.check(
                (a.set_u32)(e.effect, c"ImageEncodingMode".as_ptr(), 0),
                "NvVFX_SetU32(ImageEncodingMode)",
            )?;
            a.alloc_rgba(&raw mut *e.src, input)?;
            a.alloc_rgba(&raw mut *e.dst, output)?;
            a.alloc_nv12(&raw mut e.nv12, output)?;
        }
        let upscale_from = if settings.denoise > 0 {
            e.add_denoise()?
        } else {
            &raw mut *e.src
        };
        let a = &e.api;
        // SAFETY: as above, and `upscale_from` points into a box `e` owns.
        unsafe {
            a.check(
                (a.set_image)(e.effect, c"SrcImage0".as_ptr(), upscale_from),
                "NvVFX_SetImage(SrcImage0)",
            )?;
            a.check(
                (a.set_image)(e.effect, c"DstImage0".as_ptr(), &raw mut *e.dst),
                "NvVFX_SetImage(DstImage0)",
            )?;
            a.check(
                (a.set_cuda_stream)(e.effect, c"CudaStream".as_ptr(), e.stream),
                "NvVFX_SetCudaStream",
            )?;
        }
        e.set(settings)?;
        // SAFETY: the effect is configured and its images outlive it.
        unsafe {
            e.api.check((e.api.load)(e.effect), "NvVFX_Load")?;
            if !e.denoise.is_null() {
                e.api
                    .check((e.api.load)(e.denoise), "NvVFX_Load(denoise)")?;
            }
        }
        e.loaded = Some(settings);
        Ok(e)
    }

    #[expect(unsafe_code, reason = "every step is a call into NVIDIA's C API")]
    fn add_denoise(&mut self) -> Result<*mut Image, String> {
        let a = &self.api;
        // SAFETY: every pointer is a live field of `self`, and Drop releases what succeeded.
        unsafe {
            a.check(
                (a.create_effect)(c"VideoSuperRes".as_ptr(), &raw mut self.denoise),
                "NvVFX_CreateEffect(denoise)",
            )?;
            a.check(
                (a.set_u32)(self.denoise, c"ImageEncodingMode".as_ptr(), 0),
                "NvVFX_SetU32(ImageEncodingMode)",
            )?;
            a.alloc_rgba(&raw mut *self.clean, self.input)?;
            a.check(
                (a.set_image)(self.denoise, c"SrcImage0".as_ptr(), &raw mut *self.src),
                "NvVFX_SetImage(SrcImage0)",
            )?;
            a.check(
                (a.set_image)(self.denoise, c"DstImage0".as_ptr(), &raw mut *self.clean),
                "NvVFX_SetImage(DstImage0)",
            )?;
            a.check(
                (a.set_cuda_stream)(self.denoise, c"CudaStream".as_ptr(), self.stream),
                "NvVFX_SetCudaStream",
            )?;
        }
        Ok(&raw mut *self.clean)
    }

    /// Upscales RGB24 rows `stride` bytes apart into limited range BT.709 NV12.
    /// # Errors
    /// When a buffer is the wrong size, or the SDK or CUDA reports an error.
    #[expect(unsafe_code, reason = "the frame is handed to NVIDIA's C API")]
    pub fn run(&mut self, rgb: &[u8], stride: usize, nv12: &mut [u8]) -> Result<(), String> {
        let pitch = rgb_pitch(rgb.len(), stride, self.input)?;
        self.check_output(nv12)?;
        let a = &self.api;
        let mut cpu_in = Image::empty();
        // SAFETY: `rgb_pitch` checked the slice, and an upload from pageable memory is synchronous.
        unsafe {
            let pixels = rgb.as_ptr().cast_mut().cast();
            a.wrap_rgb(&raw mut cpu_in, self.input, pitch, pixels)?;
            let status = (a.transfer)(
                &raw const cpu_in,
                &raw mut *self.src,
                1.0,
                self.stream,
                &raw mut self.tmp,
            );
            a.check(status, "NvCVImage_Transfer(upload)")?;
        }
        self.finish(nv12)
    }

    fn check_output(&self, nv12: &[u8]) -> Result<(), String> {
        let (ow, oh) = (self.output.0 as usize, self.output.1 as usize);
        let needs = (ow * oh * 3).div_euclid(2);
        if nv12.len() != needs {
            return Err(format!(
                "NV12 buffer is {} bytes, {ow}x{oh} needs {needs}",
                nv12.len(),
            ));
        }
        Ok(())
    }

    #[expect(unsafe_code, reason = "the frame comes back through NVIDIA's C API")]
    fn finish(&mut self, nv12: &mut [u8]) -> Result<(), String> {
        let (ow, oh) = self.output;
        let pitch = i32::try_from(ow).map_err(|_| format!("width {ow} is too big"))?;
        let a = &self.api;
        let mut cpu_out = Image::empty();
        // SAFETY: `check_output` confirmed `nv12`, and the synchronize keeps it borrowed.
        unsafe {
            if !self.denoise.is_null() {
                a.check((a.run)(self.denoise, 1), "NvVFX_Run(denoise)")?;
            }
            a.check((a.run)(self.effect, 1), "NvVFX_Run")?;
            let status = (a.transfer)(
                &raw const *self.dst,
                &raw mut self.nv12,
                1.0,
                self.stream,
                null_mut(),
            );
            a.check(status, "NvCVImage_Transfer(RGB to NV12)")?;

            let pixels = nv12.as_mut_ptr().cast();
            let status = (a.init)(
                &raw mut cpu_out,
                ow,
                oh,
                pitch,
                pixels,
                NVCV_YUV420,
                NVCV_U8,
                NVCV_NV12,
                NVCV_CPU,
            );
            a.check(status, "NvCVImage_Init")?;
            cpu_out.colorspace = NV12_COLORSPACE;
            let status = (a.transfer)(
                &raw const self.nv12,
                &raw mut cpu_out,
                1.0,
                self.stream,
                null_mut(),
            );
            a.check(status, "NvCVImage_Transfer(download)")?;
            a.sync(self.stream)
        }
    }

    /// Takes effect on the next frame.
    /// # Errors
    /// When the SDK refuses a setting.
    #[expect(unsafe_code, reason = "settings go through NVIDIA's C API")]
    pub fn set(&mut self, settings: Settings) -> Result<(), String> {
        let a = &self.api;
        // SAFETY: the effect handle is live and the selectors are NUL-terminated.
        unsafe {
            a.check(
                (a.set_u32)(self.effect, c"QualityLevel".as_ptr(), settings.quality),
                "NvVFX_SetU32(QualityLevel)",
            )?;
            a.check(
                (a.set_f32)(self.effect, c"Strength".as_ptr(), settings.strength),
                "NvVFX_SetF32(Strength)",
            )?;
            if !self.denoise.is_null() && settings.denoise > 0 {
                a.check(
                    (a.set_u32)(self.denoise, c"QualityLevel".as_ptr(), settings.denoise),
                    "NvVFX_SetU32(QualityLevel)",
                )?;
            }
            if let Some(old) = self.loaded {
                if old.quality != settings.quality {
                    a.check((a.load)(self.effect), "NvVFX_Load")?;
                }
                if !self.denoise.is_null()
                    && settings.denoise > 0
                    && old.denoise != settings.denoise
                {
                    a.check((a.load)(self.denoise), "NvVFX_Load(denoise)")?;
                }
                self.loaded = Some(settings);
            }
            Ok(())
        }
    }
}

impl Drop for SuperRes {
    #[expect(
        unsafe_code,
        reason = "the SDK's resources are freed through its C API"
    )]
    fn drop(&mut self) {
        let a = &self.api;
        // SAFETY: each resource is released once, the effect first since it points at the images.
        unsafe {
            if !self.effect.is_null() {
                (a.destroy_effect)(self.effect);
            }
            if !self.denoise.is_null() {
                (a.destroy_effect)(self.denoise);
            }
            for im in [
                &mut *self.src,
                &mut *self.dst,
                &mut *self.clean,
                &mut self.nv12,
                &mut self.tmp,
            ] {
                (a.dealloc)(im);
            }
            if !self.stream.is_null() {
                (a.stream_destroy)(self.stream);
            }
        }
    }
}

/// Checks that `len` bytes hold a frame of `size` as RGB24 rows `stride` bytes apart, and returns
/// the stride as the SDK's pitch.
fn rgb_pitch(len: usize, stride: usize, size: (u32, u32)) -> Result<i32, String> {
    let (w, h) = (size.0 as usize, size.1 as usize);
    if stride < w * 3 || len < stride * (h - 1) + w * 3 {
        return Err(format!(
            "RGB buffer of {len} bytes with stride {stride} is too small for {w}x{h}"
        ));
    }
    i32::try_from(stride).map_err(|_| format!("stride {stride} is too big"))
}

type Handle = *mut c_void;

const CUDA_DEVICE_SCHEDULE_BLOCKING_SYNC: u32 = 0x04;
type Stream = *mut c_void;

const NVCV_RGB: i32 = 4;
const NVCV_RGBA: i32 = 6;
const NVCV_YUV420: i32 = 10;
const NVCV_U8: i32 = 1;
const NVCV_CHUNKY: u32 = 0;
const NVCV_NV12: u32 = 7;
const NVCV_CPU: u32 = 0;
const NVCV_GPU: u32 = 1;
const NVCV_709: u8 = 0x01;
const NVCV_VIDEO_RANGE: u8 = 0x00;
const NVCV_CHROMA_COSITED: u8 = 0x00;

/// Media Foundation reads untagged NV12 as BT.709 video range with MPEG-2 chroma siting, which
/// `NvCV` calls cosited. JPEG RGB and BT.709 share primaries and both carry gamma-encoded values,
/// so the matrix is all that changes.
const NV12_COLORSPACE: u8 = NVCV_709 | NVCV_VIDEO_RANGE | NVCV_CHROMA_COSITED;

/// `NvCVImage` from nvCVImage.h.
#[repr(C)]
#[derive(Debug)]
struct Image {
    width: u32,
    height: u32,
    pitch: i32,
    pixel_format: i32,
    component_type: i32,
    pixel_bytes: u8,
    component_bytes: u8,
    num_components: u8,
    planar: u8,
    gpu_mem: u8,
    colorspace: u8,
    reserved: [u8; 2],
    pixels: *mut c_void,
    delete_ptr: *mut c_void,
    delete_proc: Option<unsafe extern "C" fn(*mut c_void)>,
    buffer_bytes: u64,
}

const _: () = assert!(size_of::<Image>() == 64);

impl Image {
    #[expect(unsafe_code, reason = "zeroing a C struct")]
    const fn empty() -> Self {
        // SAFETY: all zero is valid for every field, and is how the SDK spells an empty image.
        unsafe { std::mem::zeroed() }
    }
}

/// DLLs loaded up front, in dependency order, before the effect's own DLL. `NVVideoEffects` finds
/// effects only among loaded modules, while `nvngxruntime.dll` and `nvngx_vsr.dll` load from the
/// same folder on their own. The NPP libraries in the wheel serve other effects.
const DLLS: [&str; 3] = ["cudart64_12.dll", "NVCVImage.dll", "NVVideoEffects.dll"];

#[derive(Debug)]
struct Api {
    create_effect: unsafe extern "C" fn(*const c_char, *mut Handle) -> i32,
    destroy_effect: unsafe extern "C" fn(Handle),
    set_u32: unsafe extern "C" fn(Handle, *const c_char, u32) -> i32,
    set_f32: unsafe extern "C" fn(Handle, *const c_char, f32) -> i32,
    set_image: unsafe extern "C" fn(Handle, *const c_char, *mut Image) -> i32,
    set_cuda_stream: unsafe extern "C" fn(Handle, *const c_char, Stream) -> i32,
    load: unsafe extern "C" fn(Handle) -> i32,
    run: unsafe extern "C" fn(Handle, i32) -> i32,
    stream_create: unsafe extern "C" fn(*mut Stream) -> i32,
    stream_destroy: unsafe extern "C" fn(Stream) -> i32,
    init: unsafe extern "C" fn(*mut Image, u32, u32, i32, *mut c_void, i32, i32, u32, u32) -> i32,
    alloc: unsafe extern "C" fn(*mut Image, u32, u32, i32, i32, u32, u32, u32) -> i32,
    dealloc: unsafe extern "C" fn(*mut Image),
    transfer: unsafe extern "C" fn(*const Image, *mut Image, f32, Stream, *mut Image) -> i32,
    error_string: unsafe extern "C" fn(i32) -> *const c_char,
    stream_synchronize: unsafe extern "C" fn(Stream) -> i32,
}

impl Api {
    #[expect(
        unsafe_code,
        reason = "loading DLLs and their exports is all raw Win32"
    )]
    fn load(dir: &Path, effect: &str) -> Result<Self, String> {
        let mut libs = [HMODULE::default(); DLLS.len() + 1];
        for (lib, name) in libs.iter_mut().zip(DLLS.into_iter().chain([effect])) {
            let path = dir.join(name);
            // The libraries are never freed. CUDA and NGX keep threads and state that do not survive
            // an unload, and loading the same path again only bumps the reference count.
            // SAFETY: plain LoadLibraryExW. The flags resolve imports from the DLL's folder first.
            *lib = unsafe {
                LoadLibraryExW(
                    &HSTRING::from(path.as_os_str()),
                    None,
                    LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_DEFAULT_DIRS,
                )
            }
            .map_err(|e| format!("could not load {}: {e}", path.display()))?;
        }
        let [cudart, cvimage, vfx, _] = libs;
        // CUDA spins a core while it waits for the GPU unless told to sleep, which cost 12 ms of
        // CPU per frame. It only takes before the context exists, so later calls fail harmlessly.
        // SAFETY: cudaSetDeviceFlags(unsigned int) from cuda_runtime_api.h.
        unsafe {
            if let Some(f) = GetProcAddress(cudart, PCSTR(c"cudaSetDeviceFlags".as_ptr().cast())) {
                let f: unsafe extern "C" fn(u32) -> i32 = std::mem::transmute(f);
                f(CUDA_DEVICE_SCHEDULE_BLOCKING_SYNC);
            }
        }
        macro_rules! sym {
            ($lib:expr, $name:literal) => {{
                // SAFETY: the signature on the field matches the SDK and CUDA headers.
                let f = unsafe { GetProcAddress($lib, PCSTR(concat!($name, "\0").as_ptr())) }
                    .ok_or(concat!($name, " is missing from its DLL"))?;
                #[expect(
                    clippy::missing_transmute_annotations,
                    reason = "the Api field gives the type"
                )]
                // SAFETY: a function pointer to a function pointer of the header's signature.
                let f = unsafe { std::mem::transmute(f) };
                f
            }};
        }
        Ok(Self {
            create_effect: sym!(vfx, "NvVFX_CreateEffect"),
            destroy_effect: sym!(vfx, "NvVFX_DestroyEffect"),
            set_u32: sym!(vfx, "NvVFX_SetU32"),
            set_f32: sym!(vfx, "NvVFX_SetF32"),
            set_image: sym!(vfx, "NvVFX_SetImage"),
            set_cuda_stream: sym!(vfx, "NvVFX_SetCudaStream"),
            load: sym!(vfx, "NvVFX_Load"),
            run: sym!(vfx, "NvVFX_Run"),
            stream_create: sym!(vfx, "NvVFX_CudaStreamCreate"),
            stream_destroy: sym!(vfx, "NvVFX_CudaStreamDestroy"),
            init: sym!(cvimage, "NvCVImage_Init"),
            alloc: sym!(cvimage, "NvCVImage_Alloc"),
            dealloc: sym!(cvimage, "NvCVImage_Dealloc"),
            transfer: sym!(cvimage, "NvCVImage_Transfer"),
            error_string: sym!(cvimage, "NvCV_GetErrorStringFromCode"),
            stream_synchronize: sym!(cudart, "cudaStreamSynchronize"),
        })
    }

    /// RGBA U8 chunky is the one layout the effects take, whatever the Python wrapper's float32
    /// interface suggests.
    /// # Safety
    /// `im` must point at a live image that is empty or was allocated by the SDK.
    #[expect(unsafe_code, reason = "NvCVImage_Alloc writes through a raw pointer")]
    unsafe fn alloc_rgba(&self, im: *mut Image, (w, h): (u32, u32)) -> Result<(), String> {
        // SAFETY: the caller vouches for `im`.
        let status =
            unsafe { (self.alloc)(im, w, h, NVCV_RGBA, NVCV_U8, NVCV_CHUNKY, NVCV_GPU, 32) };
        self.check(status, "NvCVImage_Alloc")
    }

    /// # Safety
    /// `im` must point at a live image that is empty or was allocated by the SDK.
    #[expect(unsafe_code, reason = "NvCVImage_Alloc writes through a raw pointer")]
    unsafe fn alloc_nv12(&self, im: *mut Image, (w, h): (u32, u32)) -> Result<(), String> {
        // SAFETY: the caller vouches for `im`.
        let status =
            unsafe { (self.alloc)(im, w, h, NVCV_YUV420, NVCV_U8, NVCV_NV12, NVCV_GPU, 0) };
        self.check(status, "NvCVImage_Alloc")?;
        // Transfer reads the colourspace only from this field, which Alloc leaves at 0.
        // SAFETY: Alloc succeeded on the image the caller vouched for.
        unsafe { (*im).colorspace = NV12_COLORSPACE };
        Ok(())
    }

    /// # Safety
    /// `im` must point at a live empty image, and `pixels` at `size` rows `pitch` bytes apart.
    #[expect(unsafe_code, reason = "NvCVImage_Init writes through a raw pointer")]
    unsafe fn wrap_rgb(
        &self,
        im: *mut Image,
        (w, h): (u32, u32),
        pitch: i32,
        pixels: *mut c_void,
    ) -> Result<(), String> {
        // SAFETY: the caller vouches for `im` and `pixels`.
        let status = unsafe {
            (self.init)(
                im,
                w,
                h,
                pitch,
                pixels,
                NVCV_RGB,
                NVCV_U8,
                NVCV_CHUNKY,
                NVCV_CPU,
            )
        };
        self.check(status, "NvCVImage_Init")
    }

    /// # Safety
    /// `stream` must be a live stream from `NvVFX_CudaStreamCreate`.
    #[expect(unsafe_code, reason = "cudaStreamSynchronize is a C call")]
    unsafe fn sync(&self, stream: Stream) -> Result<(), String> {
        // SAFETY: the caller vouches for `stream`.
        match unsafe { (self.stream_synchronize)(stream) } {
            0 => Ok(()),
            err => Err(format!(
                "cudaStreamSynchronize failed with CUDA error {err}"
            )),
        }
    }

    #[expect(unsafe_code, reason = "the SDK's error text comes back as a C string")]
    fn check(&self, status: i32, call: &str) -> Result<(), String> {
        if status == 0 {
            return Ok(());
        }
        // SAFETY: the SDK returns a static string, or null for codes it does not know.
        let text = unsafe { (self.error_string)(status) };
        let text = if text.is_null() {
            "unknown status".into()
        } else {
            // SAFETY: not null, and the SDK's strings are static and NUL-terminated.
            unsafe { CStr::from_ptr(text) }.to_string_lossy()
        };
        Err(format!("{call} failed: {text} (NvCV status {status})"))
    }
}
