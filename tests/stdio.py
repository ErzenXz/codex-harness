"""Black-box ACP/HTTP tests. Standard library only; never calls a real model."""
import asyncio
import base64
import json
import os
from pathlib import Path
import signal
import sys
import tempfile
import unittest

BINARY = Path(os.environ.get("HARNESS_BIN", "target/debug/codex-harness" + (".exe" if os.name == "nt" else ""))).resolve()
FIXTURE_JPEG="/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AKp//2Q=="


def response(output, text="", incomplete=False):
    events = []
    if text:
        events.append({"type": "response.output_text.delta", "delta": text})
    events.append({"type": "response.incomplete" if incomplete else "response.completed", "response": {
        "output": output, "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 900}, "output_tokens": 20}}})
    return "".join("data: " + json.dumps(event) + "\n\n" for event in events)


def final(text="done"):
    return response([{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}]}], text)


def call(name, args, ident="call-1"):
    return {"type": "function_call", "call_id": ident, "name": name, "arguments": json.dumps(args)}


def mcp_tool(body,prefix):
    return next(t["name"] for t in body["tools"] if t["name"].startswith(prefix))


def process_alive(pid):
    if os.name != "nt":
        os.kill(pid, 0)
        return
    # os.kill(pid, 0) on Windows can terminate a process; query its handle instead.
    import ctypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.restype = ctypes.c_void_p
    handle = kernel.OpenProcess(0x1000, False, pid)
    if not handle:
        raise ProcessLookupError(pid)
    try:
        code = ctypes.c_ulong()
        if not kernel.GetExitCodeProcess(ctypes.c_void_p(handle), ctypes.byref(code)) or code.value != 259:
            raise ProcessLookupError(pid)
    finally:
        kernel.CloseHandle(ctypes.c_void_p(handle))


class Harness:
    def __init__(self, handler, extra="", allow=True, content_type="text/event-stream"):
        self.handler, self.extra, self.allow = handler, extra, allow
        self.content_type, self.request_headers = content_type, []
        self.pending, self.updates, self.unmatched, self.requests = {}, [], [], []
        self.active = self.peak_active = self.connections = 0
        self.tasks, self.writers = set(), set()
        self.permission_hook = None
        self.permissions = []
        self.seen = asyncio.Event()

    async def http(self, reader, writer):
        self.connections += 1
        self.writers.add(writer)
        self.tasks.add(asyncio.current_task())
        try:
            while await reader.readline():
                headers = {}
                while (line := await reader.readline()) != b"\r\n":
                    if not line:
                        return
                    key, value = line.decode().split(":", 1)
                    headers[key.lower()] = value.strip()
                body = json.loads(await reader.readexactly(int(headers["content-length"])))
                self.request_headers.append(headers)
                self.requests.append(body)
                self.seen.set()
                self.active += 1
                self.peak_active = max(self.active, self.peak_active)
                try:
                    result = await self.handler(body)
                    payload = result.encode()
                    content_type = f"Content-Type: {self.content_type}\r\n" if self.content_type else ""
                    writer.write(f"HTTP/1.1 200 OK\r\n{content_type}Content-Length: {len(payload)}\r\n\r\n".encode())
                    # Exercise SSE boundaries, not only whole events per network read.
                    for start in range(0, len(payload), 31):
                        writer.write(payload[start:start + 31])
                        await writer.drain()
                finally:
                    self.active -= 1
        except (ConnectionError, asyncio.IncompleteReadError):
            pass
        finally:
            self.writers.discard(writer)
            writer.close()
            self.tasks.discard(asyncio.current_task())

    async def __aenter__(self):
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.workspace = self.root / "workspace"
        self.workspace.mkdir()
        self.server = await asyncio.start_server(self.http, "127.0.0.1", 0)
        port = self.server.sockets[0].getsockname()[1]
        config = self.root / "config.toml"
        config.write_text(f'auth = "none"\nbase_url = "http://127.0.0.1:{port}/v1"\nstate_dir = {json.dumps(str(self.root / "state"))}\n{self.extra}')
        env = {k: v for k, v in os.environ.items() if k not in ("OPENAI_BASE_URL", "OPENAI_MODEL", "CODEX_HARNESS_STATE_DIR")}
        self.process = await asyncio.create_subprocess_exec(str(BINARY), "--config", str(config), env=env,
            stdin=asyncio.subprocess.PIPE, stdout=asyncio.subprocess.PIPE, stderr=asyncio.subprocess.PIPE, limit=2**21)
        self.reader = asyncio.create_task(self.read())
        return self

    async def read(self):
        try:
            while line := await self.process.stdout.readline():
                value = json.loads(line)  # Any banner/status text fails here.
                if value.get("method") == "session/request_permission":
                    self.permissions.append(value)
                    if self.permission_hook:
                        await self.permission_hook(value)
                    await self.send({"jsonrpc": "2.0", "id": value["id"], "result": {"outcome": {
                        "outcome": "selected", "optionId": "allow-once" if self.allow else "reject-once"}}})
                elif "method" in value:
                    self.updates.append(value)
                elif value.get("id") in self.pending:
                    self.pending.pop(value["id"]).set_result(value)
                else:
                    self.unmatched.append(value)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(RuntimeError("ACP stream ended"))

    async def send(self, value):
        self.process.stdin.write((json.dumps(value) + "\n").encode())
        await self.process.stdin.drain()

    async def rpc(self, method, params):
        ident = f"request-{getattr(self, 'sequence', 0)}"
        self.sequence = getattr(self, "sequence", 0) + 1
        future = asyncio.get_running_loop().create_future()
        self.pending[ident] = future
        await self.send({"jsonrpc": "2.0", "id": ident, "method": method, "params": params})
        return await asyncio.wait_for(future, 20)

    async def initialize(self):
        result = await self.rpc("initialize", {"protocolVersion": 1})
        assert result["result"]["protocolVersion"] == 1, result

    async def session(self):
        result = await self.rpc("session/new", {"cwd": str(self.workspace), "mcpServers": []})
        assert "error" not in result, result
        return result["result"]["sessionId"]

    async def prompt(self, session, text="do the work"):
        return await self.rpc("session/prompt", {"sessionId": session, "prompt": [{"type": "text", "text": text}]})

    async def __aexit__(self, *exc):
        if self.process.returncode is None:
            self.process.stdin.close()
            try:
                await asyncio.wait_for(self.process.wait(), 5)
            except TimeoutError:
                self.process.kill()
                await self.process.wait()
        await self.reader
        self.stderr = (await self.process.stderr.read()).decode()
        self.server.close()
        await self.server.wait_closed()
        for writer in list(self.writers):
            writer.close()
        for task in list(self.tasks):
            task.cancel()
        await asyncio.gather(*self.tasks, return_exceptions=True)
        self.temp.cleanup()
        assert not self.stderr, self.stderr
        assert self.process.returncode == 0, self.process.returncode


class StdioTests(unittest.IsolatedAsyncioTestCase):
    async def test_mcp_images_dynamic_tools_ping_and_name_collisions(self):
        for mode in ["image","dynamic","ping","collision","error"]:
            async def model(body):
                names=[t["name"] for t in body["tools"] if t["name"].startswith("mcp_")]
                self.assertEqual(len(names),len(set(names)))
                if mode=="collision": self.assertEqual(len(names),3)
                last=body["input"][-1]
                if last.get("type")=="function_call_output":
                    if mode=="image":
                        self.assertIsInstance(last["output"],list)
                        self.assertTrue(any(p.get("image_url","").startswith("data:image/png;base64,") for p in last["output"]))
                    if mode=="error": self.assertIn("Tool reported an error",str(last["output"]))
                    return final()
                return response([call(names[0],{"text":"MCP fixture"})])
            async with Harness(model) as h:
                await h.initialize()
                log=h.root/"mcp.log"
                new=await h.rpc("session/new",{"cwd":str(h.workspace),"mcpServers":[{"name":"fixture","command":sys.executable,"args":[str(Path(__file__).with_name("mcp_server.py")),str(log),mode]}]})
                s=new["result"]["sessionId"]
                for _ in range(2 if mode=="dynamic" else 1):
                    result=await h.prompt(s)
                    self.assertEqual(result["result"]["stopReason"],"end_turn",result)
                entries=[json.loads(line) for line in log.read_text().splitlines()]
                if mode=="dynamic": self.assertEqual(sum(x["method"]=="tools/list" for x in entries),2)
                if mode=="ping": self.assertTrue(any(x["method"]=="client-response" for x in entries))
                if mode=="error": self.assertTrue(any(u["params"].get("update",{}).get("status")=="failed" for u in h.updates))

    async def test_mcp_cancellation_is_prompt_and_last_unload_stops_shared_server(self):
        async def model(body):
            return response([call(mcp_tool(body,"mcp_fixture_echo_"),{"text":"waiting"})])
        async with Harness(model) as h:
            await h.initialize()
            log=h.root/"mcp.log"
            new=await h.rpc("session/new",{"cwd":str(h.workspace),"mcpServers":[{"name":"fixture","command":sys.executable,"args":[str(Path(__file__).with_name("mcp_server.py")),str(log),"hold"]}]})
            s=new["result"]["sessionId"]
            prompt=asyncio.create_task(h.prompt(s))
            for _ in range(200):
                if log.exists() and '"method": "tools/call"' in log.read_text(): break
                await asyncio.sleep(.01)
            else: self.fail("MCP tool did not start")
            await h.rpc("session/cancel",{"sessionId":s})
            result=await asyncio.wait_for(prompt,2)
            self.assertEqual(result["result"]["stopReason"],"cancelled",result)
            self.assertIn("notifications/cancelled",log.read_text())
            pid=json.loads(log.read_text().splitlines()[0])["pid"]
            await h.rpc("_harness/session/unload",{"sessionId":s})
            for _ in range(200):
                try: process_alive(pid)
                except ProcessLookupError: break
                await asyncio.sleep(.01)
            else: self.fail("Unloading the last MCP user left its process alive")

    @unittest.skipUnless(sys.platform.startswith("linux"), "Native desktop driver requires Linux X11")
    async def test_computer_actions_multimodal_output_scaling_and_literal_arguments(self):
        actions=[{"action":"screenshot"},{"action":"click","x":0,"y":0},{"action":"move","x":0,"y":0},{"action":"double_click","x":0,"y":0,"button":"right"},{"action":"type","text":"literal ; $(not-a-command) --text"},{"action":"key","key":"ctrl+l"},{"action":"scroll","direction":"down","amount":2}]
        async def model(body):
            self.assertIn("# Computer use",body["instructions"])
            self.assertTrue(any(t["name"]=="computer_use" for t in body["tools"]))
            outputs=[i for i in body["input"] if i.get("type")=="function_call_output"]
            if outputs:
                self.assertIsInstance(outputs[0]["output"],list)
                self.assertTrue(any(part.get("image_url","").startswith("data:image/jpeg;base64,") for part in outputs[0]["output"]))
                self.assertFalse(any('"error"' in json.dumps(o["output"]) for o in outputs))
            return response([call("computer_use",actions[len(outputs)],f"c-{len(outputs)}")]) if len(outputs)<len(actions) else final()
        with tempfile.TemporaryDirectory() as temp:
            root=Path(temp)
            log=root/"driver.log"
            pointer=root/"pointer"
            pointer.write_text("#!/usr/bin/python3\nimport json,os,sys\nwith open("+repr(str(log))+",'a') as f: f.write(json.dumps({'args':sys.argv[1:],'api_key_inherited':'OPENAI_API_KEY' in os.environ})+'\\n')\nif sys.argv[1]=='getdisplaygeometry': print('1 1')\n")
            capture=root/"capture"
            capture.write_text("#!/usr/bin/python3\nimport base64,sys\nsys.stdout.buffer.write(base64.b64decode("+repr(FIXTURE_JPEG)+"))\n")
            pointer.chmod(0o700)
            capture.chmod(0o700)
            config=f'computer_use = true\ncomputer_xdotool = {json.dumps(str(pointer))}\ncomputer_import = {json.dumps(str(capture))}\n'
            async with Harness(model,extra=config) as h:
                await h.initialize()
                s=await h.session()
                result=await h.prompt(s)
                self.assertEqual(result["result"]["stopReason"],"end_turn",result)
                self.assertEqual(len(h.permissions),len(actions))
                self.assertTrue(any(u.get("params",{}).get("update",{}).get("sessionUpdate")=="tool_call_update" and any(c.get("content",{}).get("type")=="image" for c in u["params"]["update"].get("content",[])) for u in h.updates))
                await h.rpc("_harness/session/unload",{"sessionId":s})
                loaded=await h.rpc("session/load",{"sessionId":s,"cwd":str(h.workspace),"mcpServers":[]})
                self.assertNotIn("error",loaded,loaded)
            entries=[json.loads(line) for line in log.read_text().splitlines()]
            self.assertTrue(all(not x["api_key_inherited"] for x in entries))
            self.assertIn(["type","--clearmodifiers","--delay","0","--",actions[4]["text"]],[x["args"] for x in entries])

    async def test_computer_disabled_readonly_and_denied_cannot_start_backend(self):
        for extra,mode,allow,expected_permissions in [("","code",True,0),("computer_use = true\n","plan",True,0),("computer_use = true\n","code",False,1)]:
            async def model(body):
                if any(i.get("type")=="function_call_output" for i in body["input"]): return final()
                return response([call("computer_use",{"action":"screenshot"})])
            async with Harness(model,extra=extra+'computer_xdotool = "/nonexistent/must-not-execute"\n',allow=allow) as h:
                await h.initialize()
                s=await h.session()
                await h.rpc("session/set_mode",{"sessionId":s,"modeId":mode})
                result=await h.prompt(s)
                self.assertEqual(result["result"]["stopReason"],"end_turn",result)
                self.assertEqual(len(h.permissions), expected_permissions if sys.platform.startswith("linux") else 0)
                self.assertNotIn("backend unavailable",json.dumps(h.requests))

    async def test_mcp_http_json_sse_pagination_and_negotiated_headers(self):
        methods=[]
        async def remote(reader,writer):
            try:
                await reader.readline()
                headers={}
                while (line:=await reader.readline()) != b"\r\n":
                    if not line: return
                    key,value=line.decode().split(":",1)
                    headers[key.lower()]=value.strip()
                body=json.loads(await reader.readexactly(int(headers["content-length"])))
                methods.append(body["method"])
                assert "application/json" in headers["accept"] and "text/event-stream" in headers["accept"]
                assert headers["authorization"]=="Bearer fixture-only"
                if body["method"]!="initialize":
                    assert headers["mcp-session-id"]=="fixture-session"
                    assert headers["mcp-protocol-version"]=="2025-06-18"
                if body["method"]=="notifications/initialized":
                    writer.write(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                else:
                    if body["method"]=="initialize":
                        result={"protocolVersion":"2025-06-18","capabilities":{"tools":{}},"serverInfo":{"name":"remote","version":"1"}}
                    elif body["method"]=="tools/list":
                        result={"tools":[]} if "cursor" in body["params"] else {"tools":[{"name":"echo","description":"Fixture","inputSchema":{"type":"object","properties":{}}}],"nextCursor":"page2"}
                    else:
                        result={"content":[{"type":"text","text":"http MCP result"}],"structuredContent":{"ok":True}}
                    frame={"jsonrpc":"2.0","id":body["id"],"result":result}
                    sse=body["method"]=="tools/call"
                    data=(("data: "+json.dumps(frame)+"\n\n") if sse else json.dumps(frame)).encode()
                    mime="text/event-stream" if sse else "application/json"
                    writer.write(f"HTTP/1.1 200 OK\r\nMcp-Session-Id: fixture-session\r\nContent-Type: {mime}\r\nContent-Length: {len(data)}\r\nConnection: close\r\n\r\n".encode()+data)
                await writer.drain()
            finally:
                writer.close()
        async def model(body):
            if any(i.get("type")=="function_call_output" for i in body["input"]):
                self.assertIn("http MCP result",json.dumps(body["input"]))
                return final()
            return response([call(mcp_tool(body,"mcp_remote_echo_"),{})])
        server=await asyncio.start_server(remote,"127.0.0.1",0)
        try:
            async with Harness(model) as h:
                await h.initialize()
                new=await h.rpc("session/new",{"cwd":str(h.workspace),"mcpServers":[{"name":"remote","type":"http","url":f"http://127.0.0.1:{server.sockets[0].getsockname()[1]}/mcp","headers":[{"name":"Authorization","value":"Bearer fixture-only"}]}]})
                result=await h.prompt(new["result"]["sessionId"])
                self.assertEqual(result["result"]["stopReason"],"end_turn",result)
                self.assertEqual(len(h.permissions),1)
        finally:
            server.close()
            await server.wait_closed()
        self.assertEqual(methods,["initialize","notifications/initialized","tools/list","tools/list","tools/call"])

    async def test_mcp_stdio_lazy_shared_approved_and_environment_isolated(self):
        async def model(body):
            self.assertTrue(any(t["name"].startswith("mcp_fixture_echo_") for t in body["tools"]))
            if any(i.get("type")=="function_call_output" for i in body["input"]):
                self.assertIn("hello MCP",json.dumps(body["input"]))
                return final()
            return response([call(mcp_tool(body,"mcp_fixture_echo_"),{"text":"hello MCP"})])
        async with Harness(model) as h:
            await h.initialize()
            log=h.root/"mcp.log"
            servers=[{"name":"fixture","command":sys.executable,"args":[str(Path(__file__).with_name("mcp_server.py")),str(log)],"env":[{"name":"MCP_FIXTURE_VALUE","value":"configured"}]}]
            sessions=[]
            for _ in range(2):
                result=await h.rpc("session/new",{"cwd":str(h.workspace),"mcpServers":servers})
                self.assertNotIn("error",result,result)
                sessions.append(result["result"]["sessionId"])
            self.assertFalse(log.exists(),"MCP processes must start lazily")
            for s in sessions:
                result=await h.prompt(s)
                self.assertEqual(result["result"]["stopReason"],"end_turn",result)
            entries=[json.loads(x) for x in log.read_text().splitlines()]
            self.assertEqual(sum(x["method"]=="initialize" for x in entries),1)
            self.assertEqual(sum(x["method"]=="tools/call" for x in entries),2)
            self.assertEqual(len({x["pid"] for x in entries}),1)
            self.assertTrue(all(not x["api_key_inherited"] and x["explicit_env"]=="configured" for x in entries))
            self.assertEqual(len(h.permissions),2)

    async def test_mcp_denial_never_invokes_remote_tool_and_plan_does_not_connect(self):
        async def model(body):
            if any(i.get("type")=="function_call_output" for i in body["input"]): return final()
            if any(t["name"].startswith("mcp_") for t in body["tools"]):
                return response([call(mcp_tool(body,"mcp_fixture_echo_"),{"text":"must not execute"})])
            return final()
        async with Harness(model,allow=False) as h:
            await h.initialize()
            log=h.root/"mcp.log"
            servers=[{"name":"fixture","command":sys.executable,"args":[str(Path(__file__).with_name("mcp_server.py")),str(log)],"env":[]}]
            new=await h.rpc("session/new",{"cwd":str(h.workspace),"mcpServers":servers})
            s=new["result"]["sessionId"]
            await h.rpc("session/set_mode",{"sessionId":s,"modeId":"plan"})
            await h.prompt(s)
            self.assertFalse(log.exists())
            await h.rpc("session/set_mode",{"sessionId":s,"modeId":"code"})
            await h.prompt(s)
            self.assertNotIn('"method": "tools/call"',log.read_text())

    async def test_sse_accepts_missing_mime_but_rejects_explicit_html(self):
        async def model(_): return final("LUNA_OK")
        for mime in (None,"Text/Event-Stream; charset=utf-8"):
            with self.subTest(content_type=mime):
                async with Harness(model,content_type=mime) as h:
                    await h.initialize()
                    result=await h.prompt(await h.session())
                    self.assertEqual(result["result"]["stopReason"],"end_turn",result)
                    self.assertEqual(h.request_headers[0]["accept"],"text/event-stream")
        async def untrusted(_): return response([call("write_file",{"path":"bad.txt","content":"bad"})])
        async with Harness(untrusted,content_type="text/html") as h:
            await h.initialize()
            self.assertIn("error",await h.prompt(await h.session()))
            self.assertFalse(h.permissions)
            self.assertFalse((h.workspace/"bad.txt").exists())

    async def test_codeink_model_selection_before_each_prompt(self):
        async def model(_): return final("connected")
        async with Harness(model) as h:
            await h.initialize()
            created = await h.rpc("session/new", {"cwd": str(h.workspace), "mcpServers": []})
            session = created["result"]["sessionId"]
            selected = created["result"]["models"]["availableModels"][0]["modelId"]
            # CodeInk selects the advertised model before every prompt, even
            # when it is already current. This must be an idempotent operation.
            for _ in range(2):
                selection = await h.rpc("session/set_model", {"sessionId": session, "modelId": selected})
                self.assertEqual(selection.get("result"), {}, selection)
                result = await h.prompt(session, "test connection")
                self.assertEqual(result["result"]["stopReason"], "end_turn", result)
            invalid = await h.rpc("session/set_model", {"sessionId": session, "modelId": "unadvertised"})
            self.assertIn("not available", invalid["error"]["message"])
            missing = await h.rpc("session/set_model", {"sessionId": session})
            self.assertIn("error", missing)
            await h.rpc("_harness/session/unload", {"sessionId": session})
            loaded = await h.rpc("session/load", {"cwd": str(h.workspace), "sessionId": session, "mcpServers": []})
            self.assertEqual(loaded["result"]["models"]["currentModelId"], selected)
            self.assertEqual((await h.rpc("session/set_model", {"sessionId": session, "modelId": selected}))["result"], {})
            self.assertEqual((await h.prompt(session))["result"]["stopReason"], "end_turn")
            self.assertEqual(len(h.requests), 3)
            self.assertTrue(all(request["model"] == selected for request in h.requests))

    async def test_silent_idle_and_malformed_messages_do_not_break_acp(self):
        async def model(_): return final()
        async with Harness(model) as h:
            await asyncio.sleep(0.05)
            self.assertFalse(h.updates or h.unmatched)
            h.process.stdin.write(b'not-json\n{"jsonrpc":"2.0","id":{},"method":"initialize"}\n')
            await h.initialize()
            self.assertEqual([v["error"]["code"] for v in h.unmatched], [-32700, -32600])

    @unittest.skipIf(os.name == "nt", "POSIX signal behavior")
    async def test_sigint_exits_even_when_stdin_remains_open(self):
        async def model(_): return final()
        async with Harness(model) as h:
            await h.initialize()
            h.process.send_signal(signal.SIGINT)
            await asyncio.wait_for(h.process.wait(), 3)

    @unittest.skipIf(os.name == "nt", "POSIX signal behavior")
    async def test_sigterm_exits_even_when_stdin_remains_open(self):
        async def model(_): return final()
        async with Harness(model) as h:
            await h.initialize()
            h.process.send_signal(signal.SIGTERM)
            await asyncio.wait_for(h.process.wait(), 3)

    async def test_resource_links_cannot_read_harness_state(self):
        async def model(_): return final()
        async with Harness(model) as h:
            await h.initialize()
            # Workspace includes the state directory; protections must still apply.
            new = await h.rpc("session/new", {"cwd": str(h.root), "mcpServers": []})
            session = new["result"]["sessionId"]
            state = h.root / "state" / "sessions" / f"{session}.json"
            result = await h.rpc("session/prompt", {"sessionId": session, "prompt": [{"type": "resource_link", "uri": state.as_uri(), "name": "state"}]})
            self.assertIn("error", result)
            self.assertFalse(h.requests)

    async def test_approved_tool_loop_persistence_and_usage(self):
        async def model(body):
            self.assertEqual(body["tool_choice"],"auto")
            if any(item.get("type") == "function_call_output" for item in body["input"]): return final()
            return response([call("write_file", {"path": "hello.rs", "content": "fn main() {}\n"})])
        async with Harness(model) as h:
            await h.initialize()
            session = await h.session()
            result = await h.prompt(session)
            self.assertEqual(result["result"]["stopReason"], "end_turn", result)
            self.assertEqual((h.workspace / "hello.rs").read_text(), "fn main() {}\n")
            self.assertEqual(len(h.permissions), 1)
            self.assertNotIn("oldText",h.permissions[0]["params"]["toolCall"]["rawInput"])
            self.assertNotIn("newText",h.permissions[0]["params"]["toolCall"]["rawInput"])
            self.assertEqual(len(h.requests), 2)
            self.assertEqual(h.connections, 1, "HTTP client should reuse its connection")
            self.assertNotIn("prompt_cache_key", h.requests[0], "Do not force OpenAI-only fields on other providers")
            stats = await h.rpc("_harness/stats", {})
            self.assertEqual(stats["result"]["inputTokenCacheHitRate"], 0.9)  # Synthetic fixture, not a live cache claim.
            history = json.loads((h.root / "state" / "sessions" / f"{session}.json").read_text())
            self.assertTrue(any(item.get("type") == "function_call_output" for item in history["turns"][0]["items"]))
            await h.rpc("_harness/session/unload", {"sessionId": session})
            loaded = await h.rpc("session/load", {"sessionId": session, "cwd": str(h.workspace), "mcpServers": []})
            self.assertNotIn("error", loaded)
            replayed = [u["params"]["update"]["sessionUpdate"] for u in h.updates if u.get("method")=="session/update"]
            self.assertEqual(replayed.count("tool_call"),2)

    async def test_idle_cancel_preserves_independent_background_work(self):
        async def model(body):
            if any(i.get("type")=="function_call_output" for i in body["input"]): return final()
            return response([call("shell",{"command":("ping -n 21 127.0.0.1 >nul" if os.name == "nt" else "sleep 20"),"background":True,"timeout_secs":30})])
        async with Harness(model) as h:
            await h.initialize()
            session=await h.session()
            self.assertNotIn("error",await h.prompt(session))
            await h.rpc("session/cancel",{"sessionId":session})
            jobs=(await h.rpc("_harness/tasks",{"sessionId":session}))["result"]["tasks"]
            self.assertEqual(jobs[0]["status"],"running")
            await h.rpc("_harness/task/cancel",{"sessionId":session,"taskId":jobs[0]["task_id"]})

    async def test_oversized_provider_output_is_rejected_before_approval(self):
        async def model(_): return response([call("write_file",{"path":"big","content":"x"*30000})])
        async with Harness(model,extra="context_bytes = 16384\n") as h:
            await h.initialize()
            self.assertIn("error",await h.prompt(await h.session()))
            self.assertFalse(h.permissions)

    async def test_existing_files_can_be_read_then_edited(self):
        async def model(body):
            outputs=[i for i in body["input"] if i.get("type")=="function_call_output"]
            if not outputs: return response([call("read_file",{"path":"existing.txt","offset_line":1,"limit":100})])
            if len(outputs)==1:
                self.assertIn("original content",outputs[0]["output"])
                self.assertNotIn('"error"',outputs[0]["output"])
                return response([call("edit_file",{"path":"existing.txt","old_text":"original","new_text":"edited"},"edit")])
            self.assertNotIn('"error"',outputs[-1]["output"])
            return final()
        async with Harness(model) as h:
            (h.workspace/"existing.txt").write_text("original content")
            await h.initialize()
            self.assertNotIn("error",await h.prompt(await h.session()))
            self.assertEqual((h.workspace/"existing.txt").read_text(),"edited content")

    async def test_large_completed_history_does_not_discard_the_next_prompt(self):
        async def model(body):
            if body["input"][-1].get("content","").strip()=="second": return final()
            text="y"*7000
            return final(text)
        async with Harness(model,extra="context_bytes = 16384\n") as h:
            await h.initialize()
            session=await h.session()
            self.assertNotIn("error",await h.prompt(session,"first"))
            # A large new prompt plus retained history must be budgeted together.
            self.assertNotIn("error",await h.prompt(session,"x"*8000))
            self.assertNotIn("error",await h.prompt(session,"second"))

    async def test_denied_and_conflicting_edits_never_write(self):
        async def model(body):
            if any(item.get("type") == "function_call_output" for item in body["input"]): return final()
            return response([call("write_file", {"path": "file", "content": "model edit"})])
        for conflict in (False, True):
            async with Harness(model, allow=conflict) as h:
                await h.initialize()
                if conflict:
                    async def modify(_): (h.workspace / "file").write_text("human edit")
                    h.permission_hook = modify
                result = await h.prompt(await h.session())
                self.assertNotIn("error", result)
                if conflict: self.assertEqual((h.workspace / "file").read_text(), "human edit")
                else: self.assertFalse((h.workspace / "file").exists())
                self.assertIn("error", json.loads(h.requests[-1]["input"][-1]["output"]))

    async def test_plan_mode_and_incomplete_responses_cannot_mutate(self):
        for plan in (False, True):
            async def model(body):
                if any(item.get("type") == "function_call_output" for item in body["input"]): return final()
                return response([call("write_file", {"path": "forbidden", "content": "no"})], incomplete=not plan)
            async with Harness(model) as h:
                await h.initialize()
                session = await h.session()
                if plan: await h.rpc("session/set_mode", {"sessionId": session, "modeId": "plan"})
                result = await h.prompt(session)
                self.assertNotIn("error", result)
                self.assertFalse((h.workspace / "forbidden").exists())
                self.assertFalse(h.permissions)

    async def test_truncated_stream_never_executes_a_tool(self):
        async def model(_): return 'data: {"type":"response.output_item.added","item":{"type":"function_call"}}\n\n'
        async with Harness(model) as h:
            await h.initialize()
            result = await h.prompt(await h.session())
            self.assertIn("error", result)
            self.assertFalse(h.permissions)

    async def test_cancel_interrupts_model_and_a_queued_request(self):
        async def model(_):
            await asyncio.sleep(2)
            return final()
        async with Harness(model, extra="max_model_requests = 1\n") as h:
            await h.initialize()
            first, second = await h.session(), await h.session()
            running = asyncio.create_task(h.prompt(first))
            await h.seen.wait()
            queued = asyncio.create_task(h.prompt(second))
            await asyncio.sleep(0.03)
            # Cancel the queued request before releasing the active model slot.
            # Cancelling the running one first legitimately admits the queued HTTP
            # request between notifications, especially with Windows pipe scheduling.
            for session in (second, first):
                await h.send({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": session}})
            results = await asyncio.wait_for(asyncio.gather(running, queued), 1)
            self.assertTrue(all(r.get("result", {}).get("stopReason") == "cancelled" for r in results), results)
            self.assertEqual(len(h.requests), 1)

    async def test_foreground_subagents_are_parallel_and_context_isolated(self):
        started = set()
        both = asyncio.Event()
        async def model(body):
            user = next(item["content"] for item in body["input"] if item.get("role") == "user")
            if user in ("child-a", "child-b"):
                self.assertNotIn("PRIVATE PARENT", json.dumps(body["input"]))
                started.add(user)
                if len(started) == 2: both.set()
                await asyncio.wait_for(both.wait(), 3)
                self.assertNotIn("write_file", [t["name"] for t in body["tools"]])
                return final(user)
            if any(item.get("type") == "function_call_output" for item in body["input"]): return final("collected")
            return response([call("spawn_agent", {"prompt": child, "role": "explore", "background": False}, child) for child in ("child-a", "child-b")])
        async with Harness(model) as h:
            await h.initialize()
            result = await h.prompt(await h.session(), "PRIVATE PARENT")
            self.assertEqual(result["result"]["stopReason"], "end_turn", result)
            self.assertEqual(started, {"child-a", "child-b"})
            self.assertGreaterEqual(h.peak_active, 2)

    async def test_chat_completions_stream_and_usage(self):
        async def model(body):
            self.assertIn("messages", body)
            self.assertEqual(body["model"], "gpt-6-luna")
            self.assertEqual(body["reasoning_effort"], "none")
            self.assertNotIn("reasoning", body)
            chunks = [{"choices": [{"delta": {"content": "hello"}}]},
                      {"choices": [{"delta": {}, "finish_reason": "stop"}]},
                      {"choices": [], "usage": {"prompt_tokens": 100, "prompt_tokens_details": {"cached_tokens": 50}, "completion_tokens": 2}}]
            return "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
        async with Harness(model, extra='wire_api = "chat_completions"\nreasoning_effort = "none"\n') as h:
            await h.initialize()
            result = await h.prompt(await h.session())
            self.assertEqual(result["result"]["stopReason"], "end_turn")
            self.assertEqual((await h.rpc("_harness/stats", {}))["result"]["inputTokenCacheHitRate"], 0.5)

    async def test_256_sessions_with_a_bounded_model_request_budget(self):
        async def model(_):
            await asyncio.sleep(0.01)
            return final()
        async with Harness(model, extra="max_model_requests = 16\n") as h:
            await h.initialize()
            sessions = await asyncio.gather(*(h.session() for _ in range(256)))
            results = await asyncio.gather(*(h.prompt(session) for session in sessions))
            self.assertTrue(all(r.get("result", {}).get("stopReason") == "end_turn" for r in results))
            self.assertEqual(len(h.requests), 256)
            self.assertLessEqual(h.peak_active, 16)
            self.assertGreater(h.peak_active, 1)


if __name__ == "__main__":
    unittest.main(verbosity=2)
