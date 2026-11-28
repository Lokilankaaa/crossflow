//! Cross-platform native clipboard reader/writer (text + PNG images).
//!
//! Uses Win32 Clipboard APIs on Windows and AppKit `NSPasteboard` via ObjC FFI
//! on macOS, with zero external crate dependencies.

use snow::resolvers::{CryptoResolver, DefaultResolver};
use std::path::PathBuf;

pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
pub const MAX_IMAGE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardItem {
    Text(String),
    Png(Vec<u8>),
    Files(Vec<PathBuf>),
}

impl ClipboardItem {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Png(_) => "png",
            Self::Files(_) => "files",
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(s) => s.as_bytes(),
            Self::Png(b) => b.as_slice(),
            Self::Files(_) => &[],
        }
    }

    pub fn hash_hex(&self) -> String {
        match self {
            Self::Text(s) => blake2s_hex(b'T', s.as_bytes()),
            Self::Png(b) => blake2s_hex(b'P', b.as_slice()),
            Self::Files(paths) => {
                let joined = paths
                    .iter()
                    .map(|p| p.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("\n");
                blake2s_hex(b'F', joined.as_bytes())
            }
        }
    }
}

pub fn blake2s_hex(tag: u8, bytes: &[u8]) -> String {
    let mut hasher = DefaultResolver
        .resolve_hash(&snow::params::HashChoice::Blake2s)
        .expect("BLAKE2s available");
    hasher.reset();
    hasher.input(&[tag]);
    hasher.input(bytes);
    let mut out = [0u8; 32];
    hasher.result(&mut out);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(64);
    for b in out {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn change_token() -> u64 {
    #[cfg(windows)]
    {
        windows_clip::change_token()
    }
    #[cfg(target_os = "macos")]
    {
        macos_clip::change_token()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        0
    }
}

pub fn read() -> Option<ClipboardItem> {
    #[cfg(windows)]
    {
        windows_clip::read()
    }
    #[cfg(target_os = "macos")]
    {
        macos_clip::read()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        None
    }
}

pub fn write(item: &ClipboardItem) -> bool {
    #[cfg(windows)]
    {
        windows_clip::write(item)
    }
    #[cfg(target_os = "macos")]
    {
        macos_clip::write(item)
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = item;
        false
    }
}

/// Minimal pure-Rust RGBA -> valid PNG encoder using zlib stored blocks (no extra deps).
/// Used to convert Windows `CF_DIB` screenshots into portable PNG bytes for macOS.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Option<Vec<u8>> {
    let w = width as usize;
    let h = height as usize;
    if w == 0 || h == 0 || w > 8192 || h > 8192 || rgba.len() != w.checked_mul(h)?.checked_mul(4)? {
        return None;
    }
    // Build raw filtered scanlines: 1 filter byte (0 = None) + w*4 RGBA bytes per row.
    let row_bytes = w * 4 + 1;
    let raw_len = row_bytes.checked_mul(h)?;
    if raw_len > MAX_IMAGE_BYTES {
        return None;
    }
    let mut raw = Vec::with_capacity(raw_len);
    for y in 0..h {
        raw.push(0);
        raw.extend_from_slice(&rgba[y * w * 4..(y + 1) * w * 4]);
    }

    // Wrap raw scanlines in zlib stored blocks (RFC 1950 + RFC 1951).
    let mut zlib = Vec::with_capacity(raw.len() + (raw.len() / 65535 + 1) * 5 + 6);
    zlib.push(0x78); // CMF: deflate, 32K window
    zlib.push(0x01); // FLG: no dict, check bits
    let mut offset = 0;
    while offset < raw.len() {
        let end = (offset + 65535).min(raw.len());
        let chunk = &raw[offset..end];
        let is_final = end == raw.len();
        zlib.push(if is_final { 0x01 } else { 0x00 });
        let len = chunk.len() as u16;
        zlib.extend_from_slice(&len.to_le_bytes());
        zlib.extend_from_slice(&(!len).to_le_bytes());
        zlib.extend_from_slice(chunk);
        offset = end;
    }
    let adler = adler32(&raw);
    zlib.extend_from_slice(&adler.to_be_bytes());

    let mut png = Vec::with_capacity(zlib.len() + 64);
    png.extend_from_slice(&[137, 80, 78, 71, 13, 10, 26, 10]); // PNG signature
    let mut ihdr = [0u8; 13];
    ihdr[0..4].copy_from_slice(&width.to_be_bytes());
    ihdr[4..8].copy_from_slice(&height.to_be_bytes());
    ihdr[8] = 8; // bit depth
    ihdr[9] = 6; // color type RGBA
    ihdr[10] = 0; // compression
    ihdr[11] = 0; // filter
    ihdr[12] = 0; // interlace
    write_png_chunk(&mut png, b"IHDR", &ihdr);
    write_png_chunk(&mut png, b"IDAT", &zlib);
    write_png_chunk(&mut png, b"IEND", &[]);
    if png.len() > MAX_IMAGE_BYTES {
        return None;
    }
    Some(png)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn adler32(data: &[u8]) -> u32 {
    const MOD: u32 = 65521;
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &byte in data {
        a = (a + u32::from(byte)) % MOD;
        b = (b + a) % MOD;
    }
    (b << 16) | a
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn write_png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// Converts uncompressed 24-bit or 32-bit Windows `CF_DIB` (`BITMAPINFOHEADER`) to PNG.
#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn dib_to_png(dib: &[u8]) -> Option<Vec<u8>> {
    if dib.len() < 40 {
        return None;
    }
    let header_size = u32::from_le_bytes(dib[0..4].try_into().ok()?) as usize;
    if header_size < 40 || dib.len() < header_size {
        return None;
    }
    let width_i32 = i32::from_le_bytes(dib[4..8].try_into().ok()?);
    let height_i32 = i32::from_le_bytes(dib[8..12].try_into().ok()?);
    let bpp = u16::from_le_bytes(dib[14..16].try_into().ok()?);
    let compression = u32::from_le_bytes(dib[16..20].try_into().ok()?);
    // BI_RGB = 0, BI_BITFIELDS = 3
    if width_i32 <= 0 || height_i32 == 0 || (bpp != 24 && bpp != 32) || (compression != 0 && compression != 3) {
        return None;
    }
    let width = width_i32 as usize;
    let height = height_i32.unsigned_abs() as usize;
    let top_down = height_i32 < 0;
    let extra_masks = if header_size == 40 && compression == 3 { 12 } else { 0 };
    let pixel_offset = header_size + extra_masks;
    let bytes_per_pixel = (bpp / 8) as usize;
    let stride = (width * bytes_per_pixel + 3) & !3;
    let needed = pixel_offset.checked_add(stride.checked_mul(height)?)?;
    if dib.len() < needed {
        return None;
    }
    let mut rgba = vec![0u8; width.checked_mul(height)?.checked_mul(4)?];
    for y in 0..height {
        let src_row = if top_down { y } else { height - 1 - y };
        let src = &dib[pixel_offset + src_row * stride..pixel_offset + src_row * stride + width * bytes_per_pixel];
        let dst = &mut rgba[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let b = src[x * bytes_per_pixel];
            let g = src[x * bytes_per_pixel + 1];
            let r = src[x * bytes_per_pixel + 2];
            let a = if bytes_per_pixel == 4 {
                let raw_a = src[x * 4 + 3];
                if raw_a == 0 { 255 } else { raw_a }
            } else {
                255
            };
            dst[x * 4] = r;
            dst[x * 4 + 1] = g;
            dst[x * 4 + 2] = b;
            dst[x * 4 + 3] = a;
        }
    }
    encode_rgba_png(width as u32, height as u32, &rgba)
}

#[cfg(windows)]
mod windows_clip {
    use super::*;
    use std::ffi::{OsString, c_void};
    use std::os::windows::ffi::OsStringExt;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::Duration;

    const CF_DIB: u32 = 8;
    const CF_UNICODETEXT: u32 = 13;
    const CF_HDROP: u32 = 15;
    const GMEM_MOVEABLE: u32 = 0x0002;

    #[link(name = "shell32")]
    unsafe extern "system" {
        fn DragQueryFileW(hdrop: *mut c_void, i_file: u32, lpsz_file: *mut u16, cch: u32) -> u32;
    }

    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetClipboardSequenceNumber() -> u32;
        fn OpenClipboard(hwnd_new_owner: *mut c_void) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn IsClipboardFormatAvailable(format: u32) -> i32;
        fn GetClipboardData(format: u32) -> *mut c_void;
        fn SetClipboardData(format: u32, mem: *mut c_void) -> *mut c_void;
        fn RegisterClipboardFormatW(name: *const u16) -> u32;
        fn CreateWindowExW(
            dw_ex_style: u32,
            lp_class_name: *const u16,
            lp_window_name: *const u16,
            dw_style: u32,
            x: i32,
            y: i32,
            n_width: i32,
            n_height: i32,
            h_wnd_parent: *mut c_void,
            h_menu: *mut c_void,
            h_instance: *mut c_void,
            lp_param: *mut c_void,
        ) -> *mut c_void;
        fn DestroyWindow(hwnd: *mut c_void) -> i32;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalSize(mem: *mut c_void) -> usize;
    }

    #[link(name = "ole32")]
    unsafe extern "system" {
        fn CreateStreamOnHGlobal(
            h_global: *mut c_void,
            delete_on_release: i32,
            ppstm: *mut *mut c_void,
        ) -> i32;
    }

    #[repr(C)]
    struct GdiplusStartupInput {
        gdiplus_version: u32,
        debug_event_callback: *const c_void,
        suppress_background_thread: i32,
        suppress_external_codecs: i32,
    }

    #[repr(C)]
    struct GpRect {
        x: i32,
        y: i32,
        width: i32,
        height: i32,
    }

    #[repr(C)]
    struct BitmapData {
        width: u32,
        height: u32,
        stride: i32,
        pixel_format: i32,
        scan0: *mut u8,
        reserved: usize,
    }

    #[link(name = "gdiplus")]
    unsafe extern "system" {
        fn GdiplusStartup(
            token: *mut usize,
            input: *const GdiplusStartupInput,
            output: *mut c_void,
        ) -> i32;
        fn GdipCreateBitmapFromStream(stream: *mut c_void, bitmap: *mut *mut c_void) -> i32;
        fn GdipGetImageWidth(image: *mut c_void, width: *mut u32) -> i32;
        fn GdipGetImageHeight(image: *mut c_void, height: *mut u32) -> i32;
        fn GdipBitmapLockBits(
            bitmap: *mut c_void,
            rect: *const GpRect,
            flags: u32,
            format: i32,
            locked: *mut BitmapData,
        ) -> i32;
        fn GdipBitmapUnlockBits(bitmap: *mut c_void, locked: *mut BitmapData) -> i32;
        fn GdipDisposeImage(image: *mut c_void) -> i32;
    }

    fn ensure_gdiplus() -> bool {
        static INIT: OnceLock<bool> = OnceLock::new();
        *INIT.get_or_init(|| {
            let mut token: usize = 0;
            let input = GdiplusStartupInput {
                gdiplus_version: 1,
                debug_event_callback: std::ptr::null(),
                suppress_background_thread: 0,
                suppress_external_codecs: 0,
            };
            unsafe { GdiplusStartup(&mut token, &input, null_mut()) == 0 }
        })
    }

    /// Decode PNG bytes into a universal 32-bit bottom-up Windows `CF_DIB`
    /// (`BITMAPINFOHEADER` + BGRA pixels) using built-in Windows GDI+.
    pub(super) fn png_to_dib(png: &[u8]) -> Option<Vec<u8>> {
        if png.len() < 8 || !png.starts_with(&[137, 80, 78, 71]) || !ensure_gdiplus() {
            return None;
        }
        let hmem = unsafe { GlobalAlloc(GMEM_MOVEABLE, png.len()) };
        if hmem.is_null() {
            return None;
        }
        let ptr = unsafe { GlobalLock(hmem) };
        if ptr.is_null() {
            unsafe {
                let _ = GlobalFree(hmem);
            }
            return None;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(png.as_ptr(), ptr.cast::<u8>(), png.len());
            let _ = GlobalUnlock(hmem);
        }
        let mut stream: *mut c_void = null_mut();
        if unsafe { CreateStreamOnHGlobal(hmem, 1, &mut stream) } != 0 || stream.is_null() {
            unsafe {
                let _ = GlobalFree(hmem);
            }
            return None;
        }
        let mut bitmap: *mut c_void = null_mut();
        let status = unsafe { GdipCreateBitmapFromStream(stream, &mut bitmap) };
        // Release IStream (vtable slot 2 = IUnknown::Release)
        unsafe {
            let vtbl = *(stream as *const *const usize);
            let release: unsafe extern "system" fn(*mut c_void) -> u32 =
                std::mem::transmute(*vtbl.add(2));
            release(stream);
        }
        if status != 0 || bitmap.is_null() {
            return None;
        }
        let result = (|| -> Option<Vec<u8>> {
            let mut width: u32 = 0;
            let mut height: u32 = 0;
            if unsafe { GdipGetImageWidth(bitmap, &mut width) } != 0
                || unsafe { GdipGetImageHeight(bitmap, &mut height) } != 0
            {
                return None;
            }
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                return None;
            }
            let rect = GpRect {
                x: 0,
                y: 0,
                width: width as i32,
                height: height as i32,
            };
            let mut data = BitmapData {
                width: 0,
                height: 0,
                stride: 0,
                pixel_format: 0,
                scan0: null_mut(),
                reserved: 0,
            };
            // ImageLockModeRead = 1, PixelFormat32bppARGB = 0x0026200A (BGRA in memory)
            if unsafe { GdipBitmapLockBits(bitmap, &rect, 1, 0x0026_200A, &mut data) } != 0
                || data.scan0.is_null()
            {
                return None;
            }
            let w = width as usize;
            let h = height as usize;
            let row_bytes = w.checked_mul(4)?;
            let pixel_bytes = row_bytes.checked_mul(h)?;
            if pixel_bytes > MAX_IMAGE_BYTES * 2 {
                unsafe {
                    let _ = GdipBitmapUnlockBits(bitmap, &mut data);
                }
                return None;
            }
            let mut dib = Vec::with_capacity(40 + pixel_bytes);
            dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
            dib.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
            dib.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (positive = bottom-up)
            dib.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
            dib.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
            dib.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
            dib.extend_from_slice(&(pixel_bytes as u32).to_le_bytes()); // biSizeImage
            dib.extend_from_slice(&[0u8; 16]); // XPelsPerMeter, YPelsPerMeter, ClrUsed, ClrImportant
            let stride = data.stride as isize;
            for y in 0..h {
                let src_y = (h - 1 - y) as isize;
                let row_ptr = unsafe { data.scan0.offset(src_y * stride) };
                let row = unsafe { std::slice::from_raw_parts(row_ptr, row_bytes) };
                dib.extend_from_slice(row);
            }
            unsafe {
                let _ = GdipBitmapUnlockBits(bitmap, &mut data);
            }
            Some(dib)
        })();
        unsafe {
            let _ = GdipDisposeImage(bitmap);
        }
        result
    }

    fn png_format() -> u32 {
        static FMT: OnceLock<u32> = OnceLock::new();
        *FMT.get_or_init(|| {
            let name: [u16; 4] = [b'P' as u16, b'N' as u16, b'G' as u16, 0];
            unsafe { RegisterClipboardFormatW(name.as_ptr()) }
        })
    }

    fn preferred_drop_effect_format() -> u32 {
        static FMT: OnceLock<u32> = OnceLock::new();
        *FMT.get_or_init(|| {
            let wide: Vec<u16> = "Preferred DropEffect\0".encode_utf16().collect();
            unsafe { RegisterClipboardFormatW(wide.as_ptr()) }
        })
    }

    const HWND_MESSAGE: *mut c_void = (-3isize) as *mut c_void;

    struct OwnerWindow(*mut c_void);
    impl OwnerWindow {
        fn create() -> Self {
            // Microsoft Win32 spec: EmptyClipboard() after OpenClipboard(NULL) sets the owner
            // to NULL and causes SetClipboardData() to fail in headless/CreateNoWindow processes.
            let cls: [u16; 7] = [
                b'S' as u16, b'T' as u16, b'A' as u16, b'T' as u16, b'I' as u16, b'C' as u16, 0,
            ];
            let title: [u16; 1] = [0];
            let hwnd = unsafe {
                CreateWindowExW(
                    0,
                    cls.as_ptr(),
                    title.as_ptr(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                )
            };
            Self(hwnd)
        }
    }
    impl Drop for OwnerWindow {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = DestroyWindow(self.0);
                }
            }
        }
    }

    thread_local! {
        static CLIPBOARD_OWNER: OwnerWindow = OwnerWindow::create();
    }

    fn owner_hwnd() -> *mut c_void {
        CLIPBOARD_OWNER.with(|w| w.0)
    }

    struct ClipboardGuard;
    impl ClipboardGuard {
        fn open() -> Option<Self> {
            let hwnd = owner_hwnd();
            for _ in 0..15 {
                if unsafe { OpenClipboard(hwnd) } != 0 {
                    return Some(Self);
                }
                thread::sleep(Duration::from_millis(6));
            }
            None
        }
    }
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseClipboard();
            }
        }
    }

    pub fn change_token() -> u64 {
        u64::from(unsafe { GetClipboardSequenceNumber() })
    }

    fn copy_handle_bytes(handle: *mut c_void, limit: usize) -> Option<Vec<u8>> {
        if handle.is_null() {
            return None;
        }
        let size = unsafe { GlobalSize(handle) };
        if size == 0 || size > limit {
            return None;
        }
        let ptr = unsafe { GlobalLock(handle) };
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) }.to_vec();
        unsafe {
            let _ = GlobalUnlock(handle);
        }
        Some(bytes)
    }

    pub fn read() -> Option<ClipboardItem> {
        let _guard = ClipboardGuard::open()?;
        // Check copied files/folders (CF_HDROP) before plain text
        if unsafe { IsClipboardFormatAvailable(CF_HDROP) } != 0 {
            let handle = unsafe { GetClipboardData(CF_HDROP) };
            if !handle.is_null() {
                let count = unsafe { DragQueryFileW(handle, 0xFFFF_FFFF, null_mut(), 0) };
                if count > 0 && count <= 256 {
                    let mut paths = Vec::with_capacity(count as usize);
                    for i in 0..count {
                        let len = unsafe { DragQueryFileW(handle, i, null_mut(), 0) };
                        if len > 0 && len <= 4096 {
                            let mut buf = vec![0u16; (len + 1) as usize];
                            let copied =
                                unsafe { DragQueryFileW(handle, i, buf.as_mut_ptr(), len + 1) };
                            if copied > 0 {
                                let p = PathBuf::from(OsString::from_wide(&buf[..copied as usize]));
                                if p.exists() {
                                    paths.push(p);
                                }
                            }
                        }
                    }
                    if !paths.is_empty() {
                        return Some(ClipboardItem::Files(paths));
                    }
                }
            }
        }
        // Prefer UTF-16 text next
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) } != 0 {
            let handle = unsafe { GetClipboardData(CF_UNICODETEXT) };
            if let Some(bytes) = copy_handle_bytes(handle, MAX_TEXT_BYTES * 2 + 4) {
                let u16s: Vec<u16> = bytes
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .take_while(|&w| w != 0)
                    .collect();
                let text = String::from_utf16_lossy(&u16s);
                if !text.is_empty() && text.len() <= MAX_TEXT_BYTES {
                    return Some(ClipboardItem::Text(text));
                }
            }
        }
        // Check native PNG format
        let png_fmt = png_format();
        if png_fmt != 0 && unsafe { IsClipboardFormatAvailable(png_fmt) } != 0 {
            let handle = unsafe { GetClipboardData(png_fmt) };
            if let Some(bytes) = copy_handle_bytes(handle, MAX_IMAGE_BYTES)
                && bytes.starts_with(&[137, 80, 78, 71])
            {
                return Some(ClipboardItem::Png(bytes));
            }
        }
        // Fallback: Windows Snipping Tool / Paint / WeChat CF_DIB -> PNG
        if unsafe { IsClipboardFormatAvailable(CF_DIB) } != 0 {
            let handle = unsafe { GetClipboardData(CF_DIB) };
            if let Some(dib) = copy_handle_bytes(handle, MAX_IMAGE_BYTES * 2)
                && let Some(png) = dib_to_png(&dib)
            {
                return Some(ClipboardItem::Png(png));
            }
        }
        None
    }

    fn set_global_bytes(format: u32, bytes: &[u8]) -> bool {
        let mem = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len()) };
        if mem.is_null() {
            return false;
        }
        let ptr = unsafe { GlobalLock(mem) };
        if ptr.is_null() {
            unsafe {
                let _ = GlobalFree(mem);
            }
            return false;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr.cast::<u8>(), bytes.len());
            let _ = GlobalUnlock(mem);
        }
        if unsafe { SetClipboardData(format, mem) }.is_null() {
            unsafe {
                let _ = GlobalFree(mem);
            }
            return false;
        }
        true
    }

    pub fn write(item: &ClipboardItem) -> bool {
        let Some(_guard) = ClipboardGuard::open() else {
            return false;
        };
        if unsafe { EmptyClipboard() } == 0 {
            return false;
        }
        match item {
            ClipboardItem::Text(text) => {
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                wide.push(0);
                let bytes: Vec<u8> = wide.iter().flat_map(|w| w.to_le_bytes()).collect();
                set_global_bytes(CF_UNICODETEXT, &bytes)
            }
            ClipboardItem::Png(png) => {
                // Write BOTH universal CF_DIB (for Paint, WeChat, QQ, Chrome, Office, Explorer)
                // AND registered "PNG" (for transparency-aware apps and Crossflow itself).
                let mut ok = false;
                if let Some(dib) = png_to_dib(png)
                    && set_global_bytes(CF_DIB, &dib)
                {
                    ok = true;
                }
                let fmt = png_format();
                if fmt != 0 && set_global_bytes(fmt, png) {
                    ok = true;
                }
                ok
            }
            ClipboardItem::Files(paths) => {
                if paths.is_empty() {
                    return false;
                }
                // DROPFILES (20 bytes): pFiles=20, pt=(0,0), fNC=0, fWide=1
                let mut buf = Vec::with_capacity(20 + paths.len() * 128);
                buf.extend_from_slice(&20u32.to_le_bytes());
                buf.extend_from_slice(&0i32.to_le_bytes());
                buf.extend_from_slice(&0i32.to_le_bytes());
                buf.extend_from_slice(&0u32.to_le_bytes());
                buf.extend_from_slice(&1u32.to_le_bytes());
                for p in paths {
                    let s = p.to_string_lossy();
                    for w in s.encode_utf16() {
                        buf.extend_from_slice(&w.to_le_bytes());
                    }
                    buf.extend_from_slice(&0u16.to_le_bytes());
                }
                buf.extend_from_slice(&0u16.to_le_bytes());
                if !set_global_bytes(CF_HDROP, &buf) {
                    return false;
                }
                // Advertise DROPEFFECT_COPY (1) so Windows 11 Explorer, Desktop,
                // WeChat, QQ, and Office all enable Ctrl+V paste cleanly.
                let effect_fmt = preferred_drop_effect_format();
                if effect_fmt != 0 {
                    let _ = set_global_bytes(effect_fmt, &1u32.to_le_bytes());
                }
                true
            }
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_op_in_unsafe_fn)]
mod macos_clip {
    use super::*;
    use std::ffi::{CStr, c_char, c_void};

