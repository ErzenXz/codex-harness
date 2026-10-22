//! Native main-display capture and input. No subprocess, clipboard or resident helper.
//! ScreenCaptureKit is loaded lazily: its modern one-shot API avoids starting a stream.
use anyhow::{Context, Result, bail, ensure};
use std::{
    ffi::{CStr, c_char, c_int, c_void},
    ptr,
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

type Ref = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Size {
    width: f64,
    height: f64,
}
#[repr(C)]
#[derive(Clone, Copy)]
struct Rect {
    origin: Point,
    size: Size,
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> Rect;
    fn CGDisplayCreateImage(display: u32) -> Ref;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGColorSpaceCreateDeviceRGB() -> Ref;
    fn CGBitmapContextCreate(
        data: Ref,
        width: usize,
        height: usize,
        bits: usize,
        stride: usize,
        space: Ref,
        flags: u32,
    ) -> Ref;
    fn CGContextDrawImage(context: Ref, rect: Rect, image: Ref);
    fn CGContextSetInterpolationQuality(context: Ref, quality: i32);
    fn CGBitmapContextCreateImage(context: Ref) -> Ref;
    fn CGEventCreateMouseEvent(source: Ref, kind: u32, point: Point, button: u32) -> Ref;
    fn CGEventCreateKeyboardEvent(source: Ref, key: u16, down: bool) -> Ref;
    fn CGEventKeyboardSetUnicodeString(event: Ref, length: usize, text: *const u16);
    fn CGEventSetFlags(event: Ref, flags: u64);
    fn CGEventSourceFlagsState(state: i32) -> u64;
    fn CGEventSetIntegerValueField(event: Ref, field: u32, value: i64);
    fn CGEventCreateScrollWheelEvent2(
        source: Ref,
        units: u32,
        count: u32,
        vertical: i32,
        horizontal: i32,
        third: i32,
    ) -> Ref;
    fn CGEventPost(tap: u32, event: Ref);
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXIsProcessTrusted() -> bool;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRetain(object: Ref) -> Ref;
    fn CFRelease(object: Ref);
    fn CFDataCreateMutable(allocator: Ref, capacity: isize) -> Ref;
    fn CFDataGetLength(data: Ref) -> isize;
    fn CFDataGetBytePtr(data: Ref) -> *const u8;
    fn CFStringCreateWithCString(allocator: Ref, text: *const c_char, encoding: u32) -> Ref;
    fn CFNumberCreate(allocator: Ref, kind: i32, value: *const f64) -> Ref;
    fn CFDictionaryCreate(
        allocator: Ref,
        keys: *const Ref,
        values: *const Ref,
        count: isize,
        key_callbacks: Ref,
        value_callbacks: Ref,
    ) -> Ref;
}

#[link(name = "ImageIO", kind = "framework")]
unsafe extern "C" {
    static kCGImageDestinationLossyCompressionQuality: Ref;
    fn CGImageDestinationCreateWithData(data: Ref, kind: Ref, count: usize, options: Ref) -> Ref;
    fn CGImageDestinationAddImage(destination: Ref, image: Ref, properties: Ref);
    fn CGImageDestinationFinalize(destination: Ref) -> bool;
}

#[link(name = "objc")]
unsafe extern "C" {
    fn objc_getClass(name: *const c_char) -> Ref;
    fn sel_registerName(name: *const c_char) -> Ref;
    fn objc_msgSend();
    fn objc_autoreleasePoolPush() -> Ref;
    fn objc_autoreleasePoolPop(pool: Ref);
}
unsafe extern "C" {
    fn dlopen(path: *const c_char, flags: c_int) -> Ref;
    static _NSConcreteStackBlock: c_void;
}

struct Owned(Ref);
impl Owned {
    fn new(pointer: Ref, operation: &str) -> Result<Self> {
        ensure!(!pointer.is_null(), "macOS {operation} failed");
        Ok(Self(pointer))
    }
}
impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

struct Pool(Ref);
impl Pool {
    fn new() -> Self {
        Self(unsafe { objc_autoreleasePoolPush() })
    }
}
impl Drop for Pool {
    fn drop(&mut self) {
        unsafe { objc_autoreleasePoolPop(self.0) };
    }
}

fn bounds() -> Result<Rect> {
    let bounds = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    let size = bounds.size;
    ensure!(
        size.width.is_finite()
            && size.height.is_finite()
            && (1.0..=32768.0).contains(&size.width)
            && (1.0..=32768.0).contains(&size.height),
        "No usable macOS main display; run Piko in an unlocked graphical login session"
    );
    Ok(bounds)
}

pub fn geometry() -> Result<(u32, u32)> {
    let bounds = bounds()?;
    // CGDisplayBounds uses global points, while a captured Retina image uses pixels.
    Ok((
        bounds.size.width.round() as u32,
        bounds.size.height.round() as u32,
    ))
}

fn require_capture() -> Result<()> {
    ensure!(
        unsafe { CGPreflightScreenCaptureAccess() },
        "macOS Screen Recording permission is required. Open System Settings > Privacy & Security > Screen & System Audio Recording (Screen Recording on older macOS), enable the app or terminal launching Piko, then restart it. Piko does not open permission prompts automatically."
    );
    Ok(())
}

fn require_input() -> Result<()> {
    ensure!(
        unsafe { AXIsProcessTrusted() },
        "macOS Accessibility permission is required. Open System Settings > Privacy & Security > Accessibility, enable the app or terminal launching Piko, then restart it. Piko does not open permission prompts automatically."
    );
    ensure!(
        unsafe { CGEventSourceFlagsState(1) } & ((1 << 17) | (1 << 18) | (1 << 19) | (1 << 20))
            == 0,
        "Release physical Shift/Control/Option/Command keys before computer input"
    );
    Ok(())
}

/// Encodes at the exact requested viewport; quality falls in a bounded loop.
pub fn screenshot(width: u32, height: u32, limit: usize) -> Result<Vec<u8>> {
    ensure!(
        (1..=1280).contains(&width) && (1..=960).contains(&height),
        "Invalid screenshot viewport"
    );
    ensure!(
        (1..=96 * 1024).contains(&limit),
        "Invalid screenshot byte limit"
    );
    require_capture()?;
    let _pool = Pool::new();
    let captured = capture(width, height)?;
    compress(&captured, width, height, limit)
}

fn compress(captured: &Owned, width: u32, height: u32, limit: usize) -> Result<Vec<u8>> {
    for divisor in [1, 2, 3, 4] {
        let scaled_width = (width / divisor).max(1);
        let scaled_height = (height / divisor).max(1);
        if let Some(bytes) = encode(captured, scaled_width, scaled_height, limit)? {
            return Ok(bytes);
        }
    }
    bail!("Screenshot exceeds the 96 KiB limit even after adaptive JPEG compression and resizing")
}

fn encode(captured: &Owned, width: u32, height: u32, limit: usize) -> Result<Option<Vec<u8>>> {
    let color = Owned::new(unsafe { CGColorSpaceCreateDeviceRGB() }, "RGB color space")?;
    // 8-bit RGBX avoids an alpha channel in JPEG and caps the output bitmap at 4.7 MiB.
    let bitmap = Owned::new(
        unsafe {
            CGBitmapContextCreate(
                ptr::null_mut(),
                width as usize,
                height as usize,
                8,
                width as usize * 4,
                color.0,
                5, /* kCGImageAlphaNoneSkipLast */
            )
        },
        "screenshot bitmap",
    )?;
    unsafe {
        CGContextSetInterpolationQuality(bitmap.0, 3);
        CGContextDrawImage(
            bitmap.0,
            Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: width.into(),
                    height: height.into(),
                },
            },
            captured.0,
        );
    }
    let scaled = Owned::new(
        unsafe { CGBitmapContextCreateImage(bitmap.0) },
        "scaled screenshot",
    )?;
    drop(bitmap);
    let jpeg_type = Owned::new(
        unsafe { CFStringCreateWithCString(ptr::null_mut(), c"public.jpeg".as_ptr(), 0x08000100) },
        "JPEG type",
    )?;
    for quality in [0.58_f64, 0.40, 0.25, 0.12, 0.04] {
        let data = Owned::new(
            unsafe { CFDataCreateMutable(ptr::null_mut(), 0) },
            "JPEG buffer",
        )?;
        let destination = Owned::new(
            unsafe { CGImageDestinationCreateWithData(data.0, jpeg_type.0, 1, ptr::null_mut()) },
            "JPEG encoder",
        )?;
        let number = Owned::new(
            unsafe { CFNumberCreate(ptr::null_mut(), 13, &quality) },
            "JPEG quality",
        )?;
        // No dictionary callbacks are needed: both borrowed entries outlive encoding.
        let key = unsafe { kCGImageDestinationLossyCompressionQuality };
        let properties = Owned::new(
            unsafe {
                CFDictionaryCreate(
                    ptr::null_mut(),
                    &key,
                    &number.0,
                    1,
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            },
            "JPEG properties",
        )?;
        unsafe { CGImageDestinationAddImage(destination.0, scaled.0, properties.0) };
        ensure!(
            unsafe { CGImageDestinationFinalize(destination.0) },
            "macOS JPEG encoding failed"
        );
        let length =
            usize::try_from(unsafe { CFDataGetLength(data.0) }).context("Invalid JPEG length")?;
        if length <= limit {
            let bytes = unsafe { CFDataGetBytePtr(data.0) };
            ensure!(
                !bytes.is_null() && length > 0,
                "macOS returned an empty screenshot"
            );
            return Ok(Some(
                unsafe { std::slice::from_raw_parts(bytes, length) }.to_vec(),
            ));
        }
    }
    Ok(None)
}

