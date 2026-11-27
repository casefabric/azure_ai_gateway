"""Live prompt-caching check through the proxy (costs well under a cent on claude-haiku-5-5).

Usage: python3 tools/live_cache_test.py http://127.0.0.1:8135/api/projects/x/chat/completions claude-haiku-5-5

Run the proxy with --backend anthropic --anthropic-cache-diagnostics. Expected:
R1 writes ~3.2k tokens to the cache; R2 (streamed, tools in another order) reads them with
diagnostics null; R3 (changed system prompt) misses with system_changed; R4 reads again.
Check logs/requests.jsonl and the "Cache diagnostics" line in the main log.
"""
import json, sys, urllib.request
URL = sys.argv[1]
MODEL = sys.argv[2]
rules = " ".join(f"Rule {i}: when asked about colors, answer with exactly one common color name and nothing else, no punctuation." for i in range(1, 80))
SYSTEM = "You are a terse assistant for a color-naming test. " + rules
def tool(name):
    return {"type": "function", "function": {"name": name, "description": f"Tool {name} looks up a color palette by name.",
            "parameters": {"type": "object", "properties": {"palette": {"type": "string"}}, "required": ["palette"]}}}
def call(messages, tools, stream=False):
    body = {"model": MODEL, "max_tokens": 1000, "messages": messages, "tools": tools}
    if stream:
        body["stream"] = True; body["stream_options"] = {"include_usage": True}
    req = urllib.request.Request(URL, json.dumps(body).encode(), {"content-type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=120) as r:
            raw = r.read().decode()
    except urllib.error.HTTPError as e:
        print("HTTP", e.code, e.read().decode()[:400]); sys.exit(1)
    if not stream:
        d = json.loads(raw); return d["choices"][0]["message"]["content"] or "", d["usage"]
    text, usage = "", None
    for line in raw.splitlines():
        if line.startswith("data: ") and line != "data: [DONE]":
            c = json.loads(line[6:])
            if c.get("usage"): usage = c["usage"]
            for ch in c.get("choices", []):
                text += ch.get("delta", {}).get("content") or ""
    return text, usage
sysmsg = {"role": "system", "content": SYSTEM}
hist = [sysmsg, {"role": "user", "content": "Name one color."}]
a, u = call(hist, [tool("b"), tool("a")]); print("R1", repr(a), u)
hist += [{"role": "assistant", "content": a}, {"role": "user", "content": "Name another color."}]
a, u = call(hist, [tool("a"), tool("b")], stream=True); print("R2 stream", repr(a), u)
hist += [{"role": "assistant", "content": a}, {"role": "user", "content": "And a third color?"}]
hist[0] = {"role": "system", "content": SYSTEM + " Answer in English."}
a, u = call(hist, [tool("b"), tool("a")]); print("R3 system changed", repr(a), u)
hist += [{"role": "assistant", "content": a}, {"role": "user", "content": "One more color?"}]
a, u = call(hist, [tool("b"), tool("a")]); print("R4", repr(a), u)
