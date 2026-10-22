"""Native desktop ACP checks. Live input is opt-in and targets only this test's window.

PIKO_NATIVE_DESKTOP_TEST=1 enables the Windows fixture on a disposable, unlocked
interactive desktop. Do not enable it while using that desktop for other work.
No screenshots are written to disk or printed. Python standard library only.
"""
import asyncio
import base64
import json
import os
from pathlib import Path
import random
import re
import signal
import sys
import tempfile
import threading
import time
import unittest
from stdio import Harness, call, response, final


SUPPORTED = sys.platform == 'darwin' or os.name == 'nt' or sys.platform.startswith('linux')


def results(body):
    return [item['output'] for item in body['input'] if item.get('type') == 'function_call_output']


def text_result(value):
    if isinstance(value, list):
        return '\n'.join(part['text'] for part in value if part.get('type') == 'input_text')
    return value


def checked_model(handler, failures):
    """Surface fixture assertions directly instead of retrying the fake provider."""
    async def checked(body):
        if failures:
            return final('Native fixture failed')
        try:
            return await handler(body)
        except Exception as error:
            error.add_note(f'Native fixture stage: {len(results(body))} completed tool calls')
            failures.append(error)
            return final('Native fixture failed')
    return checked


def jpeg_dimensions(data):
    """Read JPEG SOF without an image dependency; reject incomplete output."""
    if not data.startswith(b'\xff\xd8') or not data.endswith(b'\xff\xd9'):
        raise AssertionError('Native capture is not a complete JPEG')
    offset = 2
    while offset < len(data):
        if data[offset] != 255:
            raise AssertionError('Invalid JPEG marker')
        while offset < len(data) and data[offset] == 255:
            offset += 1
        marker = data[offset]
        offset += 1
        length = int.from_bytes(data[offset:offset + 2], 'big')
        if length < 2 or offset + length > len(data):
            raise AssertionError('Invalid JPEG segment length')
        if marker in (0xc0, 0xc1, 0xc2):
            return (int.from_bytes(data[offset + 5:offset + 7], 'big'),
                    int.from_bytes(data[offset + 3:offset + 5], 'big'))
        offset += length
    raise AssertionError('No supported JPEG dimensions')


class DesktopBoundaries(unittest.IsolatedAsyncioTestCase):
    async def test_disabled_readonly_and_rejected_never_access_desktop(self):
        # A rejected screenshot must not trigger an OS privacy prompt or capture.
        for enabled, mode, allowed, permissions in (
            (False, 'code', True, 0), (True, 'plan', True, 0),
            (True, 'code', False, int(SUPPORTED)),
        ):
            with self.subTest(enabled=enabled, mode=mode, allowed=allowed):
                async def model(body):
                    done = results(body)
                    if done:
                        self.assertIn('error', json.loads(text_result(done[-1])))
                        self.assertFalse(isinstance(done[-1], list))
                        return final()
                    names = {tool['name'] for tool in body['tools']}
                    self.assertEqual('computer_use' in names, enabled and mode != 'plan' and SUPPORTED)
                    return response([call('computer_use', {'action': 'screenshot'})])
                async with Harness(model, extra=f'computer_use = {str(enabled).lower()}\n', allow=allowed) as harness:
                    await harness.initialize()
                    session = await harness.session()
                    await harness.rpc('session/set_mode', {'sessionId': session, 'modeId': mode})
                    result = await harness.prompt(session)
                    self.assertEqual(result['result']['stopReason'], 'end_turn')
                    self.assertEqual(len(harness.permissions), permissions)

    async def test_invalid_input_never_reaches_native_backend(self):
        actions = [
            {'action': 'unknown'}, {'action': 'type', 'text': 'contains\0nul'},
            {'action': 'type', 'text': 'x' * 4097},
            {'action': 'key', 'key': '--window 1'},
            {'action': 'scroll', 'direction': 'down', 'amount': 21},
        ]
        async def model(body):
            done = results(body)
            if done:
                self.assertIn('error', json.loads(text_result(done[-1])))
            if len(done) == len(actions):
                return final()
            return response([call('computer_use', actions[len(done)], f'invalid-{len(done)}')])
        async with Harness(model, extra='computer_use = true\n') as harness:
            await harness.initialize()
            result = await harness.prompt(await harness.session())
            self.assertEqual(result['result']['stopReason'], 'end_turn')
            self.assertEqual(harness.permissions, [], 'Invalid arguments must fail before approval')

    @unittest.skipUnless(sys.platform == 'darwin' or os.name == 'nt',
                         'Native pointer snapshot ownership applies to macOS and Windows')
    async def test_pointer_requires_own_screenshot_before_any_os_access(self):
        actions = ['move', 'click', 'double_click']
        async def model(body):
            done = results(body)
            if done:
                error = json.loads(text_result(done[-1]))
                self.assertIn('fresh screenshot', error['error'].lower())
            if len(done) == len(actions):
                return final()
            return response([call('computer_use', {'action': actions[len(done)], 'x': 0, 'y': 0},
                                  f'no-snapshot-{len(done)}')])
        async with Harness(model, extra='computer_use = true\n') as harness:
            await harness.initialize()
            result = await harness.prompt(await harness.session())
            self.assertEqual(result['result']['stopReason'], 'end_turn')
            self.assertEqual(len(harness.permissions), len(actions))


