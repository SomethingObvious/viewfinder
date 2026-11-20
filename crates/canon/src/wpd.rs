//! PTP operations through the stock WPD driver's MTP extension, shared with other brands.

use std::ffi::c_void;
use std::ptr::null_mut;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Devices::PortableDevices::{
    IPortableDevice, IPortableDeviceManager, IPortableDevicePropVariantCollection,
    IPortableDeviceValues, PortableDeviceFTM, PortableDeviceManager,
    PortableDevicePropVariantCollection, PortableDeviceValues, WPD_CLIENT_MAJOR_VERSION,
    WPD_CLIENT_MINOR_VERSION, WPD_CLIENT_NAME, WPD_CLIENT_REVISION,
    WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER, WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ,
    WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_WRITE,
    WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITHOUT_DATA_PHASE, WPD_COMMAND_MTP_EXT_READ_DATA,
    WPD_COMMAND_MTP_EXT_WRITE_DATA, WPD_PROPERTY_COMMON_COMMAND_CATEGORY,
    WPD_PROPERTY_COMMON_COMMAND_ID, WPD_PROPERTY_COMMON_HRESULT,
    WPD_PROPERTY_MTP_EXT_OPERATION_CODE, WPD_PROPERTY_MTP_EXT_OPERATION_PARAMS,
    WPD_PROPERTY_MTP_EXT_OPTIMAL_TRANSFER_BUFFER_SIZE, WPD_PROPERTY_MTP_EXT_RESPONSE_CODE,
    WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, WPD_PROPERTY_MTP_EXT_TRANSFER_DATA,
    WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_READ, WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_READ,
    WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_WRITE,
    WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE,
};
use windows::Win32::Foundation::{E_UNEXPECTED, PROPERTYKEY};
use windows::Win32::System::Com::StructuredStorage::PROPVARIANT;
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Variant::VT_UI4;
use windows_core::{GUID, HSTRING, Interface, PCWSTR, PWSTR, Result};

use crate::ptp::OP_GET_VIEWFINDER_DATA;

const VIEWFINDER_READ: usize = 2 << 20;
/// Nothing real comes near this, so a bigger announced size is a broken reply.
const MAX_READ: u64 = 32 << 20;
const DEFAULT_CHUNK: u32 = 256 << 10;

#[derive(Debug)]
pub(crate) enum Pull {
    Data,
    Empty,
    Code(u16),
}

/// A device opened through WPD.
#[derive(Debug)]
pub struct Wpd {
    pub(crate) device: IPortableDevice,
    pnp_id: HSTRING,
    client: IPortableDeviceValues,
    landing: Vec<u8>,
}

#[derive(Debug)]
struct CoTask(*mut c_void);

impl CoTask {
    const fn pcwstr(&self) -> PCWSTR {
        PCWSTR(self.0.cast())
    }
}

impl Drop for CoTask {
    #[expect(unsafe_code, reason = "freeing COM task memory is a Win32 call")]
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: WPD allocated it with CoTaskMemAlloc, and this is the only owner.
            unsafe { CoTaskMemFree(Some(self.0)) };
        }
    }
}

#[expect(unsafe_code, reason = "creating a COM object is a Win32 call")]
fn create<T: Interface>(clsid: &GUID) -> Result<T> {
    // SAFETY: `clsid` is a live GUID, and the class comes from WPD's own headers.
    unsafe { CoCreateInstance(clsid, None, CLSCTX_INPROC_SERVER) }
}

/// `PnP` ids of every device WPD can see.
/// # Errors
/// When the WPD device manager won't start.
#[expect(unsafe_code, reason = "the device manager is a COM object")]
pub fn device_ids() -> Result<Vec<String>> {
    let manager: IPortableDeviceManager = create(&PortableDeviceManager)?;
    // The manager keeps the list from when it was first asked, missing a camera turned on since.
    // SAFETY: the manager is live.
    unsafe { manager.RefreshDeviceList()? };
    let mut n = 0;
    // SAFETY: a null array with a live count asks for the count only.
    unsafe { manager.GetDevices(null_mut(), &raw mut n)? };
    let mut ids = vec![PWSTR::null(); n as usize];
    // SAFETY: `ids` holds `n` slots, which the call won't go past.
    unsafe { manager.GetDevices(ids.as_mut_ptr(), &raw mut n)? };
    Ok(ids
        .into_iter()
        .take(n as usize)
        .filter(|p| !p.is_null())
        .map(|p| {
            let owned = CoTask(p.0.cast());
            // SAFETY: not null, and WPD hands back NUL-terminated strings.
            let s = unsafe { p.to_string() }.unwrap_or_default();
            drop(owned);
            s
        })
        .collect())
}