    type Id = *mut c_void;
    type Sel = *mut c_void;
    type Cls = *mut c_void;

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Cls;
        fn sel_registerName(name: *const c_char) -> Sel;
        fn objc_msgSend();
        fn objc_autoreleasePoolPush() -> *mut c_void;
        fn objc_autoreleasePoolPop(pool: *mut c_void);
    }

    #[link(name = "AppKit", kind = "framework")]
    unsafe extern "C" {}

    struct AutoreleasePool(*mut c_void);
    impl AutoreleasePool {
        fn new() -> Self {
            Self(unsafe { objc_autoreleasePoolPush() })
        }
    }
    impl Drop for AutoreleasePool {
        fn drop(&mut self) {
            unsafe { objc_autoreleasePoolPop(self.0) };
        }
    }

    unsafe fn msg0_id(obj: Id, sel: Sel) -> Id {
        let f: unsafe extern "C" fn(Id, Sel) -> Id = std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel)
    }
    unsafe fn msg0_isize(obj: Id, sel: Sel) -> isize {
        let f: unsafe extern "C" fn(Id, Sel) -> isize =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel)
    }
    unsafe fn msg0_usize(obj: Id, sel: Sel) -> usize {
        let f: unsafe extern "C" fn(Id, Sel) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel)
    }
    unsafe fn msg0_ptr(obj: Id, sel: Sel) -> *const u8 {
        let f: unsafe extern "C" fn(Id, Sel) -> *const u8 =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel)
    }
    unsafe fn msg1_id(obj: Id, sel: Sel, a1: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1)
    }
    unsafe fn msg1_cstr_id(obj: Id, sel: Sel, a1: *const c_char) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, *const c_char) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1)
    }
    unsafe fn msg2_bytes_id(obj: Id, sel: Sel, bytes: *const u8, len: usize) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, *const u8, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, bytes, len)
    }
    unsafe fn msg1_usize_id(obj: Id, sel: Sel, idx: usize) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, usize) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, idx)
    }
    unsafe fn msg1_id_bool(obj: Id, sel: Sel, a1: Id) -> bool {
        let f: unsafe extern "C" fn(Id, Sel, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1)
    }
    unsafe fn msg1_id_void(obj: Id, sel: Sel, a1: Id) {
        let f: unsafe extern "C" fn(Id, Sel, Id) =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1)
    }
    unsafe fn msg2_id_bool(obj: Id, sel: Sel, a1: Id, a2: Id) -> bool {
        let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1, a2)
    }
    unsafe fn msg2_usize_id_id(obj: Id, sel: Sel, a1: usize, a2: Id) -> Id {
        let f: unsafe extern "C" fn(Id, Sel, usize, Id) -> Id =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1, a2)
    }

    unsafe fn nsdata_to_vec(ns_data: Id, limit: usize) -> Option<Vec<u8>> {
        if ns_data.is_null() {
            return None;
        }
        let len = msg0_usize(ns_data, sel_registerName(c"length".as_ptr()));
        if len <= 8 || len > limit {
            return None;
        }
        let ptr = msg0_ptr(ns_data, sel_registerName(c"bytes".as_ptr()));
        if ptr.is_null() {
            return None;
        }
        Some(std::slice::from_raw_parts(ptr, len).to_vec())
    }

    /// Convert arbitrary AppKit image data (`public.tiff`, `public.jpeg`, etc.) to PNG bytes.
    unsafe fn image_data_to_png(ns_data: Id) -> Option<Vec<u8>> {
        if ns_data.is_null() {
            return None;
        }
        let rep_cls = objc_getClass(c"NSBitmapImageRep".as_ptr());
        let rep = msg1_id(rep_cls, sel_registerName(c"imageRepWithData:".as_ptr()), ns_data);
        if rep.is_null() {
            return None;
        }
        // NSBitmapImageFileTypePNG = 4
        let png_data = msg2_usize_id_id(
            rep,
            sel_registerName(c"representationUsingType:properties:".as_ptr()),
            4,
            std::ptr::null_mut(),
        );
        let bytes = nsdata_to_vec(png_data, MAX_IMAGE_BYTES)?;
        bytes.starts_with(&[137, 80, 78, 71]).then_some(bytes)
    }

    unsafe fn nsstring(s: &CStr) -> Id {
        let cls = objc_getClass(c"NSString".as_ptr());
        let sel = sel_registerName(c"stringWithUTF8String:".as_ptr());
        msg1_cstr_id(cls, sel, s.as_ptr())
    }

    unsafe fn general_pasteboard() -> Id {
        let cls = objc_getClass(c"NSPasteboard".as_ptr());
        let sel = sel_registerName(c"generalPasteboard".as_ptr());
        msg0_id(cls, sel)
    }

    pub fn change_token() -> u64 {
        let _pool = AutoreleasePool::new();
        unsafe {
            let pb = general_pasteboard();
            if pb.is_null() {
                return 0;
            }
            let sel = sel_registerName(c"changeCount".as_ptr());
            msg0_isize(pb, sel) as u64
        }
    }

    pub fn read() -> Option<ClipboardItem> {
        let _pool = AutoreleasePool::new();
        unsafe {
            let pb = general_pasteboard();
            if pb.is_null() {
                return None;
            }
            // Check Finder copied file URLs BEFORE plain text (Finder puts filenames as text too!)
            let items = msg0_id(pb, sel_registerName(c"pasteboardItems".as_ptr()));
            if !items.is_null() {
                let count = msg0_usize(items, sel_registerName(c"count".as_ptr()));
                if count > 0 && count <= 256 {
                    let file_url_type = nsstring(c"public.file-url");
                    let str_sel = sel_registerName(c"stringForType:".as_ptr());
                    let obj_at_sel = sel_registerName(c"objectAtIndex:".as_ptr());
                    let nsurl_cls = objc_getClass(c"NSURL".as_ptr());
                    let url_with_str_sel = sel_registerName(c"URLWithString:".as_ptr());
                    let file_path_url_sel = sel_registerName(c"filePathURL".as_ptr());
                    let path_sel = sel_registerName(c"path".as_ptr());
                    let utf8_sel = sel_registerName(c"UTF8String".as_ptr());
                    let mut paths = Vec::new();
                    for i in 0..count {
                        let item = msg1_usize_id(items, obj_at_sel, i);
                        if item.is_null() {
                            continue;
                        }
                        let url_str = msg1_id(item, str_sel, file_url_type);
                        if url_str.is_null() {
                            continue;
                        }
                        let url = msg1_id(nsurl_cls, url_with_str_sel, url_str);
                        if url.is_null() {
                            continue;
                        }
                        let resolved = msg0_id(url, file_path_url_sel);
                        let target_url = if resolved.is_null() { url } else { resolved };
                        let path_ns = msg0_id(target_url, path_sel);
                        if path_ns.is_null() {
                            continue;
                        }
                        let ptr = msg0_ptr(path_ns, utf8_sel) as *const c_char;
                        if !ptr.is_null() {
                            let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                            let p = PathBuf::from(s);
                            if p.exists() {
                                paths.push(p);
                            }
                        }
                    }
                    if !paths.is_empty() {
                        return Some(ClipboardItem::Files(paths));
                    }
                }
            }
            // Check direct PNG image first (screenshots, browser/chat "Copy Image").
            // Apps that copy rich text (Word/Excel/Pages) put RTF/TIFF, not raw PNG.
            let data_sel = sel_registerName(c"dataForType:".as_ptr());
            let png_type = nsstring(c"public.png");
            let ns_png = msg1_id(pb, data_sel, png_type);
            if let Some(bytes) = nsdata_to_vec(ns_png, MAX_IMAGE_BYTES)
                && bytes.starts_with(&[137, 80, 78, 71])
            {
                return Some(ClipboardItem::Png(bytes));
            }
            // Prefer plain UTF-8 text next
            let text_type = nsstring(c"public.utf8-plain-text");
            let str_sel = sel_registerName(c"stringForType:".as_ptr());
            let ns_str = msg1_id(pb, str_sel, text_type);
            if !ns_str.is_null() {
                let utf8_sel = sel_registerName(c"UTF8String".as_ptr());
                let ptr = msg0_ptr(ns_str, utf8_sel) as *const c_char;
                if !ptr.is_null() {
                    let s = CStr::from_ptr(ptr).to_string_lossy().into_owned();
                    if !s.is_empty() && s.len() <= MAX_TEXT_BYTES {
                        return Some(ClipboardItem::Text(s));
                    }
                }
            }
            // Fallback: macOS Preview / Photos / Safari / WeChat TIFF or JPEG -> PNG
            let tiff_type = nsstring(c"public.tiff");
            let ns_tiff = msg1_id(pb, data_sel, tiff_type);
            if let Some(bytes) = image_data_to_png(ns_tiff) {
                return Some(ClipboardItem::Png(bytes));
            }
            let jpeg_type = nsstring(c"public.jpeg");
            let ns_jpeg = msg1_id(pb, data_sel, jpeg_type);
            if let Some(bytes) = image_data_to_png(ns_jpeg) {
                return Some(ClipboardItem::Png(bytes));
            }
            None
        }
    }

    pub fn write(item: &ClipboardItem) -> bool {
        let _pool = AutoreleasePool::new();
        unsafe {
            let pb = general_pasteboard();
            if pb.is_null() {
                return false;
            }
            let _ = msg0_isize(pb, sel_registerName(c"clearContents".as_ptr()));
            match item {
                ClipboardItem::Text(text) => {
                    let sanitized = if text.as_bytes().contains(&0) {
                        text.replace('\0', "")
                    } else {
                        text.clone()
                    };
                    let Ok(cstr) = std::ffi::CString::new(sanitized) else {
                        return false;
                    };
                    let val = nsstring(&cstr);
                    let ty = nsstring(c"public.utf8-plain-text");
                    let sel = sel_registerName(c"setString:forType:".as_ptr());
                    msg2_id_bool(pb, sel, val, ty)
                }
                ClipboardItem::Png(png) => {
                    let cls = objc_getClass(c"NSData".as_ptr());
                    let data = msg2_bytes_id(
                        cls,
                        sel_registerName(c"dataWithBytes:length:".as_ptr()),
                        png.as_ptr(),
                        png.len(),
                    );
                    if data.is_null() {
                        return false;
                    }
                    let ty = nsstring(c"public.png");
                    let sel = sel_registerName(c"setData:forType:".as_ptr());
                    msg2_id_bool(pb, sel, data, ty)
                }
                ClipboardItem::Files(paths) => {
                    if paths.is_empty() {
                        return false;
                    }
                    let arr_cls = objc_getClass(c"NSMutableArray".as_ptr());
                    let arr = msg0_id(arr_cls, sel_registerName(c"array".as_ptr()));
                    if arr.is_null() {
                        return false;
                    }
                    let url_cls = objc_getClass(c"NSURL".as_ptr());
                    let file_url_sel = sel_registerName(c"fileURLWithPath:".as_ptr());
                    let add_sel = sel_registerName(c"addObject:".as_ptr());
                    for p in paths {
                        let Ok(cstr) = std::ffi::CString::new(p.to_string_lossy().as_ref()) else {
                            continue;
                        };
                        let ns_path = nsstring(&cstr);
                        let url = msg1_id(url_cls, file_url_sel, ns_path);
                        if !url.is_null() {
                            msg1_id_void(arr, add_sel, url);
                        }
                    }
                    msg1_id_bool(pb, sel_registerName(c"writeObjects:".as_ptr()), arr)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blake2s_hash_separates_text_and_png_payloads() {
        let t = ClipboardItem::Text("hello".into()).hash_hex();
        let p = ClipboardItem::Png(b"hello".to_vec()).hash_hex();
        assert_eq!(t.len(), 64);
        assert_ne!(t, p);
    }

    #[test]
    fn dib_to_png_produces_valid_png_signature_and_chunks() {
        // Minimal 2x2 24-bit bottom-up DIB
        let mut dib = vec![0u8; 40 + 16];
        dib[0..4].copy_from_slice(&40u32.to_le_bytes());
        dib[4..8].copy_from_slice(&2i32.to_le_bytes());
        dib[8..12].copy_from_slice(&2i32.to_le_bytes());
        dib[12..14].copy_from_slice(&1u16.to_le_bytes());
        dib[14..16].copy_from_slice(&24u16.to_le_bytes());
        let png = dib_to_png(&dib).expect("valid 2x2 DIB must encode to PNG");
        assert!(png.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
        assert!(png.windows(4).any(|w| w == b"IHDR"));
        assert!(png.windows(4).any(|w| w == b"IDAT"));
        assert!(png.windows(4).any(|w| w == b"IEND"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_png_to_dib_roundtrip_decodes_pixels() {
        // 2x2 RGBA image: top-left red, top-right green, bottom-left blue, bottom-right white
        let rgba: [u8; 16] = [
            255, 0, 0, 255, // (0,0) red
            0, 255, 0, 255, // (1,0) green
            0, 0, 255, 255, // (0,1) blue
            255, 255, 255, 255, // (1,1) white
        ];
        let png = encode_rgba_png(2, 2, &rgba).expect("encode PNG");
        let dib = windows_clip::png_to_dib(&png).expect("GDI+ decode PNG to CF_DIB");
        assert_eq!(dib.len(), 40 + 16);
        assert_eq!(u32::from_le_bytes(dib[0..4].try_into().unwrap()), 40);
        assert_eq!(i32::from_le_bytes(dib[4..8].try_into().unwrap()), 2);
        assert_eq!(i32::from_le_bytes(dib[8..12].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(dib[14..16].try_into().unwrap()), 32);
        // Bottom-up DIB: first pixel row is y=1 (blue, white) in BGRA
        assert_eq!(&dib[40..44], &[255, 0, 0, 255]); // blue (B=255,G=0,R=0,A=255)
        assert_eq!(&dib[44..48], &[255, 255, 255, 255]); // white
        // Second pixel row is y=0 (red, green) in BGRA
        assert_eq!(&dib[48..52], &[0, 0, 255, 255]); // red
        assert_eq!(&dib[52..56], &[0, 255, 0, 255]); // green
        // Round-trip back to PNG via dib_to_png
        let png2 = dib_to_png(&dib).expect("re-encode DIB to PNG");
        assert!(png2.starts_with(&[137, 80, 78, 71, 13, 10, 26, 10]));
    }
}