class WindowsFixture:
    """A private top-level window that records actual OS-delivered input events."""
    def __enter__(self):
        self.events = []
        self.ready = threading.Event()
        self.error = None
        self.thread = threading.Thread(target=self._run, daemon=True)
        self.thread.start()
        if not self.ready.wait(10):
            raise AssertionError('Windows fixture did not initialize')
        if self.error:
            raise self.error
        return self

    def _run(self):
        import ctypes as C
        from ctypes import wintypes as W
        try:
            user = self.user = C.WinDLL('user32', use_last_error=True)
            kernel = C.WinDLL('kernel32', use_last_error=True)
            gdi = C.WinDLL('gdi32', use_last_error=True)
            pointer = C.c_void_p
            result = C.c_ssize_t
            WNDPROC = C.WINFUNCTYPE(result, W.HWND, W.UINT, W.WPARAM, W.LPARAM)

            class WNDCLASS(C.Structure):
                _fields_ = [('style', W.UINT), ('procedure', WNDPROC), ('class_extra', C.c_int),
                            ('window_extra', C.c_int), ('instance', W.HINSTANCE), ('icon', W.HICON),
                            ('cursor', W.HANDLE), ('background', W.HBRUSH), ('menu', W.LPCWSTR),
                            ('name', W.LPCWSTR)]

            class PAINTSTRUCT(C.Structure):
                _fields_ = [('dc', W.HDC), ('erase', W.BOOL), ('rect', W.RECT),
                            ('restore', W.BOOL), ('incremental', W.BOOL), ('reserved', C.c_byte * 32)]

            class BITMAPINFOHEADER(C.Structure):
                _fields_ = [('size', W.DWORD), ('width', W.LONG), ('height', W.LONG),
                            ('planes', W.WORD), ('bit_count', W.WORD), ('compression', W.DWORD),
                            ('size_image', W.DWORD), ('xppm', W.LONG), ('yppm', W.LONG),
                            ('colors', W.DWORD), ('important', W.DWORD)]

            kernel.GetModuleHandleW.argtypes = [W.LPCWSTR]
            kernel.GetModuleHandleW.restype = W.HMODULE
            user.DefWindowProcW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
            user.DefWindowProcW.restype = result
            user.CreateWindowExW.argtypes = [W.DWORD, W.LPCWSTR, W.LPCWSTR, W.DWORD, C.c_int,
                                            C.c_int, C.c_int, C.c_int, W.HWND, W.HMENU, W.HINSTANCE, pointer]
            user.CreateWindowExW.restype = W.HWND
            user.RegisterClassW.argtypes = [C.POINTER(WNDCLASS)]
            user.RegisterClassW.restype = W.ATOM
            user.BeginPaint.argtypes = [W.HWND, C.POINTER(PAINTSTRUCT)]
            user.BeginPaint.restype = W.HDC
            user.EndPaint.argtypes = [W.HWND, C.POINTER(PAINTSTRUCT)]
            user.SetForegroundWindow.argtypes = [W.HWND]
            user.GetForegroundWindow.restype = W.HWND
            user.SetFocus.argtypes = [W.HWND]
            user.SetFocus.restype = W.HWND
            user.SetActiveWindow.argtypes = [W.HWND]
            user.SetActiveWindow.restype = W.HWND
            user.ShowWindow.argtypes = [W.HWND, C.c_int]
            user.BringWindowToTop.argtypes = [W.HWND]
            user.GetWindowThreadProcessId.argtypes = [W.HWND, C.POINTER(W.DWORD)]
            user.GetWindowThreadProcessId.restype = W.DWORD
            user.AttachThreadInput.argtypes = [W.DWORD, W.DWORD, W.BOOL]
            user.AttachThreadInput.restype = W.BOOL
            kernel.GetCurrentThreadId.restype = W.DWORD
            user.PeekMessageW.argtypes = [C.POINTER(W.MSG), W.HWND, W.UINT, W.UINT, W.UINT]
            user.DestroyWindow.argtypes = [W.HWND]
            user.PostMessageW.argtypes = [W.HWND, W.UINT, W.WPARAM, W.LPARAM]
            user.GetMessageW.argtypes = [C.POINTER(W.MSG), W.HWND, W.UINT, W.UINT]
            user.TranslateMessage.argtypes = [C.POINTER(W.MSG)]
            user.DispatchMessageW.argtypes = [C.POINTER(W.MSG)]
            user.DispatchMessageW.restype = result
            gdi.StretchDIBits.argtypes = [W.HDC] + [C.c_int] * 8 + [pointer, pointer, W.UINT, W.DWORD]
            user.SetProcessDPIAware()
            self.width, self.height = user.GetSystemMetrics(0), user.GetSystemMetrics(1)
            if not (1 <= self.width <= 32768 and 1 <= self.height <= 32768):
                raise AssertionError('No interactive Windows primary display')
            # Noise exercises adaptive JPEG encoding instead of only a blank desktop.
            noise_width, noise_height = 1280, 960
            noise = C.create_string_buffer(random.Random(137).randbytes(noise_width * noise_height * 4))
            bitmap = BITMAPINFOHEADER(C.sizeof(BITMAPINFOHEADER), noise_width, -noise_height, 1, 32, 0, 0, 0, 0, 0, 0)

            @WNDPROC
            def callback(hwnd, message, wparam, lparam):
                if message == 0x000f:  # WM_PAINT
                    paint = PAINTSTRUCT()
                    dc = user.BeginPaint(hwnd, C.byref(paint))
                    gdi.StretchDIBits(dc, 0, 0, self.width, self.height, 0, 0, noise_width,
                                     noise_height, noise, C.byref(bitmap), 0, 0x00cc0020)
                    user.EndPaint(hwnd, C.byref(paint))
                    return 0
                if message in (0x100, 0x101, 0x102, 0x200, 0x201, 0x202, 0x203,
                               0x204, 0x205, 0x207, 0x208, 0x20a, 0x20e):
                    self.events.append((message, int(wparam), int(lparam), bool(user.GetKeyState(0x11) & 0x8000)))
                if message == 0x10:  # WM_CLOSE, only our own window
                    user.DestroyWindow(hwnd)
                    return 0
                if message == 2:
                    user.PostQuitMessage(0)
                    return 0
                return user.DefWindowProcW(hwnd, message, wparam, lparam)

            self.callback = callback
            instance = kernel.GetModuleHandleW(None)
            name = f'PikoNativeTest-{os.getpid()}'
            window_class = WNDCLASS(8, callback, 0, 0, instance, None, None, None, None, name)
            if not user.RegisterClassW(C.byref(window_class)):
                raise C.WinError(C.get_last_error())
            self.hwnd = user.CreateWindowExW(8, name, 'Piko isolated input fixture', 0x90000000,
                                            0, 0, self.width, self.height, None, None, instance, None)
            if not self.hwnd:
                raise C.WinError(C.get_last_error())
            # The ARM runner's launcher can own foreground input initially. Join
            # its input queue only while activating OUR window, then always detach.
            # No synthetic Alt keystroke, unrelated window focus or desktop switch.
            deadline = time.monotonic() + 2
            current_thread = kernel.GetCurrentThreadId()
            while user.GetForegroundWindow() != self.hwnd and time.monotonic() < deadline:
                foreground = user.GetForegroundWindow()
                foreground_thread = user.GetWindowThreadProcessId(foreground, None) if foreground else 0
                attached = False
                try:
                    if foreground_thread and foreground_thread != current_thread:
                        attached = bool(user.AttachThreadInput(current_thread, foreground_thread, True))
                    user.ShowWindow(self.hwnd, 9)
                    user.BringWindowToTop(self.hwnd)
                    user.SetActiveWindow(self.hwnd)
                    user.SetForegroundWindow(self.hwnd)
                    user.SetFocus(self.hwnd)
                finally:
                    if attached:
                        user.AttachThreadInput(current_thread, foreground_thread, False)
                pending = W.MSG()
                while user.PeekMessageW(C.byref(pending), None, 0, 0, 1):
                    user.TranslateMessage(C.byref(pending))
                    user.DispatchMessageW(C.byref(pending))
                if user.GetForegroundWindow() != self.hwnd:
                    time.sleep(.04)
            self.ready.set()
            message = W.MSG()
            while user.GetMessageW(C.byref(message), None, 0, 0) > 0:
                user.TranslateMessage(C.byref(message))
                user.DispatchMessageW(C.byref(message))
        except Exception as error:
            self.error = error
            self.ready.set()

    def require_foreground(self):
        if self.error:
            raise self.error
        if self.user.GetForegroundWindow() != self.hwnd:
            raise AssertionError('Fixture lost foreground; refusing to send desktop input elsewhere')

    def __exit__(self, *_):
        self.user.PostMessageW(self.hwnd, 0x10, 0, 0)
        self.thread.join(5)
        if self.thread.is_alive():
            raise AssertionError('Native fixture did not close')


