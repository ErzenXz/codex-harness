"""Opt-in smoke test against an existing Chrome/Edge; no browser download or real model."""
import asyncio
import json
import os
from pathlib import Path
import signal
import tempfile
import unittest
from stdio import Harness, call, response, final


@unittest.skipUnless(os.environ.get('PIKO_CHROME_BIN'), 'Set PIKO_CHROME_BIN to an installed Chromium browser')
class BrowserLive(unittest.IsolatedAsyncioTestCase):
    async def test_dedicated_browser_full_roundtrip(self):
        async def page(reader, writer):
            try:
                await reader.readuntil(b'\r\n\r\n')
            except (asyncio.IncompleteReadError, ConnectionError):
                writer.close()
                return
            body = b'<html><title>Piko test</title><body><h1>Local fixture</h1><input id="name"><button id="go" onclick="document.querySelector(\'h1\').textContent=document.querySelector(\'#name\').value">Apply</button></body></html>'
            writer.write(f'HTTP/1.1 200 OK\r\nContent-Length: {len(body)}\r\nContent-Type: text/html\r\nConnection: close\r\n\r\n'.encode() + body)
            await writer.drain()
            writer.close()
        server = await asyncio.start_server(page, '127.0.0.1', 0)
        url = f'http://127.0.0.1:{server.sockets[0].getsockname()[1]}/'
        with tempfile.TemporaryDirectory(prefix='piko-browser-test-') as directory:
            env = {key: value for key, value in os.environ.items() if key in ('HOME', 'PATH', 'TMPDIR', 'TEMP', 'TMP', 'SYSTEMROOT', 'LOCALAPPDATA', 'USERPROFILE')}
            browser = await asyncio.create_subprocess_exec(os.environ['PIKO_CHROME_BIN'], '--headless=new', '--no-first-run', '--no-default-browser-check', '--disable-background-networking', '--disable-sync', '--remote-debugging-address=127.0.0.1', '--remote-debugging-port=0', f'--user-data-dir={directory}', 'about:blank', stdout=asyncio.subprocess.DEVNULL, stderr=asyncio.subprocess.DEVNULL, env=env, start_new_session=os.name != 'nt')
            try:
                port_file = Path(directory) / 'DevToolsActivePort'
                for _ in range(200):
                    if port_file.exists():
                        break
                    if browser.returncode is not None:
                        self.fail(f'Browser exited {browser.returncode}')
                    await asyncio.sleep(.05)
                port = int(port_file.read_text().splitlines()[0])
                actions = [
                    {'action': 'navigate', 'url': url},
                    {'action': 'snapshot'},
                    {'action': 'type', 'selector': '#name', 'text': 'Piko works'},
                    {'action': 'click', 'selector': '#go'},
                    {'action': 'evaluate', 'expression': 'document.querySelector("h1").textContent'},
                    {'action': 'screenshot'},
                    {'action': 'close'},
                ]
                tab = None
                async def model(body):
                    nonlocal tab
                    done = [item['output'] for item in body['input'] if item.get('type') == 'function_call_output']
                    if done:
                        value = done[-1]
                        if isinstance(value, str):
                            parsed = json.loads(value)
                            self.assertNotIn('error', parsed)
                            tab = parsed.get('tab_id', tab)
                            if len(done) == 2:
                                self.assertIn('Local fixture', parsed['content'])
                            if len(done) == 5:
                                self.assertIn('Piko works', parsed['content'])
                        if len(done) == 6:
                            self.assertIsInstance(value, list)
                            self.assertTrue(any(part.get('type') == 'input_image' for part in value))
                    if len(done) == len(actions):
                        return final()
                    args = dict(actions[len(done)])
                    if tab:
                        args['tab_id'] = tab
                    return response([call('browser', args, f'browser-{len(done)}')])
                async with Harness(model, extra=f'browser_cdp_url = "http://127.0.0.1:{port}"\n') as h:
                    await h.initialize()
                    result = await h.prompt(await h.session())
                    self.assertEqual(result['result']['stopReason'], 'end_turn')
                    self.assertEqual(len(h.permissions), len(actions))
            finally:
                if browser.returncode is None:
                    if os.name == 'nt':
                        browser.terminate()
                    else:
                        os.killpg(browser.pid, signal.SIGTERM)
                    await asyncio.wait_for(browser.wait(), 10)
                server.close()
                await server.wait_closed()


if __name__ == '__main__':
    unittest.main()