/// The name Windows shows for a WPD device, such as "Canon EOS REBEL T3".
/// # Errors
/// When the device manager won't start or the device has gone.
#[expect(unsafe_code, reason = "the device manager is a COM object")]
pub fn friendly_name(pnp_id: &str) -> Result<String> {
    let manager: IPortableDeviceManager = create(&PortableDeviceManager)?;
    let id = HSTRING::from(pnp_id);
    let mut n = 0;
    // SAFETY: a null buffer with a live count asks for the length only.
    unsafe { manager.GetDeviceFriendlyName(&id, PWSTR::null(), &raw mut n)? };
    let mut name = vec![0_u16; n as usize];
    // SAFETY: `name` holds `n` units, which the call won't go past.
    unsafe { manager.GetDeviceFriendlyName(&id, PWSTR(name.as_mut_ptr()), &raw mut n)? };
    Ok(String::from_utf16_lossy(
        name.split(|&u| u == 0).next().unwrap_or_default(),
    ))
}

/// The device's `WPD_DEVICE_TYPES` value, 1 for a camera and 3 for a phone, if its driver says.
#[must_use]
#[expect(unsafe_code, reason = "the device manager is a COM object")]
pub fn device_type(pnp_id: &str) -> Option<u32> {
    let manager: IPortableDeviceManager = create(&PortableDeviceManager).ok()?;
    let mut kind = 0_u32;
    let mut size = 4_u32;
    let mut format = 0_u32;
    // SAFETY: `kind` is a live u32 and `size` says so, so the DWORD fits.
    unsafe {
        manager.GetDeviceProperty(
            &HSTRING::from(pnp_id),
            &HSTRING::from("PortableDeviceType"),
            (&raw mut kind).cast(),
            &raw mut size,
            &raw mut format,
        )
    }
    .ok()?;
    Some(kind)
}

#[expect(unsafe_code, reason = "the value store is a COM object")]
fn client_info() -> Result<IPortableDeviceValues> {
    let v: IPortableDeviceValues = create(&PortableDeviceValues)?;
    // SAFETY: the keys and the string are statics.
    unsafe {
        v.SetStringValue(&WPD_CLIENT_NAME, windows_core::w!("viewfinder"))?;
        v.SetUnsignedIntegerValue(&WPD_CLIENT_MAJOR_VERSION, 1)?;
        v.SetUnsignedIntegerValue(&WPD_CLIENT_MINOR_VERSION, 0)?;
        v.SetUnsignedIntegerValue(&WPD_CLIENT_REVISION, 0)?;
    }
    Ok(v)
}

#[expect(unsafe_code, reason = "opening a device is a COM call")]
fn open_device(pnp_id: &HSTRING, client: &IPortableDeviceValues) -> Result<IPortableDevice> {
    let device: IPortableDevice = create(&PortableDeviceFTM)?;
    // SAFETY: both arguments are live.
    unsafe { device.Open(pnp_id, client)? };
    Ok(device)
}

#[expect(unsafe_code, reason = "the value store is a COM object")]
fn command(cmd: &PROPERTYKEY) -> Result<IPortableDeviceValues> {
    let v: IPortableDeviceValues = create(&PortableDeviceValues)?;
    // SAFETY: the keys are statics and `cmd` outlives the calls.
    unsafe {
        v.SetGuidValue(&WPD_PROPERTY_COMMON_COMMAND_CATEGORY, &raw const cmd.fmtid)?;
        v.SetUnsignedIntegerValue(&WPD_PROPERTY_COMMON_COMMAND_ID, cmd.pid)?;
    }
    Ok(v)
}

