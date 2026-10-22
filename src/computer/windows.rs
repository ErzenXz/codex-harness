//! Small Windows desktop driver. All calls run behind the shared approval/gate.
//! Uses only inbox user32, GDI and GDI+; captures the primary physical display.
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::c_void,
    mem::size_of,
    ptr::{null, null_mut},
};
use tokio_util::sync::CancellationToken;

type Handle = *mut c_void;
#[repr(C)]
#[derive(Clone, Copy)]
struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Mouse {
    x: i32,
    y: i32,
    data: u32,
    flags: u32,
    time: u32,
    extra: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Keyboard {
    vk: u16,
    scan: u16,
    flags: u32,
    time: u32,
    extra: usize,
}
#[repr(C)]
union Event {
    mouse: Mouse,
    key: Keyboard,
}
#[repr(C)]
struct Input {
    kind: u32,
    event: Event,
}
#[repr(C)]
struct Startup {
    version: u32,
    callback: usize,
    suppress_thread: i32,
    suppress_codecs: i32,
}
#[repr(C)]
struct EncoderParameter {
    guid: Guid,
    count: u32,
    kind: u32,
    value: *const c_void,
}
#[repr(C)]
struct EncoderParameters {
    count: u32,
    parameter: EncoderParameter,
}

#[link(name = "user32")]
unsafe extern "system" {
    fn SetThreadDpiAwarenessContext(context: Handle) -> Handle;
    fn GetSystemMetrics(index: i32) -> i32;
    fn GetDC(window: Handle) -> Handle;
    fn ReleaseDC(window: Handle, dc: Handle) -> i32;
    fn SendInput(count: u32, inputs: *const Input, size: i32) -> u32;
    fn OpenInputDesktop(flags: u32, inherit: i32, access: u32) -> Handle;
    fn CloseDesktop(desktop: Handle) -> i32;
    fn GetThreadDesktop(thread: u32) -> Handle;
    fn GetUserObjectInformationW(
        object: Handle,
        index: i32,
        info: *mut c_void,
        length: u32,
        needed: *mut u32,
    ) -> i32;
    fn GetAsyncKeyState(key: i32) -> i16;
}
#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetCurrentThreadId() -> u32;
}
#[link(name = "gdi32")]
unsafe extern "system" {
    fn CreateCompatibleDC(dc: Handle) -> Handle;
    fn DeleteDC(dc: Handle) -> i32;
    fn CreateCompatibleBitmap(dc: Handle, width: i32, height: i32) -> Handle;
    fn DeleteObject(object: Handle) -> i32;
    fn SelectObject(dc: Handle, object: Handle) -> Handle;
    fn SetStretchBltMode(dc: Handle, mode: i32) -> i32;
    fn SetBrushOrgEx(dc: Handle, x: i32, y: i32, old: *mut c_void) -> i32;
    fn StretchBlt(
        target: Handle,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        source: Handle,
        sx: i32,
        sy: i32,
        sw: i32,
        sh: i32,
        operation: u32,
    ) -> i32;
}
#[link(name = "gdiplus")]
unsafe extern "system" {
    fn GdiplusStartup(token: *mut usize, input: *const Startup, output: *mut c_void) -> i32;
    fn GdiplusShutdown(token: usize);
    fn GdipCreateBitmapFromHBITMAP(bitmap: Handle, palette: Handle, image: *mut Handle) -> i32;
    fn GdipDisposeImage(image: Handle) -> i32;
    fn GdipSaveImageToStream(
        image: Handle,
        stream: Handle,
        encoder: *const Guid,
        parameters: *const EncoderParameters,
    ) -> i32;
}
#[link(name = "shlwapi")]
unsafe extern "system" {
    fn SHCreateMemStream(initial: *const u8, length: u32) -> Handle;
}

