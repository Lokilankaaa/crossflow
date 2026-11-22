//! Cross-platform native clipboard reader/writer (text + PNG images).
//!
//! Uses Win32 Clipboard APIs on Windows and AppKit `NSPasteboard` via ObjC FFI
//! on macOS, with zero external crate dependencies.

use snow::resolvers::{CryptoResolver, DefaultResolver};

pub const MAX_TEXT_BYTES: usize = 4 * 1024 * 1024; // 4 MiB
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024; // 16 MiB

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardItem {
    Text(String),
    Png(Vec<u8>),
}

impl ClipboardItem {
    pub fn kind_str(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::Png(_) => "png",
        }
    }

    pub fn bytes(&self) -> &[u8] {
        match self {
            Self::Text(s) => s.as_bytes(),
            Self::Png(b) => b.as_slice(),
        }
    }

    pub fn hash_hex(&self) -> String {
        let tag = match self {
            Self::Text(_) => b'T',
            Self::Png(_) => b'P',
        };
        blake2s_hex(tag, self.bytes())
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
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use std::sync::OnceLock;
    use std::thread;
    use std::time::Duration;

    const CF_DIB: u32 = 8;
    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;

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
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> *mut c_void;
        fn GlobalFree(mem: *mut c_void) -> *mut c_void;
        fn GlobalLock(mem: *mut c_void) -> *mut c_void;
        fn GlobalUnlock(mem: *mut c_void) -> i32;
        fn GlobalSize(mem: *mut c_void) -> usize;
    }

    fn png_format() -> u32 {
        static FMT: OnceLock<u32> = OnceLock::new();
        *FMT.get_or_init(|| {
            let name: [u16; 4] = [b'P' as u16, b'N' as u16, b'G' as u16, 0];
            unsafe { RegisterClipboardFormatW(name.as_ptr()) }
        })
    }

    struct ClipboardGuard;
    impl ClipboardGuard {
        fn open() -> Option<Self> {
            for _ in 0..8 {
                if unsafe { OpenClipboard(null_mut()) } != 0 {
                    return Some(Self);
                }
                thread::sleep(Duration::from_millis(4));
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
        // Prefer UTF-16 text first
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
                let fmt = png_format();
                fmt != 0 && set_global_bytes(fmt, png)
            }
        }
    }
}

#[cfg(target_os = "macos")]
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
    unsafe fn msg2_id_bool(obj: Id, sel: Sel, a1: Id, a2: Id) -> bool {
        let f: unsafe extern "C" fn(Id, Sel, Id, Id) -> bool =
            std::mem::transmute(objc_msgSend as *const ());
        f(obj, sel, a1, a2)
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
            // Prefer plain UTF-8 text
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
            // Fallback: PNG image
            let png_type = nsstring(c"public.png");
            let data_sel = sel_registerName(c"dataForType:".as_ptr());
            let ns_data = msg1_id(pb, data_sel, png_type);
            if !ns_data.is_null() {
                let len = msg0_usize(ns_data, sel_registerName(c"length".as_ptr()));
                if len > 8 && len <= MAX_IMAGE_BYTES {
                    let ptr = msg0_ptr(ns_data, sel_registerName(c"bytes".as_ptr()));
                    if !ptr.is_null() {
                        let bytes = std::slice::from_raw_parts(ptr, len).to_vec();
                        if bytes.starts_with(&[137, 80, 78, 71]) {
                            return Some(ClipboardItem::Png(bytes));
                        }
                    }
                }
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
                    let Ok(cstr) = std::ffi::CString::new(text.as_str()) else {
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
}
