# Fake ACP agent for tests: requires login, lists two sessions, replays history on load, echoes prompts.
import json
import sys


def send(message):
    print(json.dumps(message), flush=True)


def update(session_id, payload):
    send({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": session_id, "update": payload}})


authenticated = False
for line in sys.stdin:
    request = json.loads(line)
    method, params, request_id = request["method"], request.get("params", {}), request["id"]
    if method == "initialize":
        result = {
            "protocolVersion": 1,
            "agentCapabilities": {"loadSession": True, "sessionCapabilities": {"list": {}}},
            "authMethods": [{"id": "stored-login", "name": "Stored login"}],
        }
    elif method == "authenticate":
        authenticated = params["methodId"] == "stored-login"
        result = {}
    elif method in ("session/list", "session/load", "session/new") and not authenticated:
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32000, "message": "Authentication required"}})
        continue
    elif method == "session/list":
        result = {"sessions": [
            {"sessionId": "old", "title": "older chat", "updatedAt": "2026-11-01T09:00:00Z"},
            {"sessionId": "new", "title": "newer chat", "updatedAt": "2026-11-05T09:00:00Z"},
        ]}
    elif method == "session/load":
        sid = params["sessionId"]
        update(sid, {"sessionUpdate": "user_message_chunk", "content": {"type": "text", "text": "earlier question"}})
        update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "earlier answer"}})
        update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "image", "data": "aGk=", "mimeType": "image/png"}})
        result = {}
    elif method == "session/new":
        result = {"sessionId": "fresh"}
    elif method == "session/prompt":
        text = params["prompt"][0]["text"]
        update(params["sessionId"], {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "echo: " + text}})
        result = {"stopReason": "end_turn"}
    else:
        send({"jsonrpc": "2.0", "id": request_id, "error": {"code": -32601, "message": "unsupported"}})
        continue
    send({"jsonrpc": "2.0", "id": request_id, "result": result})