#[expect(unsafe_code, reason = "PROPVARIANT is a C union")]
fn ui4(value: u32) -> PROPVARIANT {
    let mut pv = PROPVARIANT::default();
    // SAFETY: the union is empty, and setting the tag with the matching field makes a valid VT_UI4.
    unsafe {
        let inner = &mut *pv.Anonymous.Anonymous;
        inner.vt = VT_UI4;
        inner.Anonymous.ulVal = value;
    }
    pv
}

#[expect(unsafe_code, reason = "the value stores are COM objects")]
fn operation(cmd: &PROPERTYKEY, op: u16, params: &[u32]) -> Result<IPortableDeviceValues> {
    let v = command(cmd)?;
    let list: IPortableDevicePropVariantCollection = create(&PortableDevicePropVariantCollection)?;
    for &p in params {
        // SAFETY: the collection copies the value.
        unsafe { list.Add(&ui4(p))? };
    }
    // SAFETY: the keys are statics and the collection is live.
    unsafe {
        v.SetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_OPERATION_CODE, op.into())?;
        // Vendor reads fail with 0x80070490 unless this collection is present, even empty.
        v.SetIPortableDevicePropVariantCollectionValue(
            &WPD_PROPERTY_MTP_EXT_OPERATION_PARAMS,
            &list,
        )?;
    }
    Ok(v)
}

/// The unanswered operation, with when it went in ms since `EPOCH` in the high bits.
static FLIGHT: AtomicU64 = AtomicU64::new(0);
static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

struct Flight;

impl Flight {
    fn start(op: u16) -> Self {
        let ms = EPOCH.elapsed().as_millis() as u64;
        FLIGHT.store((ms.max(1) << 16) | u64::from(op), Ordering::Relaxed);
        Self
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        FLIGHT.store(0, Ordering::Relaxed);
    }
}

/// The operation the camera hasn't answered yet, and how long it's waited.
#[must_use]
pub fn waiting() -> Option<(u16, Duration)> {
    let flight = FLIGHT.load(Ordering::Relaxed);
    (flight != 0).then(|| {
        let since = Duration::from_millis(flight >> 16);
        (
            (flight & 0xFFFF) as u16,
            EPOCH.elapsed().saturating_sub(since),
        )
    })
}

#[expect(unsafe_code, reason = "sending a command is a COM call")]
fn send(device: &IPortableDevice, cmd: &IPortableDeviceValues) -> Result<IPortableDeviceValues> {
    // SAFETY: both are live.
    let results = unsafe { device.SendCommand(0, cmd)? };
    // SAFETY: the key is a static.
    if let Ok(hr) = unsafe { results.GetErrorValue(&WPD_PROPERTY_COMMON_HRESULT) } {
        hr.ok()?;
    }
    Ok(results)
}

#[expect(unsafe_code, reason = "reading a result is a COM call")]
fn response_code(results: &IPortableDeviceValues) -> Result<u16> {
    // SAFETY: the key is a static.
    let code = unsafe { results.GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_RESPONSE_CODE)? };
    Ok(code as u16)
}

#[expect(unsafe_code, reason = "the context comes back as a raw COM string")]
fn context(results: &IPortableDeviceValues) -> Option<CoTask> {
    // SAFETY: the key is a static.
    let s = unsafe { results.GetStringValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT) }.ok()?;
    let ctx = CoTask(s.0.cast());
    // SAFETY: only read when not null.
    (!s.is_null() && unsafe { *s.0 } != 0).then_some(ctx)
}

#[expect(unsafe_code, reason = "reading a result is a COM call")]
fn total_size(results: &IPortableDeviceValues) -> u64 {
    let key = &WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE;
    // SAFETY: the key is a static.
    unsafe { results.GetUnsignedLargeIntegerValue(key) }
        // SAFETY: as above.
        .or_else(|_| unsafe { results.GetUnsignedIntegerValue(key) }.map(u64::from))
        .unwrap_or(0)
}