@unittest.skipUnless(os.name == 'nt' and os.environ.get('PIKO_NATIVE_DESKTOP_TEST') == '1',
                     'Opt-in Windows interactive desktop fixture')
class WindowsNative(unittest.IsolatedAsyncioTestCase):
    async def test_capture_and_real_input_roundtrip(self):
        with WindowsFixture() as fixture:
            await asyncio.sleep(.25)
            fixture.require_foreground()
            screenshot_size = None
            actions = [
                {'action': 'screenshot'}, {'action': 'move'}, {'action': 'click'},
                {'action': 'double_click'}, {'action': 'click', 'button': 'right'},
                {'action': 'type', 'text': 'Piko héllo 世界 🙂'},
                {'action': 'key', 'key': 'ctrl+a'},
                {'action': 'scroll', 'direction': 'down', 'amount': 2},
                {'action': 'scroll', 'direction': 'right', 'amount': 2},
            ]
            async def model(body):
                nonlocal screenshot_size
                done = results(body)
                if done:
                    value = done[-1]
                    if isinstance(value, str):
                        self.assertNotIn('error', json.loads(value))
                    if len(done) == 1:
                        self.assertIsInstance(value, list)
                        image = next(part['image_url'] for part in value if part.get('type') == 'input_image')
                        prefix, encoded = image.split(',', 1)
                        self.assertEqual(prefix, 'data:image/jpeg;base64')
                        jpeg = base64.b64decode(encoded, validate=True)
                        self.assertLessEqual(len(jpeg), 96 * 1024)
                        screenshot_size = jpeg_dimensions(jpeg)
                        width, height = screenshot_size
                        self.assertTrue(1 <= width <= min(1280, fixture.width))
                        self.assertTrue(1 <= height <= min(960, fixture.height))
                        match = re.search(r'Structured content: (\{.*\})', text_result(value))
                        self.assertIsNotNone(match, 'Capture must report actual screenshot geometry')
                        geometry = json.loads(match.group(1))
                        self.assertEqual((geometry['width'], geometry['height']), screenshot_size)
                        self.assertEqual((geometry['native_width'], geometry['native_height']),
                                         (fixture.width, fixture.height))
                if len(done) == len(actions):
                    return final()
                fixture.require_foreground()
                action = dict(actions[len(done)])
                if action['action'] in ('move', 'click', 'double_click'):
                    action.update(x=screenshot_size[0] // 2, y=screenshot_size[1] // 2)
                return response([call('computer_use', action, f'native-{len(done)}')])
            failures = []
            async with Harness(checked_model(model, failures), extra='computer_use = true\n') as harness:
                await harness.initialize()
                result = await harness.prompt(await harness.session())
                if failures:
                    raise failures[0]
                self.assertEqual(result['result']['stopReason'], 'end_turn')
                self.assertEqual(len(harness.permissions), len(actions))
            await asyncio.sleep(.2)
            events = fixture.events
            self.assertTrue(any(event[0] == 0x201 for event in events), 'No actual left click delivered')
            self.assertTrue(any(event[0] == 0x203 for event in events), 'No actual double click delivered')
            self.assertTrue(any(event[0] == 0x204 for event in events), 'No actual right click delivered')
            self.assertTrue(any(event[0] == 0x20a for event in events), 'No actual vertical scroll delivered')
            self.assertTrue(any(event[0] == 0x20e for event in events), 'No actual horizontal scroll delivered')
            self.assertTrue(any(event[0] == 0x100 and event[1] == 65 and event[3] for event in events), 'No Ctrl+A delivered')
            units = b''.join(event[1].to_bytes(2, 'little') for event in events if event[0] == 0x102)
            self.assertIn('Piko héllo 世界 🙂', units.decode('utf-16-le'))
            clicks = [event for event in events if event[0] == 0x201]
            x, y = clicks[0][2] & 0xffff, (clicks[0][2] >> 16) & 0xffff
            self.assertLessEqual(abs(x - fixture.width // 2), max(2, fixture.width // screenshot_size[0]))
            self.assertLessEqual(abs(y - fixture.height // 2), max(2, fixture.height // screenshot_size[1]))


class MacForeground:
    """Inspect/activate only the dedicated fixture process using native AppKit."""
    def __init__(self, pid):
        import ctypes as C
        C.CDLL('/System/Library/Frameworks/AppKit.framework/AppKit')
        objc = C.CDLL('/usr/lib/libobjc.A.dylib')
        objc.objc_getClass.argtypes = [C.c_char_p]
        objc.objc_getClass.restype = C.c_void_p
        objc.sel_registerName.argtypes = [C.c_char_p]
        objc.sel_registerName.restype = C.c_void_p
        self.C, self.objc, self.pid = C, objc, pid
        self.address = C.cast(objc.objc_msgSend, C.c_void_p).value
        running = self.send(objc.objc_getClass(b'NSRunningApplication'),
                            b'runningApplicationWithProcessIdentifier:', C.c_void_p, (C.c_int,), pid)
        if not running:
            raise AssertionError('Dedicated Chrome has no running application identity')
        self.send(running, b'activateWithOptions:', C.c_bool, (C.c_ulong,), 2)

    def send(self, receiver, selector, result, args=(), *values):
        function = self.C.CFUNCTYPE(result, self.C.c_void_p, self.C.c_void_p, *args)(self.address)
        return function(receiver, self.objc.sel_registerName(selector), *values)

    def require_foreground(self):
        workspace = self.send(self.objc.objc_getClass(b'NSWorkspace'), b'sharedWorkspace', self.C.c_void_p)
        application = self.send(workspace, b'frontmostApplication', self.C.c_void_p)
        pid = self.send(application, b'processIdentifier', self.C.c_int)
        if pid != self.pid:
            raise AssertionError(f'Dedicated Chrome lost foreground ({pid}); refusing native input')


@unittest.skipUnless(sys.platform == 'darwin' and os.environ.get('PIKO_NATIVE_DESKTOP_TEST') == '1'
                     and os.environ.get('PIKO_CHROME_BIN'),
                     'Opt-in macOS native desktop fixture requires existing Chrome and OS permissions')
class MacNative(unittest.IsolatedAsyncioTestCase):
    async def test_capture_and_native_chrome_input_roundtrip(self):
        async def page(reader, writer):
            try:
                await reader.readuntil(b'\r\n\r\n')
                body = b'''<!doctype html><title>Piko native input fixture</title>
                <style>body{margin:48px;background:#f3f3f3}textarea{width:500px;height:180px;font-size:20px}
                button{display:block;margin-top:24px;padding:20px}</style>
                <textarea id="field"></textarea><button id="apply">Apply</button>
                <script>window.events=[];window.applied=0;
                document.querySelector('#apply').onclick=()=>window.applied++;
                for(const type of ['mousedown','mousemove','dblclick','contextmenu','wheel','keydown'])
                document.addEventListener(type,e=>{window.events.push({type,key:e.key,meta:e.metaKey,
                button:e.button,dx:e.deltaX,dy:e.deltaY});if(type==='contextmenu'||type==='wheel')e.preventDefault()},
                {passive:false});</script>'''
                writer.write(f'HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n'.encode() + body)
                await writer.drain()
            except (asyncio.IncompleteReadError, ConnectionError):
                pass
            finally:
                writer.close()
        server = await asyncio.start_server(page, '127.0.0.1', 0)
        url = f'http://127.0.0.1:{server.sockets[0].getsockname()[1]}/'
        browser = None
        try:
            with tempfile.TemporaryDirectory(prefix='piko-native-chrome-') as directory:
                env = {key: value for key, value in os.environ.items()
                       if key in ('HOME', 'PATH', 'TMPDIR', 'LANG', 'LC_ALL')}
                browser = await asyncio.create_subprocess_exec(
                    os.environ['PIKO_CHROME_BIN'], '--no-first-run', '--no-default-browser-check',
                    '--disable-background-networking', '--disable-sync', '--disable-session-crashed-bubble',
                    '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0',
                    f'--user-data-dir={directory}', '--window-position=80,80', '--window-size=800,600',
                    f'--app={url}', stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL,
                    env=env, start_new_session=True)
                try:
                    port_file = Path(directory) / 'DevToolsActivePort'
                    for _ in range(200):
                        if port_file.exists():
                            break
                        if browser.returncode is not None:
                            self.fail(f'Dedicated Chrome exited {browser.returncode}')
                        await asyncio.sleep(.05)
                    port = int(port_file.read_text().splitlines()[0])
                    foreground = MacForeground(browser.pid)
                    await asyncio.sleep(.5)
                    foreground.require_foreground()
                    tab, geometry, screenshot = None, None, None
                    unicode_text = 'Piko héllo 世界 🙂'
                    geometry_js = '''new Promise((resolve,reject)=>{const deadline=performance.now()+3000;
                    const poll=()=>{if(document.readyState!=='complete'||!document.querySelector('#field')||
                    !document.querySelector('#apply')||!Array.isArray(window.events)){
                    if(performance.now()>=deadline){reject(new Error('Native fixture page not ready'));return}
                    setTimeout(poll,25);return}
                    const p=e=>{const r=e.getBoundingClientRect();return {
                    x:screenX+(outerWidth-innerWidth)/2+r.x+r.width/2,
                    y:screenY+(outerHeight-innerHeight)+r.y+r.height/2}};resolve({
                    focus:document.hasFocus(),field:p(document.querySelector('#field')),
                    button:p(document.querySelector('#apply'))})};poll()})'''
                    verify_js = '''new Promise(resolve=>setTimeout(()=>resolve({
                    value:document.querySelector('#field').value,
                    events:window.events,applied:window.applied}),100))'''
                    actions = [
                        ('browser', {'action': 'tabs'}),
                        # Navigate waits for this target's Page.loadEventFired,
                        # then bounded DOM polling confirms the fixture script ran.
                        ('browser', {'action': 'navigate', 'url': url}),
                        ('browser', {'action': 'evaluate', 'expression': geometry_js}),
                        ('computer_use', {'action': 'screenshot'}),
                        ('computer_use', {'action': 'move', 'target': 'field'}),
                        ('computer_use', {'action': 'click', 'target': 'field'}),
                        ('computer_use', {'action': 'double_click', 'target': 'field'}),
                        ('computer_use', {'action': 'click', 'target': 'field', 'button': 'right'}),
                        ('computer_use', {'action': 'type', 'text': unicode_text}),
                        ('browser', {'action': 'evaluate', 'expression': verify_js}),
                        ('computer_use', {'action': 'key', 'key': 'cmd+a'}),
                        ('computer_use', {'action': 'type', 'text': 'replacement'}),
                        ('computer_use', {'action': 'scroll', 'direction': 'down', 'amount': 2}),
                        ('computer_use', {'action': 'scroll', 'direction': 'right', 'amount': 2}),
                        ('computer_use', {'action': 'click', 'target': 'button'}),
                        ('browser', {'action': 'evaluate', 'expression': verify_js}),
                    ]

                    async def model(body):
                        nonlocal tab, geometry, screenshot
                        done = results(body)
                        if done:
                            value = done[-1]
                            if isinstance(value, str):
                                parsed = json.loads(value)
                                previous_tool, previous_args = actions[len(done) - 1]
                                self.assertNotIn('error', parsed,
                                                 f'Tool {len(done)} {previous_tool}/{previous_args["action"]}: {parsed}')
                                if len(done) == 1:
                                    tab = next(item['tab_id'] for item in parsed['results'] if item['url'] == url)
                                elif len(done) == 3:
                                    geometry = json.loads(parsed['content'])
                                    self.assertTrue(geometry['focus'], 'Fixture document is not focused')
                                elif len(done) in (10, 16):
                                    state = json.loads(parsed['content'])
                                    self.assertEqual(state['value'], unicode_text if len(done) == 10 else 'replacement')
                                    if len(done) == 16:
                                        self.assertEqual(state['applied'], 1)
                                        events = state['events']
                                        self.assertTrue(any(e['type'] == 'mousemove' for e in events))
                                        self.assertTrue(any(e['type'] == 'dblclick' for e in events))
                                        self.assertTrue(any(e['type'] == 'contextmenu' for e in events))
                                        self.assertTrue(any(e['type'] == 'keydown' and e['meta'] and e['key'] == 'a' for e in events))
                                        self.assertTrue(any(e['type'] == 'wheel' and e['dy'] != 0 for e in events))
                                        self.assertTrue(any(e['type'] == 'wheel' and e['dx'] != 0 for e in events))
                            if len(done) == 4:
                                self.assertIsInstance(value, list)
                                data = next(p['image_url'] for p in value if p.get('type') == 'input_image')
                                jpeg = base64.b64decode(data.split(',', 1)[1], validate=True)
                                self.assertLessEqual(len(jpeg), 96 * 1024)
                                dimensions = jpeg_dimensions(jpeg)
                                match = re.search(r'Structured content: (\{.*\})', text_result(value))
                                self.assertIsNotNone(match)
                                screenshot = json.loads(match.group(1))
                                self.assertEqual(dimensions, (screenshot['width'], screenshot['height']))
                        if len(done) == len(actions):
                            return final()
                        tool, args = actions[len(done)]
                        args = dict(args)
                        if tool == 'browser' and tab and args['action'] != 'tabs':
                            args['tab_id'] = tab
                        if tool == 'computer_use' and args['action'] != 'screenshot':
                            foreground.require_foreground()
                            if 'target' in args:
                                point = geometry[args.pop('target')]
                                args.update(x=round(point['x'] * screenshot['width'] / screenshot['native_width']),
                                            y=round(point['y'] * screenshot['height'] / screenshot['native_height']))
                        return response([call(tool, args, f'mac-native-{len(done)}')])

                    failures = []
                    async with Harness(checked_model(model, failures), extra=f'computer_use = true\nbrowser_cdp_url = "http://127.0.0.1:{port}"\n') as harness:
                        # Check focus again at the actual approval boundary, before OS input.
                        async def guard(permission):
                            if 'computer_use' in json.dumps(permission):
                                try:
                                    foreground.require_foreground()
                                except AssertionError:
                                    harness.allow = False
                        harness.permission_hook = guard
                        await harness.initialize()
                        result = await harness.prompt(await harness.session())
                        if failures:
                            raise failures[0]
                        self.assertEqual(result['result']['stopReason'], 'end_turn')
                        self.assertEqual(len(harness.permissions), len(actions))
                finally:
                    if browser.returncode is None:
                        os.killpg(browser.pid, signal.SIGTERM)
                        await asyncio.wait_for(browser.wait(), 10)
        finally:
            server.close()
            await server.wait_closed()


if __name__ == '__main__':
    unittest.main()
