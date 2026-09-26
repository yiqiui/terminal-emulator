# -*- coding: utf-8 -*-
"""无头浏览器端到端测试：多标签/分屏 UI 的按钮与切换链路。"""
import base64
import sys
import time
from playwright.sync_api import sync_playwright

URL = "file:///D:/workspace/code/wd/terminal-emulator/ui/test.html"
errors = []
console_errors = []


def main():
    with sync_playwright() as p:
        browser = p.chromium.launch(headless=True)
        page = browser.new_page(viewport={"width": 1200, "height": 800})
        page.on("console", lambda msg: console_errors.append(msg.text) if msg.type == "error" else None)
        page.on("pageerror", lambda err: errors.append(str(err)))
        page.goto(URL)
        page.wait_for_timeout(1500)

        def check(name, cond, detail=""):
            print(("PASS " if cond else "FAIL ") + name + (f" | {detail}" if detail and not cond else ""))

        # 1. 初始：一个 tab，一个 pane，xterm 渲染，shell=powershell
        tabs = page.locator(".tab").count()
        panes = page.locator(".pane").count()
        check("初始单标签单面板", tabs == 1 and panes == 1, f"tabs={tabs} panes={panes}")
        cols = page.evaluate("document.querySelector('.pane .term-host .xterm') ? document.querySelector('.pane .term-host .xterm').getBoundingClientRect().width : 0")
        check("终端宽度正常(>400px)", cols > 400, f"width={cols}")
        starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        check("首个会话自动启动 powershell", len(starts) == 1 and starts[0]["params"]["shell"] == "powershell", str(starts))

        # 2. 切换 shell -> CMD：restart 链路（stop + start cmd）
        page.select_option("#shell", "cmd")
        page.wait_for_timeout(600)
        starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        check("切换 shell 触发新会话(cmd)", len(starts) == 2 and starts[1]["params"]["shell"] == "cmd", str(starts))
        banner = page.evaluate("document.querySelectorAll('.pane .xterm-rows')[0].textContent")
        check("新会话 banner 已渲染(cmd)", "[CMD READY]" in banner, banner[:80])

        # 3. 重启按钮
        page.click("#restart")
        page.wait_for_timeout(600)
        starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        check("重启触发第三次 start", len(starts) == 3, str(len(starts)))

        # 4. 分屏（右）：出现第二个 pane，第三个会话
        page.click("#split-h")
        page.wait_for_timeout(800)
        panes = page.locator(".pane").count()
        starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        check("分屏出现第二个面板", panes == 2, f"panes={panes}")
        check("分屏启动新会话", len(starts) == 4, str(len(starts)))

        # 5. 再分屏（下）：第三个 pane
        page.click("#split-v")
        page.wait_for_timeout(800)
        panes = page.locator(".pane").count()
        check("嵌套分屏出现第三个面板", panes == 3, f"panes={panes}")

        # 6. + 新标签：tab 数 2，pane 数回 1
        page.click("#new-tab")
        page.wait_for_timeout(600)
        tabs = page.locator(".tab").count()
        panes = page.locator(".pane").count()
        check("新标签页", tabs == 2 and panes == 1, f"tabs={tabs} panes={panes}")

        # 7. 回到第一个标签（应仍是 3 面板布局）
        page.locator(".tab").first.click()
        page.wait_for_timeout(600)
        panes = page.locator(".pane").count()
        check("切回旧标签布局保留", panes == 3, f"panes={panes}")

        # 8. 关闭活动面板
        page.click("#close-pane")
        page.wait_for_timeout(600)
        panes = page.locator(".pane").count()
        check("关闭面板", panes == 2, f"panes={panes}")

        # 9. 清屏按钮
        page.click("#clear")
        page.wait_for_timeout(300)

        # 10. SSH 快连
        page.click("#ssh-toggle")
        page.fill("#ssh-host", "example.com")
        page.click("#ssh-connect")
        page.wait_for_timeout(600)
        starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        last = starts[-1]
        check("SSH 会话参数正确", last["params"]["shell"] == "ssh" and last["params"]["host"] == "example.com", str(last["params"]))
        shell_label = page.evaluate("document.getElementById('status-shell').textContent")
        check("状态栏显示 ssh 目标", "ssh" in shell_label, shell_label)

        # 11. 命令面板 Ctrl+Shift+P
        page.keyboard.press("Control+Shift+KeyP")
        page.wait_for_timeout(400)
        shown = page.evaluate("document.getElementById('palette-modal').classList.contains('show')")
        items = page.locator(".palette-item").count()
        check("命令面板打开且有动作列表", shown and items >= 10, f"shown={shown} items={items}")
        page.keyboard.press("Escape")
        page.wait_for_timeout(200)

        # 12. 设置中心：改字号保存 → storage.set 被调用
        page.click("#settings-toggle")
        page.wait_for_timeout(300)
        page.fill("#set-fontsize", "16")
        page.click("#settings-save")
        page.wait_for_timeout(500)
        saved = page.evaluate("(window.__dbxStorage && window.__dbxStorage.terminalSettings) || null")
        check("设置持久化到 storage", saved and saved.get("fontSize") == 16, str(saved))

        # 13. Telnet 向导（经命令面板打开）
        page.keyboard.press("Control+Shift+KeyP")
        page.wait_for_timeout(300)
        page.locator(".palette-item", has_text="Telnet").click()
        page.wait_for_timeout(300)
        page.select_option("#conn-type", "telnet")
        page.fill("#conn-host", "bbs.example.com")
        page.click("#conn-go")
        page.wait_for_timeout(600)
        starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        last = starts[-1]
        all_starts = page.evaluate("window.__test.calls.filter(c=>c.method==='terminal/start')")
        print('DEBUG 最近4次 start:', [s['params'] for s in all_starts[-4:]])
        check("Telnet 会话参数", last["params"].get("shell") == "telnet" and last["params"].get("host") == "bbs.example.com", str(last["params"]))

        # 14. SFTP 面板冒烟（mock 返回空目录）
        page.click("#sftp-toggle")
        page.fill("#sftp-host", "srv.example.com")
        page.fill("#sftp-user", "root")
        page.click("#sftp-open")
        page.wait_for_timeout(800)
        sftp_status = page.evaluate("document.getElementById('sftp-status').textContent")
        check("SFTP 列表请求无致命错误", page.evaluate("document.getElementById('fatal').style.display") != "block", sftp_status)

        # 汇总
        fatal = page.evaluate("document.getElementById('fatal').textContent")
        check("无致命错误横幅", page.evaluate("document.getElementById('fatal').style.display") != "block", fatal)
        check("无页面异常", not errors, "; ".join(errors[:3]))
        check("无控制台错误", not console_errors, "; ".join(console_errors[:3]))
        page.screenshot(path="test_terminal.png", full_page=False)
        browser.close()


main()
print("DONE")
