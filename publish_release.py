# -*- coding: utf-8 -*-
"""创建 GitHub Release（v0.1.10）并上传候选包/icon/release-candidates.json；提交 .dbx-store.json。"""
import base64
import hashlib
import json
import subprocess
import time
from pathlib import Path

REPO = "yiqiui/terminal-emulator"
PROJECT = Path(__file__).parent
TAG = "v0.1.10"
DIST = PROJECT / "dist" / f"com.local.terminal-emulator-0.1.10-windows-x64.dbxp"


def gh(endpoint, payload=None, method="GET"):
    args = ["gh", "api"]
    if method != "GET":
        args += ["-X", method]
    args.append(endpoint)
    if payload is not None:
        args += ["--input", "-"]
    result = subprocess.run(args, input=json.dumps(payload) if payload is not None else None,
                            capture_output=True, text=True, encoding="utf-8")
    if result.returncode != 0:
        raise RuntimeError(f"gh api {method} {endpoint}: {result.stderr[:300]}")
    return json.loads(result.stdout) if result.stdout.strip() else {}


def main():
    # 1. .dbx-store.json（自动化/首次投稿元数据）提交到仓库
    meta = {
        "name": "Terminal Emulator",
        "description": "A DBX workbench terminal emulator with multi-tab sessions, split panes, SSH/Telnet/Serial connections, SFTP file management, and a live system monitor.",
        "icon": "assets/plugin.svg",
        "tags": ["terminal", "ssh", "telnet", "serial", "sftp"],
        "permissions": ["host.events", "host.binary", "host.workbench", "host.storage"],
        "source": f"https://github.com/{REPO}",
        "homepage": f"https://github.com/{REPO}",
        "license": "MIT",
        "releaseNotes": "First store submission: multi-tab sessions with split panes, PowerShell/CMD/Bash/SSH/Telnet/Serial, SFTP file explorer, system monitor, command palette, settings center, and DBX theme following.",
        "localizations": {
            "zh-CN": {
                "name": "终端模拟器",
                "description": "DBX 工作台内置终端：多标签分屏会话、SSH/Telnet/Serial 连接、SFTP 文件管理、系统监控、命令面板与主题跟随。"
            }
        },
    }
    try:
        existing = gh(f"repos/{REPO}/contents/.dbx-store.json")
        gh(f"repos/{REPO}/contents/.dbx-store.json", {
            "message": "update .dbx-store.json metadata",
            "content": base64.b64encode(json.dumps(meta, ensure_ascii=False, indent=2).encode()).decode(),
            "sha": existing["sha"],
        }, method="PUT")
        print(".dbx-store.json 已更新")
    except RuntimeError as e:
        if "404" in e.args[0]:
            gh(f"repos/{REPO}/contents/.dbx-store.json", {
                "message": "add .dbx-store.json metadata",
                "content": base64.b64encode(json.dumps(meta, ensure_ascii=False, indent=2).encode()).decode(),
            }, method="PUT")
            print(".dbx-store.json 已创建")
        else:
            raise

    # 2. Release（存在则复用）
    try:
        release = gh(f"repos/{REPO}/releases/tags/{TAG}")
        print("Release 已存在，复用")
    except RuntimeError:
        release = gh(f"repos/{REPO}/releases", {
            "tag_name": TAG,
            "target_commitish": "main",
            "name": "Terminal Emulator 0.1.10",
            "body": "First release candidate for DBX Store submission.\n\n- Multi-tab sessions and split panes\n- PowerShell/CMD/Bash/SSH/Telnet/Serial\n- SFTP file explorer, system monitor, command palette, settings center\n\nUnsigned review candidate (windows-x64).",
            "draft": False,
            "prerelease": False,
        }, method="POST")
        print(f"Release 已创建: {release['id']}")

    # 3. 上传资产
    upload_url = release["upload_url"].split("{")[0]
    assets = {a["name"]: a for a in release.get("assets", [])}
    for file in [DIST, PROJECT / "assets" / "plugin.svg"]:
        name = file.name if file.suffix == ".dbxp" else "icon.svg"
        if name in assets:
            print(f"资产已存在: {name}")
            continue
        args = ["gh", "api", "--method", "POST", upload_url + f"?name={name}", "-H", "Content-Type: application/octet-stream", "--input", str(file)]
        result = subprocess.run(args, capture_output=True, text=True, encoding="utf-8", timeout=300)
        if result.returncode != 0:
            raise RuntimeError(f"upload {name}: {result.stderr[:300]}")
        print(f"资产已上传: {name}")

    # 4. release-candidates.json（校验和）
    data = DIST.read_bytes()
    icon_url = f"https://github.com/{REPO}/releases/download/{TAG}/icon.svg"
    rc = {
        "schemaVersion": 1,
        "id": "com.local.terminal-emulator",
        "publisher": "local",
        "version": "0.1.10",
        "targets": [
            {
                "target": "windows-x64",
                "url": f"https://github.com/{REPO}/releases/download/{TAG}/{DIST.name}",
                "sha256": hashlib.sha256(data).hexdigest(),
                "size": len(data),
            }
        ],
    }
    rc_path = PROJECT / "release-candidates.json"
    rc_path.write_text(json.dumps(rc, ensure_ascii=False, indent=2), encoding="utf-8")
    print("release-candidates.json 已生成:", rc_path)
    print("icon:", icon_url)
    print("候选包 URL:", f"https://github.com/{REPO}/releases/download/{TAG}/{DIST.name}")
    print("sha256:", rc["targets"][0]["sha256"])
    print("size:", rc["targets"][0]["size"])


main()