// A native completion block owns one Arc per copied block. Timeouts never leave
// callbacks pointing at a stack frame, and late images are released by CaptureState.
static CAPTURE_REQUESTS: AtomicUsize = AtomicUsize::new(0);
struct CapturePermit;
impl CapturePermit {
    fn acquire() -> Result<Self> {
        reserve_capture(&CAPTURE_REQUESTS)?;
        Ok(Self)
    }
}
fn reserve_capture(counter: &AtomicUsize) -> Result<()> {
    let mut active = counter.load(Ordering::Acquire);
    loop {
        ensure!(
            active < 4,
            "Earlier macOS capture requests are still pending. Unlock the display and retry later, or restart Piko if ScreenCaptureKit remains unresponsive"
        );
        match counter.compare_exchange_weak(active, active + 1, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => return Ok(()),
            Err(current) => active = current,
        }
    }
}

impl Drop for CapturePermit {
    fn drop(&mut self) {
        CAPTURE_REQUESTS.fetch_sub(1, Ordering::AcqRel);
    }
}
struct CaptureState {
    _permit: CapturePermit,
    value: Mutex<Option<std::result::Result<usize, String>>>,
    ready: Condvar,
    core_foundation: bool,
}
impl Drop for CaptureState {
    fn drop(&mut self) {
        if let Ok(value) = self.value.get_mut()
            && let Some(Ok(image)) = value.take()
        {
            release_capture(image as Ref, self.core_foundation);
        }
    }
}
#[repr(C)]
struct CaptureBlock {
    isa: *const c_void,
    flags: i32,
    reserved: i32,
    invoke: unsafe extern "C" fn(*const CaptureBlock, Ref, Ref),
    descriptor: &'static BlockDescriptor,
    state: *const CaptureState,
}
#[repr(C)]
struct BlockDescriptor {
    reserved: usize,
    size: usize,
    copy: unsafe extern "C" fn(*mut CaptureBlock, *const CaptureBlock),
    dispose: unsafe extern "C" fn(*mut CaptureBlock),
}
unsafe extern "C" fn copy_block(_destination: *mut CaptureBlock, source: *const CaptureBlock) {
    unsafe { Arc::increment_strong_count((*source).state) };
}
unsafe extern "C" fn dispose_block(block: *mut CaptureBlock) {
    unsafe { Arc::decrement_strong_count((*block).state) };
}
static BLOCK_DESCRIPTOR: BlockDescriptor = BlockDescriptor {
    reserved: 0,
    size: std::mem::size_of::<CaptureBlock>(),
    copy: copy_block,
    dispose: dispose_block,
};
unsafe extern "C" fn captured(block: *const CaptureBlock, image: Ref, error: Ref) {
    let _pool = Pool::new();
    let state = unsafe { &*(*block).state };
    let result = if !error.is_null() {
        Err(format!(
            "ScreenCaptureKit capture failed: {}",
            error_description(error)
        ))
    } else if image.is_null() {
        Err("ScreenCaptureKit returned no screenshot; check Screen Recording permission and the unlocked display".into())
    } else {
        Ok(retain_capture(image, state.core_foundation) as usize)
    };
    if let Ok(mut value) = state.value.lock() {
        if value.is_none() {
            *value = Some(result);
            state.ready.notify_one();
        } else if let Ok(image) = result {
            release_capture(image as Ref, state.core_foundation);
        }
    } else if let Ok(image) = result {
        release_capture(image as Ref, state.core_foundation);
    }
}

