//! Native virtual files: Explorer chooses the destination and consumes verified, lazy IStreams.
//! OLE lives on the clipboard worker's STA. No input-hook work or simulated paste keystrokes.
#![allow(non_snake_case)] // COM interface method names.
use super::paste::{Fetch, RemoteFiles};
use std::{
    cell::RefCell,
    ffi::c_void,
    fs::File,
    io::{Read, Seek, SeekFrom},
    mem::{ManuallyDrop, size_of},
    ptr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use windows::{
    Win32::{
        Foundation::{
            DATA_S_SAMEFORMATETC, DV_E_DVASPECT, DV_E_DVTARGETDEVICE, DV_E_FORMATETC, DV_E_LINDEX,
            DV_E_TYMED, E_NOTIMPL, E_OUTOFMEMORY, E_POINTER, GlobalFree, OLE_E_ADVISENOTSUPPORTED,
            S_FALSE, S_OK, STG_E_ACCESSDENIED, STG_E_INVALIDFUNCTION, STG_E_READFAULT,
            STG_E_WRITEFAULT,
        },
        Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
        System::{
            Com::*,
            DataExchange::RegisterClipboardFormatW,
            Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
            Ole::{OleInitialize, OleSetClipboard, OleUninitialize, ReleaseStgMedium},
        },
        UI::{
            Shell::{
                CFSTR_FILECONTENTS, CFSTR_FILEDESCRIPTORW, CFSTR_PASTESUCCEEDED,
                CFSTR_PERFORMEDDROPEFFECT, CFSTR_PREFERREDDROPEFFECT, FD_ATTRIBUTES, FD_FILESIZE,
                FD_PROGRESSUI, FD_UNICODE, FILEDESCRIPTORW, IDataObjectAsyncCapability,
                IDataObjectAsyncCapability_Impl, SHCreateStdEnumFmtEtc,
            },
            WindowsAndMessaging::{
                DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
            },
        },
    },
    core::{BOOL, Error, HRESULT, Interface, PCWSTR, PWSTR, Ref, Result, implement},
};

// The generated Result<()> wrapper loses the S_FALSE distinction, so compare the raw HRESULT.
#[link(name = "ole32")]
unsafe extern "system" {
    #[link_name = "OleIsCurrentClipboard"]
    fn is_current_clipboard(object: *mut c_void) -> HRESULT;
}
thread_local! { static OLE: RefCell<Option<Clipboard>> = const { RefCell::new(None) }; }
struct Clipboard {
    object: Option<IDataObject>,
}
impl Clipboard {
    fn owns(&self) -> bool {
        self.object
            .as_ref()
            .is_some_and(|object| unsafe { is_current_clipboard(object.as_raw()) == S_OK })
    }
    fn clear(&mut self) -> Result<()> {
        if self.owns() {
            unsafe {
                OleSetClipboard(None::<&IDataObject>)?;
            }
        }
        self.object = None;
        Ok(())
    }
}
impl Drop for Clipboard {
    fn drop(&mut self) {
        let _ = self.clear();
        // Flushing would eagerly download the whole clipboard. Release instead.
        self.object = None;
        unsafe {
            OleUninitialize();
        }
    }
}
pub(crate) fn pump() {
    unsafe {
        let mut message = MSG::default();
        for _ in 0..64 {
            if !PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                break;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
pub(super) fn maintain(active: bool) -> anyhow::Result<()> {
    OLE.with(|state| -> Result<()> {
        let mut state = state.borrow_mut();
        if state.is_none() {
            unsafe {
                OleInitialize(None)?;
            }
            *state = Some(Clipboard { object: None });
        }
        let state = state.as_mut().unwrap();
        if !active {
            state.clear()?;
        } else if !state.owns() {
            state.object = None;
        }
        Ok(())
    })?;
    // Do not hold the TLS RefCell borrow while dispatching reentrant COM calls.
    pump();
    Ok(())
}
pub(super) fn owns_files() -> bool {
    OLE.with(|state| state.borrow().as_ref().is_some_and(Clipboard::owns))
}
pub(super) fn clear_files() -> anyhow::Result<()> {
    OLE.with(|state| -> Result<()> {
        if let Some(state) = state.borrow_mut().as_mut() {
            state.clear()?;
        }
        Ok(())
    })?;
    Ok(())
}
pub(super) fn set_files(files: RemoteFiles) -> anyhow::Result<()> {
    let object = data_object(files)?;
    OLE.with(|state| -> Result<()> {
        let mut state = state.borrow_mut();
        let state = state.as_mut().ok_or_else(|| Error::from(E_NOTIMPL))?;
        unsafe {
            OleSetClipboard(&object)?;
        }
        state.object = Some(object);
        Ok(())
    })?;
    Ok(())
}
pub(super) fn finish() {
    OLE.with(|state| {
        state.borrow_mut().take();
    });
}

#[derive(Clone, Copy)]
struct Formats {
    descriptors: u16,
    contents: u16,
    preferred: u16,
    performed: u16,
    succeeded: u16,
}
impl Formats {
    fn new() -> Result<Self> {
        fn register(name: PCWSTR) -> Result<u16> {
            let value = unsafe { RegisterClipboardFormatW(name) };
            if value == 0 {
                Err(Error::from_win32())
            } else {
                Ok(value as u16)
            }
        }
        Ok(Self {
            descriptors: register(CFSTR_FILEDESCRIPTORW)?,
            contents: register(CFSTR_FILECONTENTS)?,
            preferred: register(CFSTR_PREFERREDDROPEFFECT)?,
            performed: register(CFSTR_PERFORMEDDROPEFFECT)?,
            succeeded: register(CFSTR_PASTESUCCEEDED)?,
        })
    }
    fn get_formats(self) -> [FORMATETC; 3] {
        let mut contents = format(self.contents, TYMED_ISTREAM);
        contents.lindex = 0;
        [
            format(self.descriptors, TYMED_HGLOBAL),
            contents,
            format(self.preferred, TYMED_HGLOBAL),
        ]
    }
}
fn format(id: u16, medium: TYMED) -> FORMATETC {
    FORMATETC {
        cfFormat: id,
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: medium.0 as u32,
        ..Default::default()
    }
}
pub(crate) fn data_object(files: RemoteFiles) -> Result<IDataObject> {
    Ok(FileDataObject {
        files,
        formats: Formats::new()?,
        asynchronous: AtomicBool::new(true),
        operating: AtomicBool::new(false),
    }
    .into())
}
#[implement(IDataObject, IDataObjectAsyncCapability)]
struct FileDataObject {
    files: RemoteFiles,
    formats: Formats,
    asynchronous: AtomicBool,
    operating: AtomicBool,
}
impl FileDataObject {
    fn query(&self, f: &FORMATETC) -> HRESULT {
        if !f.ptd.is_null() {
            return DV_E_DVTARGETDEVICE;
        }
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        let medium = if f.cfFormat == self.formats.contents {
            if f.lindex < -1 || f.lindex >= self.files.entries().len() as i32 {
                return DV_E_LINDEX;
            }
            TYMED_ISTREAM
        } else if f.cfFormat == self.formats.descriptors || f.cfFormat == self.formats.preferred {
            if f.lindex != -1 {
                return DV_E_LINDEX;
            }
            TYMED_HGLOBAL
        } else {
            return DV_E_FORMATETC;
        };
        if f.tymed & medium.0 as u32 == 0 {
            DV_E_TYMED
        } else {
            S_OK
        }
    }
}
impl IDataObject_Impl for FileDataObject_Impl {
    fn GetData(&self, f: *const FORMATETC) -> Result<STGMEDIUM> {
        let f = unsafe { f.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        self.query(f).ok()?;
        if f.cfFormat == self.formats.contents {
            if f.lindex < 0 {
                return Err(DV_E_LINDEX.into());
            }
            let fetch = self.files.fetch(f.lindex as usize).map_err(read_error)?;
            let stream: IStream = FileStream {
                fetch,
                cursor: Mutex::new(0),
            }
            .into();
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                ..Default::default()
            })
        } else if f.cfFormat == self.formats.preferred {
            // Copy only. A remote paste must never delete the original, even after local Ctrl+X.
            global_medium(&1u32.to_ne_bytes())
        } else {
            let entries = self.files.entries();
            let mut bytes =
                vec![0u8; size_of::<u32>() + entries.len() * size_of::<FILEDESCRIPTORW>()];
            bytes[..4].copy_from_slice(&(entries.len() as u32).to_ne_bytes());
            for (index, entry) in entries.iter().enumerate() {
                let mut descriptor = FILEDESCRIPTORW {
                    dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0 | FD_PROGRESSUI.0 | FD_UNICODE.0)
                        as u32,
                    dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
                    nFileSizeHigh: (entry.size >> 32) as u32,
                    nFileSizeLow: entry.size as u32,
                    ..Default::default()
                };
                // FILEDESCRIPTORW is packed; use a separate aligned array for the name.
                let mut name = [0; 260];
                for (dest, value) in name.iter_mut().zip(entry.name.encode_utf16()) {
                    *dest = value;
                }
                descriptor.cFileName = name;
                unsafe {
                    ptr::write_unaligned(
                        bytes
                            .as_mut_ptr()
                            .add(4 + index * size_of::<FILEDESCRIPTORW>())
                            .cast(),
                        descriptor,
                    );
                }
            }
            global_medium(&bytes)
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> Result<()> {
        Err(DV_E_TYMED.into())
    }
    fn QueryGetData(&self, f: *const FORMATETC) -> HRESULT {
        unsafe { f.as_ref() }.map_or(E_POINTER, |f| self.query(f))
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if out.is_null() {
            return E_POINTER;
        }
        unsafe {
            (*out).ptd = ptr::null_mut();
        }
        DATA_S_SAMEFORMATETC
    }
    fn SetData(&self, f: *const FORMATETC, medium: *const STGMEDIUM, release: BOOL) -> Result<()> {
        let f = unsafe { f.as_ref() }.ok_or_else(|| Error::from(E_POINTER))?;
        if medium.is_null() {
            return Err(E_POINTER.into());
        }
        if f.cfFormat != self.formats.performed && f.cfFormat != self.formats.succeeded {
            return Err(DV_E_FORMATETC.into());
        }
        // Acknowledge Shell completion notifications; never propagate a move/delete to the source.
        if release.as_bool() {
            unsafe {
                ReleaseStgMedium(medium.cast_mut());
            }
        }
        Ok(())
    }
    fn EnumFormatEtc(&self, direction: u32) -> Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(E_NOTIMPL.into());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats.get_formats()) }
    }
    fn DAdvise(&self, _: *const FORMATETC, _: u32, _: Ref<IAdviseSink>) -> Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}
impl IDataObjectAsyncCapability_Impl for FileDataObject_Impl {
    fn SetAsyncMode(&self, value: BOOL) -> Result<()> {
        self.asynchronous.store(value.as_bool(), Ordering::SeqCst);
        Ok(())
    }
    fn GetAsyncMode(&self) -> Result<BOOL> {
        Ok(self.asynchronous.load(Ordering::SeqCst).into())
    }
    fn StartOperation(&self, _: Ref<IBindCtx>) -> Result<()> {
        self.operating.store(true, Ordering::SeqCst);
        Ok(())
    }
    fn InOperation(&self) -> Result<BOOL> {
        Ok(self.operating.load(Ordering::SeqCst).into())
    }
    fn EndOperation(&self, result: HRESULT, _: Ref<IBindCtx>, effect: u32) -> Result<()> {
        self.operating.store(false, Ordering::SeqCst);
        if result.is_err() || effect == 0 {
            self.files.cancel();
        }
        Ok(())
    }
}
fn global_medium(bytes: &[u8]) -> Result<STGMEDIUM> {
    unsafe {
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes.len())?;
        let data = GlobalLock(memory);
        if data.is_null() {
            let _ = GlobalFree(Some(memory));
            return Err(E_OUTOFMEMORY.into());
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), data.cast(), bytes.len());
        let _ = GlobalUnlock(memory);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: memory },
            ..Default::default()
        })
    }
}
fn read_error(error: impl std::fmt::Display) -> Error {
    Error::new(STG_E_READFAULT, error.to_string())
}

