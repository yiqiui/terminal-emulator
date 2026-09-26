# -*- coding: utf-8 -*-
"""修复：startMonitor 未定义、系统卡片数据、工具栏溢出。"""
from pathlib import Path
import re

p = Path(__file__).parent / "ui" / "index.html"
t = p.read_text(encoding="utf-8")

# 1. startMonitor 定义（railMonitor 点击引用了它）
if "function startMonitor" not in t:
    old = "        setInterval(pollStats, 2000);"
    assert old in t
    t = t.replace(old, "        setInterval(pollStats, 2000);\n        function startMonitor() { pollStats(); }", 1)
    print("1 startMonitor defined")

# 2. 系统卡片数据：后端补 host/os/uptime
cb = Path(__file__).parent / "backend" / "src" / "main.rs"
mt = cb.read_text(encoding="utf-8")
if '"hostName"' not in mt:
    old = "    let networks = sysinfo::Networks::new_with_refreshed_list();"
    add = (        "    let host_name = sysinfo::System::host_name().unwrap_or_default();" + chr(10)        + "    let os_version = sysinfo::System::long_os_version().unwrap_or_default();" + chr(10)        + "    let uptime = sysinfo::System::uptime();" + chr(10)        + "    let per_core: Vec<i64> = sys.cpus().iter().map(|cpu| cpu.cpu_usage().round() as i64).collect();" + chr(10)    )    assert old in mt, "stats host vars"
    mt = mt.replace(old, add + old)
    old = '        "netTx": tx,'
    new = '        "netTx": tx,\n        "hostName": host_name,\n        "osVersion": os_version,\n        "uptime": uptime,\n        "perCore": per_core,''
    assert old in mt, "stats host json"
    mt = mt.replace(old, new)
    cb.write_text(mt, encoding="utf-8")
    print("2 backend host info")

# 3. 前端填充系统卡片
if "mon-sys\").textContent" not in t and 'mon-sys' in t:
    old = '''            const gb = (v) => (v / 1073741824).toFixed(1);'''
    new = '''            const gb = (v) => (v / 1073741824).toFixed(1);
            const days = Math.floor((s.uptime || 0) / 86400);
            const hours = Math.floor(((s.uptime || 0) % 86400) / 3600);
            $("mon-sys").textContent = [s.hostName, s.osVersion, `运行 ${days}天${hours}小时`].filter(Boolean).join(" · ");'''
    assert old in t, "mon-sys populate"
    t = t.replace(old, new)
    print("3 mon-sys populate")

# 4. 工具栏溢出：横向滚动 + 主列弹性收缩
old = '''      #toolbar {
        display: flex;
        align-items: center;
        gap: 6px;
        padding: 7px 12px;'''
new = '''      #toolbar {
        display: flex;
        align-items: center;
        gap: 6px;
        padding: 7px 12px;
        overflow-x: auto;
        min-width: 0;
        flex-shrink: 1;'''
assert old in t, "toolbar overflow"
t = t.replace(old, new)
old = "      #main-col {"
# main-col 无样式定义，加弹性约束
if "#main-col {" not in t:
    t = t.replace("      #workbench { display: flex; flex: 1; min-height: 0; }",
                  "      #workbench { display: flex; flex: 1; min-height: 0; }\n      #main-col { flex: 1 1 0%; min-width: 0; display: flex; flex-direction: column; }")
    print("4 toolbar/main-col overflow fixed")

# 5. 后端 sysinfo 的 CPU 采样间隔优化（Windows 下首次为 0 的问题已由 200ms 双刷新覆盖）
p.write_text(t, encoding="utf-8")
print("ALL DONE")