fn error_description(error: Ref) -> String {
    unsafe {
        let send: unsafe extern "C" fn(Ref, Ref) -> Ref =
            std::mem::transmute(objc_msgSend as *const ());
        let description = send(error, sel_registerName(c"localizedDescription".as_ptr()));
        if description.is_null() {
            return "unknown error".into();
        }
        let utf8 = send(description, sel_registerName(c"UTF8String".as_ptr()));
        if utf8.is_null() {
            "unknown error".into()
        } else {
            CStr::from_ptr(utf8.cast()).to_string_lossy().into_owned()
        }
    }
}

fn retain_capture(object: Ref, core_foundation: bool) -> Ref {
    if core_foundation {
        unsafe { CFRetain(object) }
    } else {
        unsafe { send0(object, c"retain") }
    }
}
fn release_capture(object: Ref, core_foundation: bool) {
    if core_foundation {
        unsafe { CFRelease(object) };
    } else {
        unsafe { send0(object, c"release") };
    }
}
struct ObjcOwned(Ref);
impl ObjcOwned {
    fn new(object: Ref, operation: &str) -> Result<Self> {
        ensure!(!object.is_null(), "ScreenCaptureKit {operation} failed");
        Ok(Self(object))
    }
}
impl Drop for ObjcOwned {
    fn drop(&mut self) {
        release_capture(self.0, false);
    }
}
unsafe fn send0(object: Ref, name: &CStr) -> Ref {
    let send: unsafe extern "C" fn(Ref, Ref) -> Ref =
        unsafe { std::mem::transmute(objc_msgSend as *const ()) };
    unsafe { send(object, sel_registerName(name.as_ptr())) }
}

