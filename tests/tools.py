"""Public ACP integration checks for bounded native tools. No paid services."""
import asyncio
import base64
import json
import os
import unittest
from stdio import Harness, call, response, final, FIXTURE_JPEG


def outputs(body):
    return [item['output'] for item in body['input'] if item.get('type') == 'function_call_output']


def text_output(value):
    if isinstance(value, list):
        return next(part['text'] for part in value if part.get('type') == 'input_text')
    return value


class ToolTests(unittest.IsolatedAsyncioTestCase):
    async def test_patch_updates_moves_adds_deletes_and_images_reach_model(self):
        patch = '*** Begin Patch\n*** Update File: a.txt\n*** Move to: moved.txt\n@@\n-old\n+new\n*** Add File: added.txt\n+hello\n*** Delete File: gone.txt\n*** End Patch'
        async def model(body):
            done = outputs(body)
            if not done:
                return response([call('apply_patch', {'patch': patch}, 'patch')])
            if len(done) == 1:
                self.assertEqual(json.loads(done[0])['status'], 'applied')
                return response([call('view_image', {'path': 'pixel.jpg'}, 'image')])
            self.assertIsInstance(done[-1], list)
            self.assertTrue(any(part.get('type') == 'input_image' for part in done[-1]))
            return final()
        async with Harness(model) as h:
            (h.workspace / 'a.txt').write_text('old\n')
            (h.workspace / 'gone.txt').write_text('remove\n')
            (h.workspace / 'pixel.jpg').write_bytes(base64.b64decode(FIXTURE_JPEG))
            await h.initialize()
            result = await h.prompt(await h.session())
            self.assertEqual(result['result']['stopReason'], 'end_turn')
            self.assertEqual((h.workspace / 'moved.txt').read_text(), 'new\n')
            self.assertEqual((h.workspace / 'added.txt').read_text(), 'hello\n')
            self.assertFalse((h.workspace / 'a.txt').exists())
            self.assertFalse((h.workspace / 'gone.txt').exists())
            self.assertEqual(len(h.permissions), 1)
            updates = [v['params']['update'] for v in h.updates if v.get('method') == 'session/update' and v['params']['update'].get('sessionUpdate') == 'tool_call_update' and v['params']['update'].get('toolCallId') == 'image']
            self.assertEqual(updates[-1]['rawOutput']['imageCount'], 1)

    async def test_patch_denied_or_stale_does_not_change_any_file(self):
        patch = '*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** Add File: other.txt\n+hello\n*** End Patch'
        for stale in (False, True):
            async def model(body):
                done = outputs(body)
                if not done:
                    return response([call('apply_patch', {'patch': patch})])
                self.assertIn('error', json.loads(done[-1]))
                return final()
            async with Harness(model, allow=stale) as h:
                (h.workspace / 'a.txt').write_text('old\n')
                if stale:
                    async def changed(_):
                        (h.workspace / 'a.txt').write_text('human edit\n')
                    h.permission_hook = changed
                await h.initialize()
                await h.prompt(await h.session())
                self.assertEqual((h.workspace / 'a.txt').read_text(), 'human edit\n' if stale else 'old\n')
                self.assertFalse((h.workspace / 'other.txt').exists())

    async def test_optional_and_readonly_tool_capabilities(self):
        async def model(body):
            names = {tool['name'] for tool in body['tools']}
            self.assertIn('view_image', names)
            self.assertNotIn('browser', names)
            self.assertNotIn('web_fetch', names)
            self.assertNotIn('web_search', names)
            if body['input'][0]['content'].strip() == 'readonly':
                for name in ('apply_patch', 'terminal_start', 'terminal_write', 'shell'):
                    self.assertNotIn(name, names)
            return final()
        async with Harness(model, extra='web_enabled = false\n') as h:
            await h.initialize()
            session = await h.session()
            await h.rpc('session/set_mode', {'sessionId': session, 'modeId': 'plan'})
            await h.prompt(session, 'readonly')

    async def test_running_agent_messages_and_completed_followups(self):
        child_requests = []
        async def model(body):
            first = body['input'][0].get('content', '').strip()
            if first == 'child task':
                child_requests.append(body)
                if len(child_requests) == 1:
                    await asyncio.sleep(.15)
                    return final('initial reply')
                if len(child_requests) == 2:
                    self.assertTrue(any('steering message' in str(item.get('content', '')) for item in body['input']))
                    return final('steered reply')
                self.assertTrue(any('second task' in str(item.get('content', '')) for item in body['input']))
                return final('followup reply')
            done = outputs(body)
            if not done:
                return response([call('spawn_agent', {'prompt': 'child task', 'role': 'explore', 'background': True}, 'spawn')])
            task = json.loads(done[0])['task_id']
            if len(done) == 1:
                return response([call('message_agent', {'task_id': task, 'message': 'steering message'}, 'message'), call('task_status', {'task_id': task, 'wait_ms': 30000}, 'wait')])
            if len(done) == 3:
                self.assertEqual(json.loads(done[-1])['output'], 'steered reply')
                return response([call('followup_agent', {'task_id': task, 'message': 'second task'}, 'followup'), call('task_status', {'task_id': task, 'wait_ms': 30000}, 'wait2')])
            self.assertEqual(json.loads(done[-1])['output'], 'followup reply')
            return final()
        async with Harness(model) as h:
            await h.initialize()
            result = await h.prompt(await h.session(), 'parent task')
            self.assertEqual(result['result']['stopReason'], 'end_turn')
            self.assertEqual(len(child_requests), 3)

    async def test_cancel_new_parent_turn_stops_resumed_child(self):
        phase = 0
        child_count = 0
        child_running = asyncio.Event()
        async def model(body):
            nonlocal child_count
            if body['input'][0].get('content', '').strip() == 'resumable child':
                child_count += 1
                if child_count == 2:
                    child_running.set()
                    await asyncio.sleep(2)
                return final('child result')
            done = outputs(body)
            if phase == 0:
                if not done:
                    return response([call('spawn_agent', {'prompt': 'resumable child', 'role': 'explore', 'background': False}, 'spawn')])
                return final()
            task = json.loads(done[0])['task_id']
            return response([call('followup_agent', {'task_id': task, 'message': 'continue'}, 'follow'), call('task_status', {'task_id': task, 'wait_ms': 30000}, 'wait')])
        async with Harness(model) as h:
            await h.initialize()
            session = await h.session()
            await h.prompt(session, 'initial turn')
            phase = 1
            prompt = asyncio.create_task(h.prompt(session, 'resume turn'))
            await asyncio.wait_for(child_running.wait(), 5)
            await h.rpc('session/cancel', {'sessionId': session})
            result = await prompt
            self.assertEqual(result['result']['stopReason'], 'cancelled')
            for _ in range(50):
                tasks = (await h.rpc('_harness/tasks', {'sessionId': session}))['result']['tasks']
                if tasks[0]['status'] == 'cancelled':
                    break
                await asyncio.sleep(.01)
            self.assertEqual(tasks[0]['status'], 'cancelled')

    async def test_interactive_terminal_approvals_resize_input_and_output(self):
        command = "if ([Console]::IsInputRedirected) {exit 9}; [Console]::Write('ready>'); $line = [Console]::ReadLine(); [Console]::WriteLine('reply:' + $line)" if os.name == 'nt' else "test -t 0 && test -t 1 || exit 9; printf 'ready>'; IFS= read -r line; printf 'reply:%s\\n' \"$line\"; stty size"
        await self._terminal_roundtrip(command)

    @unittest.skipUnless(os.name == 'nt', 'Windows PowerShell host API')
    async def test_windows_powershell_read_host_roundtrip(self):
        await self._terminal_roundtrip("if ([Console]::IsInputRedirected) {exit 9}; Write-Host 'ready>'; $line = Read-Host; Write-Host ('reply:' + $line)")

    async def _terminal_roundtrip(self, command):
        async def model(body):
            done = outputs(body)
            if not done:
                return response([call('terminal_start', {'command': command, 'timeout_secs': 30 if os.name == 'nt' else 10, 'rows': 24, 'cols': 80}, 'start')])
            task = json.loads(done[0])['task_id']
            self.assertTrue(all('error' not in json.loads(value) for value in done))
            if len(done) == 1:
                return response([call('terminal_resize', {'task_id': task, 'rows': 41, 'cols': 101}, 'resize')])
            if len(done) == 2:
                return response([call('terminal_write', {'task_id': task, 'data': 'test-input\n'}, 'write')])
            if len(done) == 3 or json.loads(done[-1])['status'] == 'running':
                return response([call('task_status', {'task_id': task, 'wait_ms': 2000}, f'wait-{len(done)}')])
            result = json.loads(done[-1])
            self.assertEqual(result['status'], 'completed')
            self.assertEqual(result['exit_code'], 0)
            self.assertIn('reply:test-input', result['output'])
            if os.name != 'nt':
                self.assertIn('41 101', result['output'])
            return final()
        async with Harness(model) as h:
            await h.initialize()
            result = await h.prompt(await h.session())
            self.assertEqual(result['result']['stopReason'], 'end_turn')
            self.assertEqual(len(h.permissions), 2)

    async def test_terminal_denial_never_starts_a_task(self):
        async def model(body):
            done = outputs(body)
            if not done:
                return response([call('terminal_start', {'command': 'echo must-not-run', 'timeout_secs': 10, 'rows': 24, 'cols': 80})])
            self.assertIn('error', json.loads(done[-1]))
            return final()
        async with Harness(model, allow=False) as h:
            await h.initialize()
            session = await h.session()
            await h.prompt(session)
            tasks = await h.rpc('_harness/tasks', {'sessionId': session})
            self.assertEqual(tasks['result']['tasks'], [])
            self.assertEqual(len(h.permissions), 1)

    async def test_private_web_addresses_and_disabled_browser_fail_without_network(self):
        async def model(body):
            done = outputs(body)
            if not done:
                return response([call('web_fetch', {'url': 'http://127.0.0.1/private'}, 'web'), call('browser', {'action': 'snapshot', 'tab_id': 'any'}, 'browser')])
            self.assertEqual(len(done), 2)
            self.assertTrue(all('error' in json.loads(value) for value in done))
            return final()
        async with Harness(model) as h:
            await h.initialize()
            await h.prompt(await h.session())
            self.assertEqual(len(h.permissions), 0)


if __name__ == '__main__':
    unittest.main()