fn landing(buf: &mut Vec<u8>, want: usize) -> &[u8] {
    if buf.len() < want {
        buf.resize(want, 0);
    }
    buf.get(..want).unwrap_or_default()
}

#[expect(unsafe_code, reason = "the data comes back in a raw COM buffer")]
fn read_chunk(
    device: &IPortableDevice,
    ctx: &CoTask,
    landing: &[u8],
    out: &mut Vec<u8>,
) -> Result<usize> {
    let cmd = command(&WPD_COMMAND_MTP_EXT_READ_DATA)?;
    // SAFETY: the keys are statics, and the context and landing buffer outlive the calls.
    unsafe {
        cmd.SetStringValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, ctx.pcwstr())?;
        cmd.SetUnsignedIntegerValue(
            &WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_READ,
            landing.len() as u32,
        )?;
        // Documented as a result only, but the driver fails even small reads without it.
        cmd.SetBufferValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_DATA, landing)?;
    }
    let results = send(device, &cmd)?;
    let mut ptr = null_mut();
    let mut len = 0;
    // SAFETY: both out slots are live locals.
    unsafe {
        results.GetBufferValue(
            &WPD_PROPERTY_MTP_EXT_TRANSFER_DATA,
            &raw mut ptr,
            &raw mut len,
        )?;
    };
    let data = CoTask(ptr.cast());
    // SAFETY: the key is a static.
    let read =
        unsafe { results.GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_READ) }
            .map_or(len, |n| n.min(len)) as usize;
    if !data.0.is_null() && read > 0 {
        // SAFETY: the buffer holds `len` bytes, `read` is at most that, and `data` frees it later.
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(ptr, read) });
    }
    Ok(read)
}

#[expect(
    unsafe_code,
    reason = "the WPD interface pointers aren't Send on their own"
)]
#[expect(
    clippy::non_send_fields_in_send_ty,
    reason = "the device comes from PortableDeviceFTM, so its pointers work on any MTA thread"
)]
// SAFETY: the device object is created from PortableDeviceFTM, which aggregates the free-threaded
// marshaller, so its interface pointers are valid on any MTA thread.
unsafe impl Send for Wpd {}

impl Wpd {
    /// Opens the device with this `PnP` id from an MTA thread.
    /// # Errors
    /// When the device is gone or another app holds it.
    pub fn open(pnp_id: &str) -> Result<Self> {
        let pnp_id = HSTRING::from(pnp_id);
        let client = client_info()?;
        let device = open_device(&pnp_id, &client)?;
        Ok(Self {
            device,
            pnp_id,
            client,
            landing: Vec::new(),
        })
    }

    /// An operation with no data phase, returning the PTP response code.
    /// # Errors
    /// When the operation never reached the camera.
    pub fn call(&self, op: u16, params: &[u32]) -> Result<u16> {
        let _flight = Flight::start(op);
        let cmd = operation(
            &WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITHOUT_DATA_PHASE,
            op,
            params,
        )?;
        response_code(&send(&self.device, &cmd)?)
    }

    /// An operation that reads into `out`, returning the PTP response code.
    /// # Errors
    /// When the operation never reached the camera.
    #[expect(unsafe_code, reason = "reading a result is a COM call")]
    pub fn read(&mut self, op: u16, params: &[u32], out: &mut Vec<u8>) -> Result<u16> {
        let _flight = Flight::start(op);
        out.clear();
        let cmd = operation(
            &WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ,
            op,
            params,
        )?;
        let results = send(&self.device, &cmd)?;
        let Some(ctx) = context(&results) else {
            return response_code(&results);
        };
        let total = total_size(&results);
        let chunk = u64::from(
            // SAFETY: the key is a static.
            unsafe {
                results.GetUnsignedIntegerValue(&WPD_PROPERTY_MTP_EXT_OPTIMAL_TRANSFER_BUFFER_SIZE)
            }
            .unwrap_or(DEFAULT_CHUNK)
            .clamp(4096, VIEWFINDER_READ as u32),
        );
        let body = (|| -> Result<()> {
            if total > MAX_READ {
                return Err(E_UNEXPECTED.into());
            }
            while (out.len() as u64) < total {
                let want = (total - out.len() as u64).min(chunk) as usize;
                if read_chunk(&self.device, &ctx, landing(&mut self.landing, want), out)? == 0 {
                    break;
                }
            }
            Ok(())
        })();
        // An unfinished transfer makes the device refuse every later command (FC.SDK README).
        let end = self.end(&ctx);
        body?;
        end
    }