fn request(core_foundation: bool, start: impl FnOnce(*const CaptureBlock)) -> Result<usize> {
    let state = Arc::new(CaptureState {
        _permit: CapturePermit::acquire()?,
        value: Mutex::new(None),
        ready: Condvar::new(),
        core_foundation,
    });
    let block = CaptureBlock {
        isa: &raw const _NSConcreteStackBlock,
        flags: 1 << 25,
        reserved: 0,
        invoke: captured,
        descriptor: &BLOCK_DESCRIPTOR,
        state: Arc::as_ptr(&state),
    };
    start(&block);
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut value = state
        .value
        .lock()
        .map_err(|_| anyhow::anyhow!("Screenshot callback failed"))?;
    loop {
        if let Some(result) = value.take() {
            return result.map_err(anyhow::Error::msg);
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        ensure!(
            !remaining.is_zero(),
            "ScreenCaptureKit screenshot timed out; check Screen Recording permission and unlock the display"
        );
        value = state
            .ready
            .wait_timeout(value, remaining)
            .map_err(|_| anyhow::anyhow!("Screenshot callback failed"))?
            .0;
    }
}

fn capture(width: u32, height: u32) -> Result<Owned> {
    static SCREEN_CAPTURE_KIT: OnceLock<bool> = OnceLock::new();
    let loaded = *SCREEN_CAPTURE_KIT.get_or_init(|| unsafe {
        // Intentionally keep this system framework loaded for outstanding callbacks.
        !dlopen(
            c"/System/Library/Frameworks/ScreenCaptureKit.framework/ScreenCaptureKit".as_ptr(),
            1,
        )
        .is_null()
    });
    if loaded {
        unsafe {
            let class = objc_getClass(c"SCScreenshotManager".as_ptr());
            let selector = sel_registerName(
                c"captureImageWithFilter:configuration:completionHandler:".as_ptr(),
            );
            let responds: unsafe extern "C" fn(Ref, Ref, Ref) -> bool =
                std::mem::transmute(objc_msgSend as *const ());
            if !class.is_null()
                && responds(
                    class,
                    sel_registerName(c"respondsToSelector:".as_ptr()),
                    selector,
                )
            {
                return configured_capture(class, selector, width, height);
            }
        }
    }
    // This fallback is only for pre-macOS 14 systems lacking the modern one-shot API.
    Owned::new(
        unsafe { CGDisplayCreateImage(CGMainDisplayID()) },
        "screen capture; check Screen Recording permission and the unlocked graphical session",
    )
}

unsafe fn configured_capture(
    manager: Ref,
    selector: Ref,
    width: u32,
    height: u32,
) -> Result<Owned> {
    unsafe {
        let content_class = objc_getClass(c"SCShareableContent".as_ptr());
        ensure!(
            !content_class.is_null(),
            "ScreenCaptureKit content API unavailable"
        );
        let content = ObjcOwned::new(
            request(false, |block| {
                let send: unsafe extern "C" fn(Ref, Ref, bool, bool, *const CaptureBlock) =
                    std::mem::transmute(objc_msgSend as *const ());
                send(content_class, sel_registerName(c"getShareableContentExcludingDesktopWindows:onScreenWindowsOnly:completionHandler:".as_ptr()), true, true, block);
            })? as Ref,
            "display enumeration",
        )?;
        let displays = send0(content.0, c"displays");
        ensure!(!displays.is_null(), "ScreenCaptureKit returned no displays");
        let number: unsafe extern "C" fn(Ref, Ref) -> usize =
            std::mem::transmute(objc_msgSend as *const ());
        let count = number(displays, sel_registerName(c"count".as_ptr()));
        ensure!(count <= 128, "ScreenCaptureKit returned too many displays");
        let at: unsafe extern "C" fn(Ref, Ref, usize) -> Ref =
            std::mem::transmute(objc_msgSend as *const ());
        let display_id: unsafe extern "C" fn(Ref, Ref) -> u32 =
            std::mem::transmute(objc_msgSend as *const ());
        let mut main = ptr::null_mut();
        for index in 0..count {
            let display = at(
                displays,
                sel_registerName(c"objectAtIndex:".as_ptr()),
                index,
            );
            if !display.is_null()
                && display_id(display, sel_registerName(c"displayID".as_ptr())) == CGMainDisplayID()
            {
                main = display;
                break;
            }
        }
        ensure!(
            !main.is_null(),
            "Main display is unavailable to ScreenCaptureKit; unlock the graphical session"
        );
        let filter_class = objc_getClass(c"SCContentFilter".as_ptr());
        let config_class = objc_getClass(c"SCStreamConfiguration".as_ptr());
        let array_class = objc_getClass(c"NSArray".as_ptr());
        ensure!(
            !filter_class.is_null() && !config_class.is_null() && !array_class.is_null(),
            "ScreenCaptureKit configuration API unavailable"
        );
        let allocated = send0(filter_class, c"alloc");
        ensure!(
            !allocated.is_null(),
            "ScreenCaptureKit filter allocation failed"
        );
        let initialize: unsafe extern "C" fn(Ref, Ref, Ref, Ref) -> Ref =
            std::mem::transmute(objc_msgSend as *const ());
        let filter = ObjcOwned::new(
            initialize(
                allocated,
                sel_registerName(c"initWithDisplay:excludingWindows:".as_ptr()),
                main,
                send0(array_class, c"array"),
            ),
            "main display filter",
        )?;
        let config = ObjcOwned::new(send0(config_class, c"new"), "configuration")?;
        let set: unsafe extern "C" fn(Ref, Ref, usize) =
            std::mem::transmute(objc_msgSend as *const ());
        set(
            config.0,
            sel_registerName(c"setWidth:".as_ptr()),
            width as usize,
        );
        set(
            config.0,
            sel_registerName(c"setHeight:".as_ptr()),
            height as usize,
        );
        let set_bool: unsafe extern "C" fn(Ref, Ref, bool) =
            std::mem::transmute(objc_msgSend as *const ());
        set_bool(
            config.0,
            sel_registerName(c"setScalesToFit:".as_ptr()),
            true,
        );
        set_bool(
            config.0,
            sel_registerName(c"setShowsCursor:".as_ptr()),
            true,
        );
        let image = request(true, |block| {
            let send: unsafe extern "C" fn(Ref, Ref, Ref, Ref, *const CaptureBlock) =
                std::mem::transmute(objc_msgSend as *const ());
            send(manager, selector, filter.0, config.0, block);
        })?;
        Ok(Owned(image as Ref))
    }
}

fn post(event: Owned) {
    unsafe { CGEventPost(0, event.0) };
}

pub fn pointer(action: &str, x: u32, y: u32, button: &str) -> Result<()> {
    ensure!(
        matches!(action, "move" | "click" | "double_click"),
        "Invalid pointer action"
    );
    let (button, down, up) = match button {
        "left" => (0, 1, 2),
        "right" => (1, 3, 4),
        "middle" => (2, 25, 26),
        _ => bail!("Invalid mouse button"),
    };
    let bounds = bounds()?;
    ensure!(
        f64::from(x) < bounds.size.width && f64::from(y) < bounds.size.height,
        "Coordinate outside main display bounds"
    );
    require_input()?;
    let point = Point {
        x: bounds.origin.x + f64::from(x),
        y: bounds.origin.y + f64::from(y),
    };
    post(Owned::new(
        unsafe { CGEventCreateMouseEvent(ptr::null_mut(), 5, point, 0) },
        "pointer movement event",
    )?);
    if action != "move" {
        for count in 1..=if action == "double_click" { 2 } else { 1 } {
            let press = Owned::new(
                unsafe { CGEventCreateMouseEvent(ptr::null_mut(), down, point, button) },
                "mouse press event",
            )?;
            let release = Owned::new(
                unsafe { CGEventCreateMouseEvent(ptr::null_mut(), up, point, button) },
                "mouse release event",
            )?;
            unsafe {
                CGEventSetIntegerValueField(press.0, 1, count);
                CGEventSetIntegerValueField(release.0, 1, count);
            }
            post(press);
            post(release);
        }
    }
    Ok(())
}

fn keyboard(code: u16, down: bool, flags: u64) -> Result<Owned> {
    let event = Owned::new(
        unsafe { CGEventCreateKeyboardEvent(ptr::null_mut(), code, down) },
        "keyboard event",
    )?;
    unsafe { CGEventSetFlags(event.0, flags) };
    Ok(event)
}

pub fn type_text(text: &str, cancel: &CancellationToken) -> Result<()> {
    ensure!(
        text.len() <= 4096 && !text.contains('\0'),
        "Text exceeds 4096 bytes or contains NUL"
    );
    require_input()?;
    // One Unicode scalar per event pair preserves surrogate pairs and never changes the clipboard.
    for character in text.chars() {
        ensure!(!cancel.is_cancelled(), "Computer action cancelled");
        let mut units = [0_u16; 2];
        let encoded = character.encode_utf16(&mut units);
        let down = keyboard(0, true, 0)?;
        let up = keyboard(0, false, 0)?;
        unsafe {
            CGEventKeyboardSetUnicodeString(down.0, encoded.len(), encoded.as_ptr());
            CGEventKeyboardSetUnicodeString(up.0, encoded.len(), encoded.as_ptr());
        }
        post(down);
        post(up);
    }
    Ok(())
}

fn key_code(name: &str) -> Option<u16> {
    Some(match name.to_ascii_lowercase().as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "equal" | "equals" => 24,
        "9" => 25,
        "7" => 26,
        "minus" => 27,
        "8" => 28,
        "0" => 29,
        "bracketright" => 30,
        "o" => 31,
        "u" => 32,
        "bracketleft" => 33,
        "i" => 34,
        "p" => 35,
        "return" | "enter" => 36,
        "l" => 37,
        "j" => 38,
        "apostrophe" | "quote" => 39,
        "k" => 40,
        "semicolon" => 41,
        "backslash" => 42,
        "comma" => 43,
        "slash" => 44,
        "n" => 45,
        "m" => 46,
        "period" => 47,
        "tab" => 48,
        "space" => 49,
        "grave" | "backtick" => 50,
        "backspace" => 51,
        "escape" | "esc" => 53,
        "f17" => 64,
        "f18" => 79,
        "f19" => 80,
        "f20" => 90,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f3" => 99,
        "f8" => 100,
        "f9" => 101,
        "f11" => 103,
        "f13" => 105,
        "f16" => 106,
        "f14" => 107,
        "f10" => 109,
        "f12" => 111,
        "f15" => 113,
        "help" | "insert" => 114,
        "home" => 115,
        "pageup" | "page_up" | "prior" => 116,
        "delete" | "del" => 117,
        "f4" => 118,
        "end" => 119,
        "f2" => 120,
        "pagedown" | "page_down" | "next" => 121,
        "f1" => 122,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        _ => return None,
    })
}

