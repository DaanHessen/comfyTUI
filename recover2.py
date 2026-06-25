import json
import os

def apply_change(content, target, replacement, start_line, end_line):
    lines = content.split('\n')
    prefix = '\n'.join(lines[:start_line-1])
    suffix = '\n'.join(lines[end_line:])
    search_area = '\n'.join(lines[start_line-1:end_line])
    
    if target in search_area:
        new_area = search_area.replace(target, replacement, 1)
        res = ""
        if prefix: res += prefix + '\n'
        res += new_area
        if suffix: res += '\n' + suffix
        return res
    else:
        return content.replace(target, replacement, 1)

with open("/home/daanh/.gemini/antigravity/brain/a8340b03-8c6f-4a18-b8be-0fd1a53de206/.system_generated/logs/transcript_full.jsonl", "r") as f:
    lines = f.readlines()

os.system("git checkout HEAD src/main.rs")
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
            pass
        elif isinstance(tool_call.get("function"), dict):
            name = tool_call["function"].get("name")
            args_str = tool_call["function"].get("arguments", "{}")
            if isinstance(args_str, str):
                try: args = json.loads(args_str)
                except: pass
            else:
                args = args_str

        if name and name.startswith("default_api:"):
            name = name.replace("default_api:", "")

        if name == "replace_file_content":
            if args.get("TargetFile", "").endswith("main.rs"):
                target = args.get("TargetContent", "")
                replacement = args.get("ReplacementContent", "")
                sl = args.get("StartLine", 1)
                el = args.get("EndLine", len(content.split('\n')))
                content = apply_change(content, target, replacement, sl, el)

        elif name == "multi_replace_file_content":
            if args.get("TargetFile", "").endswith("main.rs"):
                chunks = args.get("ReplacementChunks", [])
                if isinstance(chunks, str):
                    try: chunks = json.loads(chunks)
                    except: continue
                
                # Sort chunks by StartLine descending so line numbers don't shift for earlier chunks!
                # Wait! The tool applies chunks independently or simultaneously?
                # The actual tool sorts them by descending StartLine!
                chunks = sorted(chunks, key=lambda x: x.get("StartLine", 0), reverse=True)
                
                for chunk in chunks:
                    target = chunk.get("TargetContent", "")
                    replacement = chunk.get("ReplacementContent", "")
                    sl = chunk.get("StartLine", 1)
                    el = chunk.get("EndLine", len(content.split('\n')))
                    content = apply_change(content, target, replacement, sl, el)

with open("src/main.rs.recovered", "w") as f:
    f.write(content)

