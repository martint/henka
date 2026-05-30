#!/usr/bin/env python3
"""End-to-end smoke test: drive the MCP server over stdio through a real
register -> list_operations -> rename (preview, then apply) flow."""
import json, os, subprocess, sys, tempfile, pathlib

REPO = pathlib.Path(__file__).resolve().parent.parent
BIN = REPO / "target/debug/refactor-server"

def main():
    work = tempfile.mkdtemp()
    proj = pathlib.Path(work) / "demo"
    proj.mkdir()
    (proj / "Greeting.java").write_text(
        "public class Greeting {\n    public String greet() {\n        return \"hi\";\n    }\n}\n")
    (proj / "Main.java").write_text(
        "public class Main {\n    public static void main(String[] args) {\n"
        "        System.out.println(new Greeting().greet());\n    }\n}\n")
    cfg = pathlib.Path(work) / "projects.toml"

    env = dict(os.environ, REFACTOR_MCP_CONFIG=str(cfg), REFACTOR_LOG="warn",
               JDTLS_HOME=str(REPO / ".cache/jdtls"))
    p = subprocess.Popen([str(BIN)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                         stderr=subprocess.DEVNULL, env=env, text=True, bufsize=1)

    def call(obj):
        p.stdin.write(json.dumps(obj) + "\n"); p.stdin.flush()
        if "id" not in obj:
            return None
        while True:
            line = p.stdout.readline()
            if not line:
                raise SystemExit("server closed")
            msg = json.loads(line)
            if msg.get("id") == obj["id"]:
                return msg

    def tool(i, name, args):
        r = call({"jsonrpc":"2.0","id":i,"method":"tools/call",
                  "params":{"name":name,"arguments":args}})
        if "error" in r:
            return {"_error": r["error"]}
        return json.loads(r["result"]["content"][0]["text"])

    call({"jsonrpc":"2.0","id":1,"method":"initialize",
          "params":{"protocolVersion":"2024-11-05","capabilities":{},
                    "clientInfo":{"name":"smoke","version":"0"}}})
    call({"jsonrpc":"2.0","method":"notifications/initialized"})

    tools = call({"jsonrpc":"2.0","id":2,"method":"tools/list"})
    names = [t["name"] for t in tools["result"]["tools"]]
    print("TOOLS:", sorted(names))
    assert "rename" in names and "find-usages" in names, "Java operations missing"

    print("REGISTER:", tool(3, "register_project", {"root": str(proj), "id": "demo"}))
    ops = tool(4, "list_operations", {"project": "demo"})
    print("OPERATIONS:", [o["id"] for o in ops])

    print("\n--- find-usages of greet ---")
    usages = tool(5, "find-usages", {"project":"demo","file":"Greeting.java",
                                     "line":1,"character":18})
    print("USAGES:", json.dumps(usages, indent=2))
    assert usages.get("count", 0) >= 1, "expected to find usages of greet"

    print("--- rename greet -> greeting (preview) ---")
    preview = tool(6, "rename", {"project":"demo","file":"Greeting.java",
                                 "line":1,"character":18,"new_name":"greeting"})
    if "_error" in preview:
        print("ERROR:", preview); p.terminate(); sys.exit(1)
    for f in preview["files"]:
        print(f["diff"])

    # Files must be untouched after preview.
    assert "greet()" in (proj/"Main.java").read_text()

    print("--- apply ---")
    applied = tool(7, "rename", {"project":"demo","file":"Greeting.java",
                                 "line":1,"character":18,"new_name":"greeting","dry_run":False})
    print("APPLIED:", applied)
    main_after = (proj/"Main.java").read_text()
    greeting_after = (proj/"Greeting.java").read_text()
    assert ".greeting()" in main_after, main_after
    assert "String greeting()" in greeting_after, greeting_after

    # After applying, the session must see the new state: find-usages of the
    # renamed symbol should still resolve (this returned 0 before edit-sync).
    print("--- find-usages of greeting (after apply) ---")
    after = tool(8, "find-usages", {"project":"demo","file":"Greeting.java",
                                    "line":1,"character":18})
    print("USAGES:", json.dumps(after, indent=2))
    assert after.get("count", 0) >= 1, "session should reflect applied rename"

    p.terminate()
    print("\nOK: end-to-end rename + find-usages through MCP succeeded")

if __name__ == "__main__":
    main()