fn chord(key: &str) -> Result<(Vec<(u16, u64)>, u16)> {
    ensure!(!key.is_empty() && key.len() <= 128, "Invalid key chord");
    let mut parts: Vec<_> = key.split('+').collect();
    let code = key_code(parts.pop().context("Missing key")?).context(
        "Unsupported macOS key; use a named key such as Return, Escape, PageUp or a letter",
    )?;
    let mut modifiers = Vec::with_capacity(parts.len());
    for name in parts {
        let modifier = match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => (59, 1 << 18),
            "alt" | "option" => (58, 1 << 19),
            "shift" => (56, 1 << 17),
            "super" | "cmd" | "command" | "meta" => (55, 1 << 20),
            _ => bail!("Unsupported macOS key modifier: {name}"),
        };
        ensure!(!modifiers.contains(&modifier), "Duplicate key modifier");
        modifiers.push(modifier);
    }
    Ok((modifiers, code))
}

pub fn key(key: &str) -> Result<()> {
    let (modifiers, code) = chord(key)?;
    require_input()?;
    // Allocate all events before posting any, so allocation failure cannot leave a modifier held.
    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    let mut flags = 0;
    for &(modifier, mask) in &modifiers {
        flags |= mask;
        events.push(keyboard(modifier, true, flags)?);
    }
    events.push(keyboard(code, true, flags)?);
    events.push(keyboard(code, false, flags)?);
    for &(modifier, mask) in modifiers.iter().rev() {
        flags &= !mask;
        events.push(keyboard(modifier, false, flags)?);
    }
    for event in events {
        post(event);
    }
    Ok(())
}