fn win_error(action: &str) -> anyhow::Error {
    anyhow::anyhow!(
        "{action}: {}. Use an unlocked interactive Windows desktop; secure/UAC desktops and higher-integrity applications cannot be controlled",
        std::io::Error::last_os_error()
    )
}
struct Dpi(Handle);
impl Dpi {
    fn enter() -> Result<Self> {
        // PER_MONITOR_AWARE_V2 is scoped to this thread and restored on every exit.
        let previous = unsafe { SetThreadDpiAwarenessContext(-4isize as Handle) };
        ensure!(
            !previous.is_null(),
            "Could not select physical desktop coordinates (Windows 10 1703 or later required)"
        );
        Ok(Self(previous))
    }
}
impl Drop for Dpi {
    fn drop(&mut self) {
        unsafe {
            SetThreadDpiAwarenessContext(self.0);
        }
    }
}
struct Desktop(Handle);
impl Drop for Desktop {
    fn drop(&mut self) {
        unsafe {
            CloseDesktop(self.0);
        }
    }
}
fn desktop_name(handle: Handle) -> Result<Vec<u16>> {
    let mut name = [0u16; 256];
    let mut needed = 0;
    ensure!(
        unsafe {
            GetUserObjectInformationW(
                handle,
                2,
                name.as_mut_ptr().cast(),
                size_of::<[u16; 256]>() as u32,
                &mut needed,
            )
        } != 0,
        "Could not identify the interactive desktop"
    );
    Ok(name.into_iter().take_while(|c| *c != 0).collect())
}
fn interactive() -> Result<()> {
    let input = Desktop(unsafe { OpenInputDesktop(0, 0, 1) });
    if input.0.is_null() {
        return Err(win_error("Desktop unavailable"));
    }
    let current = unsafe { GetThreadDesktop(GetCurrentThreadId()) };
    ensure!(
        !current.is_null() && desktop_name(input.0)? == desktop_name(current)?,
        "The current thread is not on the active desktop. Unlock Windows and run Piko in the signed-in desktop session"
    );
    Ok(())
}
fn dimensions() -> Result<(u32, u32)> {
    let (w, h) = unsafe { (GetSystemMetrics(0), GetSystemMetrics(1)) };
    ensure!(
        (1..=32768).contains(&w) && (1..=32768).contains(&h),
        "Invalid primary monitor geometry"
    );
    Ok((w as u32, h as u32))
}
pub fn geometry() -> Result<(u32, u32)> {
    let _dpi = Dpi::enter()?;
    interactive()?;
    dimensions()
}
struct Screen(Handle);
impl Drop for Screen {
    fn drop(&mut self) {
        unsafe {
            ReleaseDC(null_mut(), self.0);
        }
    }
}
struct Dc(Handle);
impl Drop for Dc {
    fn drop(&mut self) {
        unsafe {
            DeleteDC(self.0);
        }
    }
}
struct Bitmap(Handle);
impl Drop for Bitmap {
    fn drop(&mut self) {
        unsafe {
            DeleteObject(self.0);
        }
    }
}
struct Selection {
    dc: Handle,
    old: Handle,
}
impl Drop for Selection {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
        }
    }
}
struct Gdi(usize);
impl Drop for Gdi {
    fn drop(&mut self) {
        unsafe {
            GdiplusShutdown(self.0);
        }
    }
}
struct Image(Handle);
impl Drop for Image {
    fn drop(&mut self) {
        unsafe {
            GdipDisposeImage(self.0);
        }
    }
}
struct Stream(Handle);
impl Stream {
    // IUnknown (3), ISequentialStream::Read/Write, IStream::Seek.
    unsafe fn vtable(&self) -> *const usize {
        unsafe { *self.0.cast::<*const usize>() }
    }
    fn seek(&self, distance: i64, origin: u32) -> Result<u64> {
        let function: unsafe extern "system" fn(Handle, i64, u32, *mut u64) -> i32 =
            unsafe { std::mem::transmute(*self.vtable().add(5)) };
        let mut position = 0;
        ensure!(
            unsafe { function(self.0, distance, origin, &mut position) } >= 0,
            "Could not seek screenshot stream"
        );
        Ok(position)
    }
    fn bytes(&self, limit: usize) -> Result<Vec<u8>> {
        let length = self.seek(0, 2)?;
        ensure!(
            length > 0 && length <= limit as u64,
            "Screenshot exceeds the {limit}-byte image limit; reduce display complexity or resolution"
        );
        self.seek(0, 0)?;
        let mut bytes = vec![0; length as usize];
        let function: unsafe extern "system" fn(Handle, *mut u8, u32, *mut u32) -> i32 =
            unsafe { std::mem::transmute(*self.vtable().add(3)) };
        let mut read = 0;
        ensure!(
            unsafe { function(self.0, bytes.as_mut_ptr(), bytes.len() as u32, &mut read) } >= 0
                && read as usize == bytes.len(),
            "Incomplete screenshot stream"
        );
        Ok(bytes)
    }
}
impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let release: unsafe extern "system" fn(Handle) -> u32 =
                std::mem::transmute(*self.vtable().add(2));
            release(self.0);
        }
    }
}

