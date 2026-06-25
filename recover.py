import json
import os

with open("/home/daanh/.gemini/antigravity/brain/a8340b03-8c6f-4a18-b8be-0fd1a53de206/.system_generated/logs/transcript_full.jsonl", "r") as f:
    lines = f.readlines()

os.system("git checkout HEAD@{0} src/main.rs")
with open("src/main.rs", "r") as f:
    content = f.read()

for line in lines:
    try:
        step = json.loads(line)
    except:
        continue

    if step.get("type") != "PLANNER_RESPONSE":
        continue

    for tool_call in step.get("tool_calls", []):
        args = tool_call.get("args", tool_call.get("arguments", {}))
        
        name = tool_call.get("name")
        if isinstance(name, str):
            # sometimes the function is nested
            pass
        elif isinstance(tool_call.get("function"), dict):
            name = tool_call["function"].get("name")
            args_str = tool_call["function"].get("arguments", "{}")
            if isinstance(args_str, str):
                args = json.loads(args_str)
            else:
                args = args_str

        # Remove default_api: prefix if present
        if name and name.startswith("default_api:"):
            name = name.replace("default_api:", "")

        if name == "replace_file_content":
            if args.get("TargetFile", "").endswith("main.rs"):
                target = args.get("TargetContent", "")
                replacement = args.get("ReplacementContent", "")
                if target in content:
                    content = content.replace(target, replacement, 1)

        elif name == "multi_replace_file_content":
            if args.get("TargetFile", "").endswith("main.rs"):
                chunks = args.get("ReplacementChunks", [])
                if isinstance(chunks, str):
                    try:
                        chunks = json.loads(chunks)
                    except:
                        continue
                for chunk in chunks:
                    target = chunk.get("TargetContent", "")
                    replacement = chunk.get("ReplacementContent", "")
                    if target in content:
                        content = content.replace(target, replacement, 1)

with open("src/main.rs.recovered", "w") as f:
    f.write(content)