pub fn scroll(direction: &str, amount: u32) -> Result<()> {
    ensure!((1..=20).contains(&amount), "Scroll amount must be 1..=20");
    let (vertical, horizontal) = match direction {
        "up" => (amount as i32, 0),
        "down" => (-(amount as i32), 0),
        "left" => (0, amount as i32),
        "right" => (0, -(amount as i32)),
        _ => bail!("Invalid scroll direction"),
    };
    require_input()?;
    post(Owned::new(
        unsafe { CGEventCreateScrollWheelEvent2(ptr::null_mut(), 1, 2, vertical, horizontal, 0) },
        "scroll event",
    )?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    unsafe extern "C" {
        fn _Block_copy(block: *const c_void) -> *mut c_void;
        fn _Block_release(block: *mut c_void);
    }
    #[test]
    fn named_keys_and_modifier_aliases() {
        assert_eq!(
            chord("Command+Shift+P").unwrap(),
            (vec![(55, 1 << 20), (56, 1 << 17)], 35)
        );
        assert_eq!(chord("ctrl+alt+Delete").unwrap().1, 117);
        assert_eq!(chord("Super+Return").unwrap().1, 36);
        for invalid in ["", "+a", "Ctrl+Ctrl+A", "Hyper+a", "Ctrl+", "F99"] {
            assert!(chord(invalid).is_err(), "{invalid}");
        }
    }
    #[test]
    fn permission_checks_never_request_access() {
        // Preflight APIs only: never screenshots, input, permission prompts, or desktop mutation.
        let capture_allowed = unsafe { CGPreflightScreenCaptureAccess() };
        assert_eq!(require_capture().is_ok(), capture_allowed);
        let input_allowed = unsafe { AXIsProcessTrusted() };
        if !input_allowed {
            assert!(require_input().is_err());
        } else if unsafe { CGEventSourceFlagsState(1) }
            & ((1 << 17) | (1 << 18) | (1 << 19) | (1 << 20))
            == 0
        {
            assert!(require_input().is_ok());
        }
    }
    #[test]
    fn native_jpeg_encoder_scales_synthetic_pixels_without_screen_access() {
        let (width, height) = (1280, 960);
        let mut pixels = vec![0_u8; width * height * 4];
        let mut seed = 1234567_u32;
        for pixel in pixels.as_chunks_mut::<4>().0 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            pixel[..3].copy_from_slice(&seed.to_le_bytes()[..3]);
        }
        let color = Owned::new(unsafe { CGColorSpaceCreateDeviceRGB() }, "test RGB").unwrap();
        let bitmap = Owned::new(
            unsafe {
                CGBitmapContextCreate(
                    pixels.as_mut_ptr().cast(),
                    width,
                    height,
                    8,
                    width * 4,
                    color.0,
                    5,
                )
            },
            "test bitmap",
        )
        .unwrap();
        let image = Owned::new(
            unsafe { CGBitmapContextCreateImage(bitmap.0) },
            "test image",
        )
        .unwrap();
        let jpeg = compress(&image, width as u32, height as u32, 8 * 1024).unwrap();
        assert!(jpeg.len() <= 8 * 1024);
        let dimensions = crate::multimodal::image_dimensions("image/jpeg", &jpeg).unwrap();
        assert!(dimensions.0 < width as u32 && dimensions.1 < height as u32);
        assert!(
            (i64::from(dimensions.0) * height as i64 - i64::from(dimensions.1) * width as i64)
                .abs()
                <= width.max(height) as i64
        );
    }

    #[test]
    fn stalled_capture_requests_have_a_fixed_concurrency_limit() {
        let counter = AtomicUsize::new(0);
        for _ in 0..4 {
            reserve_capture(&counter).unwrap();
        }
        assert!(reserve_capture(&counter).is_err());
        assert_eq!(counter.load(Ordering::Acquire), 4);
        counter.fetch_sub(1, Ordering::AcqRel);
        reserve_capture(&counter).unwrap();
        assert_eq!(counter.load(Ordering::Acquire), 4);
    }

    #[test]
    fn completion_block_copies_and_releases_owned_callback_state() {
        let state = Arc::new(CaptureState {
            _permit: CapturePermit::acquire().unwrap(),
            value: Mutex::new(None),
            ready: Condvar::new(),
            core_foundation: true,
        });
        let block = CaptureBlock {
            isa: &raw const _NSConcreteStackBlock,
            flags: 1 << 25,
            reserved: 0,
            invoke: captured,
            descriptor: &BLOCK_DESCRIPTOR,
            state: Arc::as_ptr(&state),
        };
        let copied = unsafe { _Block_copy((&block as *const CaptureBlock).cast()) };
        assert!(!copied.is_null());
        assert_eq!(Arc::strong_count(&state), 2);
        unsafe {
            _Block_release(copied);
        }
        assert_eq!(Arc::strong_count(&state), 1);
        let error = request(true, |block| unsafe {
            captured(block, ptr::null_mut(), ptr::null_mut())
        })
        .unwrap_err();
        assert!(error.to_string().contains("no screenshot"));
    }

    #[test]
    fn utf16_keeps_non_bmp_scalars_intact() {
        let mut units = [0; 2];
        assert_eq!('🦀'.encode_utf16(&mut units), &[0xd83e, 0xdd80]);
    }
}