#[implement(IStream)]
struct FileStream {
    fetch: Arc<Fetch>,
    cursor: Mutex<u64>,
}
impl ISequentialStream_Impl for FileStream_Impl {
    fn Read(&self, data: *mut c_void, count: u32, read: *mut u32) -> HRESULT {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if count == 0 {
            return S_OK;
        }
        if data.is_null() {
            return E_POINTER;
        }
        let result = (|| -> Result<u32> {
            // Keep COM dispatching without holding stream or clipboard locks while downloading.
            let path = self.fetch.wait(pump).map_err(read_error)?;
            let mut cursor = self.cursor.lock().unwrap();
            let length =
                u64::from(count).min(self.fetch.entry.size.saturating_sub(*cursor)) as usize;
            if length == 0 {
                return Ok(0);
            }
            let mut file = File::open(path).map_err(read_error)?;
            file.seek(SeekFrom::Start(*cursor)).map_err(read_error)?;
            let buffer = unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), length) };
            file.read_exact(buffer).map_err(read_error)?;
            *cursor += length as u64;
            Ok(length as u32)
        })();
        match result {
            Ok(length) => {
                if !read.is_null() {
                    unsafe {
                        *read = length;
                    }
                }
                if length == count { S_OK } else { S_FALSE }
            }
            Err(error) => error.code(),
        }
    }
    fn Write(&self, _: *const c_void, _: u32, written: *mut u32) -> HRESULT {
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        STG_E_ACCESSDENIED
    }
}
impl IStream_Impl for FileStream_Impl {
    fn Seek(&self, distance: i64, origin: STREAM_SEEK, position: *mut u64) -> Result<()> {
        let mut cursor = self.cursor.lock().unwrap();
        let base = match origin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *cursor,
            STREAM_SEEK_END => self.fetch.entry.size,
            _ => return Err(STG_E_INVALIDFUNCTION.into()),
        };
        let next = i128::from(base) + i128::from(distance);
        if !(0..=i128::from(u64::MAX)).contains(&next) {
            return Err(STG_E_INVALIDFUNCTION.into());
        }
        *cursor = next as u64;
        if !position.is_null() {
            unsafe {
                *position = *cursor;
            }
        }
        Ok(())
    }
    fn SetSize(&self, _: u64) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn CopyTo(
        &self,
        destination: Ref<IStream>,
        count: u64,
        read: *mut u64,
        written: *mut u64,
    ) -> Result<()> {
        if !read.is_null() {
            unsafe {
                *read = 0;
            }
        }
        if !written.is_null() {
            unsafe {
                *written = 0;
            }
        }
        let destination = destination.as_ref().ok_or_else(|| Error::from(E_POINTER))?;
        let mut buffer = vec![0u8; 64 * 1024];
        let mut total = 0;
        let mut total_written = 0;
        while total < count {
            let mut got = 0;
            let result = self.Read(
                buffer.as_mut_ptr().cast(),
                (count - total).min(buffer.len() as u64) as u32,
                &mut got,
            );
            result.ok()?;
            total += u64::from(got);
            if !read.is_null() {
                unsafe {
                    *read = total;
                }
            }
            if got == 0 {
                break;
            }
            let mut put = 0;
            let result = unsafe { destination.Write(buffer.as_ptr().cast(), got, Some(&mut put)) };
            total_written += u64::from(put);
            if !written.is_null() {
                unsafe {
                    *written = total_written;
                }
            }
            result.ok()?;
            if put != got {
                return Err(STG_E_WRITEFAULT.into());
            }
        }
        Ok(())
    }
    fn Commit(&self, _: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Err(STG_E_ACCESSDENIED.into())
    }
    fn LockRegion(&self, _: u64, _: u64, _: &LOCKTYPE) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn UnlockRegion(&self, _: u64, _: u64, _: u32) -> Result<()> {
        Err(STG_E_INVALIDFUNCTION.into())
    }
    fn Stat(&self, stat: *mut STATSTG, flags: &STATFLAG) -> Result<()> {
        if stat.is_null() {
            return Err(E_POINTER.into());
        }
        let mut value = STATSTG {
            r#type: STGTY_STREAM.0 as u32,
            cbSize: self.fetch.entry.size,
            grfMode: STGM_READ,
            ..Default::default()
        };
        if flags.0 & STATFLAG_NONAME.0 == 0 {
            let name: Vec<_> = self
                .fetch
                .entry
                .name
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let memory = unsafe { CoTaskMemAlloc(name.len() * 2) }.cast::<u16>();
            if memory.is_null() {
                return Err(E_OUTOFMEMORY.into());
            }
            unsafe {
                ptr::copy_nonoverlapping(name.as_ptr(), memory, name.len());
            }
            value.pwcsName = PWSTR(memory);
        }
        unsafe {
            *stat = value;
        }
        Ok(())
    }
    fn Clone(&self) -> Result<IStream> {
        Ok(FileStream {
            fetch: self.fetch.clone(),
            cursor: Mutex::new(*self.cursor.lock().unwrap()),
        }
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        engine::Runtime,
        exchange::{
            paste::{Entry, Manifest},
            wire::{self, ClipboardVersion},
        },
    };

    #[test]
    fn native_file_descriptors_preserve_unicode_and_64_bit_sizes_without_downloading() {
        let rt = Runtime::new(&Config::default());
        let source = wire::new_id();
        let size = 5 * 1024 * 1024 * 1024 + 17;
        let remote = RemoteFiles::new(
            &rt,
            source.clone(),
            Manifest {
                token: wire::new_id(),
                files: vec![Entry {
                    name: "资料🙂.bin".into(),
                    size,
                }],
            },
            ClipboardVersion {
                clock: 1,
                origin: source,
            },
        )
        .unwrap();
        let object = data_object(remote).unwrap();
        let formats = Formats::new().unwrap();
        unsafe {
            let enumerator = object.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
            let mut list = [FORMATETC::default(); 4];
            let mut count = 0;
            assert_eq!(enumerator.Next(&mut list, Some(&mut count)), S_FALSE);
            assert_eq!(count, 3);
            assert_eq!(list[0].cfFormat, formats.descriptors);
            assert_eq!(list[1].cfFormat, formats.contents);
            assert_eq!(list[2].cfFormat, formats.preferred);
            let mut medium = object
                .GetData(&format(formats.descriptors, TYMED_HGLOBAL))
                .unwrap();
            let memory = GlobalLock(medium.u.hGlobal).cast::<u8>();
            assert!(!memory.is_null());
            let count = ptr::read_unaligned(memory.cast::<u32>());
            let descriptor = ptr::read_unaligned(memory.add(4).cast::<FILEDESCRIPTORW>());
            let name = descriptor.cFileName;
            let actual_size =
                (u64::from(descriptor.nFileSizeHigh) << 32) | u64::from(descriptor.nFileSizeLow);
            let _ = GlobalUnlock(medium.u.hGlobal);
            ReleaseStgMedium(&mut medium);
            assert_eq!(count, 1);
            assert_eq!(actual_size, size);
            assert_eq!(
                String::from_utf16(&name[..name.iter().position(|n| *n == 0).unwrap()]).unwrap(),
                "资料🙂.bin"
            );
            let mut effect = object
                .GetData(&format(formats.preferred, TYMED_HGLOBAL))
                .unwrap();
            let memory = GlobalLock(effect.u.hGlobal).cast::<u32>();
            let copy_effect = ptr::read_unaligned(memory);
            let _ = GlobalUnlock(effect.u.hGlobal);
            ReleaseStgMedium(&mut effect);
            assert_eq!(copy_effect, 1, "remote paste must always copy");
            let mut request = format(formats.contents, TYMED_ISTREAM);
            assert_eq!(object.QueryGetData(&request), S_OK);
            assert_eq!(object.GetData(&request).err().unwrap().code(), DV_E_LINDEX);
            request.lindex = 1;
            assert_eq!(object.QueryGetData(&request), DV_E_LINDEX);
            request.lindex = 0;
            request.tymed = TYMED_HGLOBAL.0 as u32;
            assert_eq!(object.QueryGetData(&request), DV_E_TYMED);
            request.tymed = (TYMED_HGLOBAL.0 | TYMED_ISTREAM.0) as u32;
            assert_eq!(object.QueryGetData(&request), S_OK);
            let asynchronous: IDataObjectAsyncCapability = object.cast().unwrap();
            assert!(asynchronous.GetAsyncMode().unwrap().as_bool());
            assert!(!asynchronous.InOperation().unwrap().as_bool());
        }
        assert!(rt.exchange.snapshot().is_empty());
    }
}