    /// An operation that sends `data`, returning the PTP response code.
    /// # Errors
    /// When the operation never reached the camera.
    #[expect(unsafe_code, reason = "the value stores are COM objects")]
    pub fn write(&self, op: u16, params: &[u32], data: &[u8]) -> Result<u16> {
        let _flight = Flight::start(op);
        let cmd = operation(
            &WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_WRITE,
            op,
            params,
        )?;
        // SAFETY: the key is a static.
        unsafe {
            cmd.SetUnsignedLargeIntegerValue(
                &WPD_PROPERTY_MTP_EXT_TRANSFER_TOTAL_DATA_SIZE,
                data.len() as u64,
            )?;
        };
        let results = send(&self.device, &cmd)?;
        let Some(ctx) = context(&results) else {
            return response_code(&results);
        };
        let body = (|| -> Result<()> {
            let cmd = command(&WPD_COMMAND_MTP_EXT_WRITE_DATA)?;
            // SAFETY: the keys are statics, and the context and data outlive the calls.
            unsafe {
                cmd.SetStringValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, ctx.pcwstr())?;
                cmd.SetUnsignedIntegerValue(
                    &WPD_PROPERTY_MTP_EXT_TRANSFER_NUM_BYTES_TO_WRITE,
                    data.len() as u32,
                )?;
                cmd.SetBufferValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_DATA, data)?;
            }
            send(&self.device, &cmd).map(drop)
        })();
        let end = self.end(&ctx);
        body?;
        end
    }

    #[expect(unsafe_code, reason = "the value store is a COM object")]
    fn end(&self, ctx: &CoTask) -> Result<u16> {
        let cmd = command(&WPD_COMMAND_MTP_EXT_END_DATA_TRANSFER)?;
        // SAFETY: the key is a static and the context outlives the call.
        unsafe { cmd.SetStringValue(&WPD_PROPERTY_MTP_EXT_TRANSFER_CONTEXT, ctx.pcwstr())? };
        response_code(&send(&self.device, &cmd)?)
    }

    /// On EOS bodies this read has no response phase, so `END_DATA_TRANSFER` never returns and
    /// skipping it poisons the device. FC.SDK's fix, measured on a 450D, is a throwaway device
    /// per frame, read short and closed without the end phase.
    #[expect(unsafe_code, reason = "closing the device is a COM call")]
    pub(crate) fn viewfinder(&mut self, params: &[u32], out: &mut Vec<u8>) -> Result<Pull> {
        let _flight = Flight::start(OP_GET_VIEWFINDER_DATA);
        out.clear();
        let device = open_device(&self.pnp_id, &self.client)?;
        let pull = (|| -> Result<Pull> {
            let cmd = operation(
                &WPD_COMMAND_MTP_EXT_EXECUTE_COMMAND_WITH_DATA_TO_READ,
                OP_GET_VIEWFINDER_DATA,
                params,
            )?;
            let results = send(&device, &cmd)?;
            let Some(ctx) = context(&results) else {
                return response_code(&results).map(Pull::Code);
            };
            let total = total_size(&results);
            if total == 0 {
                return Ok(Pull::Empty);
            }
            if total >= MAX_READ {
                return Err(E_UNEXPECTED.into());
            }
            // One byte more than announced makes the read come back short, which ends the data
            // phase. A 2 MiB landing buffer like EDSDK's made T3 pulls about 4 ms slower.
            let want = total as usize + 1;
            read_chunk(&device, &ctx, landing(&mut self.landing, want), out)?;
            Ok(if out.is_empty() {
                Pull::Empty
            } else {
                Pull::Data
            })
        })();
        // SAFETY: the throwaway device is open, and nothing uses it after this.
        unsafe { device.Close() }.ok();
        pull
    }
}
