import re
with open("main_original.rs", "r") as f:
    orig = f.read()
    
with open("src/main.rs", "r") as f:
    curr = f.read()

# Extract collect_gpu from orig
m = re.search(r'(fn collect_gpu\(.*?^})', orig, re.MULTILINE | re.DOTALL)
if m:
    orig_fn = m.group(1)
    
    # In orig_fn, add the gpu_history and vram_history fields to the GpuMetrics initialization
    # It looks like:
    # GpuMetrics {
    #     available: true,
    #     name: fields[0].to_owned(),
    # ...
    # }
    new_fn = orig_fn.replace(
        "available: true,",
        "available: true,\n        gpu_history: std::collections::VecDeque::new(),\n        vram_history: std::collections::VecDeque::new(),"
    )
    
    # Now find where collect_gpu is in curr and replace it
    m2 = re.search(r'(fn collect_gpu\(.*?^})', curr, re.MULTILINE | re.DOTALL)
    if m2:
        curr = curr.replace(m2.group(1), new_fn)
    else:
        # It got mangled. Search for fn collect_gpu(index: u32) -> GpuMetrics {
        m3 = re.search(r'(fn collect_gpu\(.*?)fn ', curr, re.MULTILINE | re.DOTALL)
        if m3:
            curr = curr.replace(m3.group(1), new_fn + "\n\n")
            
    with open("src/main.rs", "w") as f:
        f.write(curr)
    print("Fixed!")
else:
    print("Not found in orig")