pub fn screenshot(mut width: u32, mut height: u32, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        (1..=1280).contains(&width)
            && (1..=960).contains(&height)
            && (1..=96 * 1024).contains(&limit),
        "Invalid screenshot bounds"
    );
    // JPEG entropy varies dramatically. Downscale noisy displays until the actual
    // encoded image fits, retaining correct dimensions in the JPEG itself.
    for _ in 0..10 {
        let bytes = screenshot_once(width, height)?;
        if bytes.len() <= limit {
            return Ok(bytes);
        }
        width = (width * 3 / 4).max(1);
        height = (height * 3 / 4).max(1);
    }
    bail!("Screenshot cannot fit the {limit}-byte image limit")
}
fn screenshot_once(width: u32, height: u32) -> Result<Vec<u8>> {
    let _dpi = Dpi::enter()?;
    interactive()?;
    let (sw, sh) = dimensions()?;
    let screen = Screen(unsafe { GetDC(null_mut()) });
    if screen.0.is_null() {
        return Err(win_error("Could not open display"));
    }
    let dc = Dc(unsafe { CreateCompatibleDC(screen.0) });
    if dc.0.is_null() {
        return Err(win_error("Could not create screenshot context"));
    }
    // Allocate only the resized surface, never a full-resolution display buffer.
    let bitmap = Bitmap(unsafe { CreateCompatibleBitmap(screen.0, width as i32, height as i32) });
    if bitmap.0.is_null() {
        return Err(win_error("Could not allocate screenshot"));
    }
    let old = unsafe { SelectObject(dc.0, bitmap.0) };
    ensure!(
        !old.is_null() && old as isize != -1,
        "Could not select screenshot surface"
    );
    let selection = Selection { dc: dc.0, old };
    unsafe {
        SetStretchBltMode(dc.0, 4);
        SetBrushOrgEx(dc.0, 0, 0, null_mut());
    }
    if unsafe {
        StretchBlt(
            dc.0,
            0,
            0,
            width as i32,
            height as i32,
            screen.0,
            0,
            0,
            sw as i32,
            sh as i32,
            0x40CC0020,
        )
    } == 0
    {
        return Err(win_error("Screen capture failed"));
    }
    drop(selection);
    let mut token = 0;
    let startup = Startup {
        version: 1,
        callback: 0,
        suppress_thread: 0,
        suppress_codecs: 1,
    };
    ensure!(
        unsafe { GdiplusStartup(&mut token, &startup, null_mut()) } == 0,
        "Windows image encoder initialization failed"
    );
    let _gdi = Gdi(token);
    let mut raw_image = null_mut();
    ensure!(
        unsafe { GdipCreateBitmapFromHBITMAP(bitmap.0, null_mut(), &mut raw_image) } == 0
            && !raw_image.is_null(),
        "Could not prepare screenshot for JPEG encoding"
    );
    let image = Image(raw_image);
    let raw_stream = unsafe { SHCreateMemStream(null(), 0) };
    ensure!(
        !raw_stream.is_null(),
        "Could not allocate screenshot stream"
    );
    let stream = Stream(raw_stream);
    let jpeg = Guid {
        a: 0x557cf401,
        b: 0x1a04,
        c: 0x11d3,
        d: [0x9a, 0x73, 0, 0, 0xf8, 0x1e, 0xf3, 0x2e],
    };
    let quality_guid = Guid {
        a: 0x1d5be4b5,
        b: 0xfa4a,
        c: 0x452d,
        d: [0x9c, 0xdd, 0x5d, 0xb3, 0x51, 5, 0xe7, 0xeb],
    };
    let quality = 45u32;
    let parameters = EncoderParameters {
        count: 1,
        parameter: EncoderParameter {
            guid: quality_guid,
            count: 1,
            kind: 4,
            value: (&quality as *const u32).cast(),
        },
    };
    ensure!(
        unsafe { GdipSaveImageToStream(image.0, stream.0, &jpeg, &parameters) } == 0,
        "Windows JPEG encoding failed"
    );
    stream.bytes(4 * 1024 * 1024)
}
fn keyboard(vk: u16, scan: u16, flags: u32) -> Input {
    Input {
        kind: 1,
        event: Event {
            key: Keyboard {
                vk,
                scan,
                flags,
                time: 0,
                extra: 0,
            },
        },
    }
}
fn mouse(x: i32, y: i32, data: u32, flags: u32) -> Input {
    Input {
        kind: 0,
        event: Event {
            mouse: Mouse {
                x,
                y,
                data,
                flags,
                time: 0,
                extra: 0,
            },
        },
    }
}
fn send(inputs: &[Input]) -> Result<()> {
    let count = unsafe {
        SendInput(
            inputs.len() as u32,
            inputs.as_ptr(),
            size_of::<Input>() as i32,
        )
    };
    ensure!(
        count as usize == inputs.len(),
        "Windows accepted {count}/{} input events. Unlock the desktop and focus a normal, non-elevated application; Windows UIPI blocks higher-integrity or secure targets",
        inputs.len()
    );
    Ok(())
}
fn ensure_modifiers_released() -> Result<()> {
    ensure!(
        [0x10, 0x11, 0x12, 0x5b, 0x5c]
            .iter()
            .all(|key| unsafe { GetAsyncKeyState(*key) } >= 0),
        "Release physical Shift/Ctrl/Alt/Windows keys before computer input"
    );
    Ok(())
}
pub fn pointer(action: &str, x: u32, y: u32, button: &str) -> Result<()> {
    ensure!(
        matches!(action, "move" | "click" | "double_click"),
        "Unknown pointer action"
    );
    let (down, up) = match button {
        "left" => (2, 4),
        "right" => (8, 16),
        "middle" => (32, 64),
        _ => bail!("Unknown pointer button"),
    };
    let _dpi = Dpi::enter()?;
    interactive()?;
    let (w, h) = dimensions()?;
    ensure!(
        x < w && y < h,
        "Pointer coordinates are outside the primary monitor"
    );
    // ABSOLUTE without VIRTUALDESK addresses the primary monitor even with negative secondary origins.
    let x = (u64::from(x) * 65536 / u64::from(w) + 65536 / (u64::from(w) * 2)).min(65535) as i32;
    let y = (u64::from(y) * 65536 / u64::from(h) + 65536 / (u64::from(h) * 2)).min(65535) as i32;
    send(&[mouse(x, y, 0, 0x8001)])?;
    if action != "move" {
        ensure_modifiers_released()?;
        let clicks = if action == "double_click" { 2 } else { 1 };
        for _ in 0..clicks {
            if let Err(error) = send(&[mouse(0, 0, 0, down), mouse(0, 0, 0, up)]) {
                let _ = send(&[mouse(0, 0, 0, up)]);
                return Err(error);
            }
        }
    }
    Ok(())
}
pub fn type_text(text: &str, cancel: &CancellationToken) -> Result<()> {
    ensure!(text.len() <= 4096, "Computer text exceeds 4096 bytes");
    interactive()?;
    ensure_modifiers_released()?;
    // KEYEVENTF_UNICODE sends UTF-16 without changing clipboard contents or keyboard layout.
    // Small batches bound allocation; release every scan code again on partial insertion.
    let mut batch = Vec::with_capacity(128);
    for character in text.chars() {
        ensure!(!cancel.is_cancelled(), "Computer typing cancelled");
        let mut units = [0u16; 2];
        for &code in character.encode_utf16(&mut units).iter() {
            batch.push(keyboard(0, code, 4));
            batch.push(keyboard(0, code, 6));
        }
        // Never split a surrogate pair at an observable cancellation boundary.
        if batch.len() >= 128 {
            send_unicode(&batch)?;
            batch.clear();
        }
    }
    if !batch.is_empty() {
        send_unicode(&batch)?;
    }
    Ok(())
}
fn send_unicode(batch: &[Input]) -> Result<()> {
    if let Err(error) = send(batch) {
        let releases: Vec<_> = batch
            .iter()
            .step_by(2)
            .map(|input| keyboard(0, unsafe { input.event.key.scan }, 6))
            .collect();
        let _ = send(&releases);
        return Err(error);
    }
    Ok(())
}
fn named_key(name: &str) -> Option<(u16, bool)> {
    Some(match name {
        "ctrl" | "control" => (0x11, false),
        "alt" | "option" => (0x12, false),
        "shift" => (0x10, false),
        "meta" | "super" | "win" | "windows" | "command" | "cmd" => (0x5b, true),
        "enter" | "return" => (0x0d, false),
        "tab" => (9, false),
        "escape" | "esc" => (0x1b, false),
        "space" => (0x20, false),
        "backspace" => (8, false),
        "delete" | "del" => (0x2e, true),
        "insert" => (0x2d, true),
        "left" | "arrowleft" => (0x25, true),
        "up" | "arrowup" => (0x26, true),
        "right" | "arrowright" => (0x27, true),
        "down" | "arrowdown" => (0x28, true),
        "home" => (0x24, true),
        "end" => (0x23, true),
        "pageup" | "page_up" => (0x21, true),
        "pagedown" | "page_down" => (0x22, true),
        _ if name.len() == 1 && name.as_bytes()[0].is_ascii_alphanumeric() => {
            (name.as_bytes()[0].to_ascii_uppercase() as u16, false)
        }
        _ if name.starts_with('f') => {
            let n = name[1..].parse::<u16>().ok()?;
            if !(1..=24).contains(&n) {
                return None;
            }
            (0x6f + n, false)
        }
        _ => return None,
    })
}
fn chord_keys(chord: &str) -> Result<Vec<(u16, bool)>> {
    ensure!(
        !chord.is_empty() && chord.len() <= 128,
        "Invalid keyboard chord length"
    );
    let mut keys = Vec::new();
    for token in chord.split('+') {
        let token = token.trim().to_ascii_lowercase();
        let key = named_key(&token).with_context(|| format!("Unsupported Windows key: {token}"))?;
        ensure!(!keys.contains(&key), "Duplicate chord key: {token}");
        keys.push(key);
    }
    ensure!(keys.len() <= 8, "Too many chord keys");
    ensure!(
        keys[..keys.len() - 1]
            .iter()
            .all(|(vk, _)| matches!(*vk, 0x10..=0x12 | 0x5b)),
        "Only modifiers may precede the final chord key"
    );
    Ok(keys)
}
pub fn key(chord: &str) -> Result<()> {
    let keys = chord_keys(chord)?;
    interactive()?;
    ensure_modifiers_released()?;
    let mut events = Vec::with_capacity(keys.len() * 2);
    for &(vk, extended) in &keys {
        events.push(keyboard(vk, 0, u32::from(extended)));
    }
    for &(vk, extended) in keys.iter().rev() {
        events.push(keyboard(vk, 0, 2 | u32::from(extended)));
    }
    if let Err(error) = send(&events) {
        let releases: Vec<_> = keys
            .iter()
            .rev()
            .map(|&(vk, extended)| keyboard(vk, 0, 2 | u32::from(extended)))
            .collect();
        let _ = send(&releases);
        return Err(error);
    }
    Ok(())
}
pub fn scroll(direction: &str, amount: u32) -> Result<()> {
    ensure!((1..=20).contains(&amount), "Invalid scroll amount");
    let (flags, sign) = match direction {
        "up" => (0x800, 1),
        "down" => (0x800, -1),
        "right" => (0x1000, 1),
        "left" => (0x1000, -1),
        _ => bail!("Unknown scroll direction"),
    };
    interactive()?;
    ensure_modifiers_released()?;
    send(&[mouse(0, 0, (sign * amount as i32 * 120) as u32, flags)])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn win32_input_abi_and_chords() {
        assert_eq!(
            size_of::<Input>(),
            if cfg!(target_pointer_width = "64") {
                40
            } else {
                28
            }
        );
        assert_eq!(
            chord_keys("Ctrl+Shift+ArrowLeft").unwrap(),
            vec![(0x11, false), (0x10, false), (0x25, true)]
        );
        assert_eq!(
            chord_keys("Command+F24").unwrap(),
            vec![(0x5b, true), (0x87, false)]
        );
        for invalid in [
            "",
            "Ctrl+",
            "Ctrl+Ctrl+A",
            "A+B",
            "F25",
            "F0",
            "Ctrl+!",
            "Ctrl+Alt+Unknown",
        ] {
            assert!(chord_keys(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn invalid_native_inputs_do_not_touch_desktop() {
        assert!(screenshot(0, 1, 1).is_err());
        assert!(screenshot(1281, 1, 1).is_err());
        assert!(pointer("bad", 0, 0, "left").is_err());
        assert!(key("Ctrl+Unknown").is_err());
        assert!(scroll("up", 21).is_err());
        assert!(type_text(&"x".repeat(4097), &CancellationToken::new()).is_err());
    }
}
