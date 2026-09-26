# Terminal Emulator (DBX Plugin)

DBX 工作台内置终端模拟器插件。参照 [nyakang/nyaterm](https://github.com/nyakang/nyaterm) 的本地终端实现（portable-pty + xterm.js + 输出合并流控 + 多会话管理），以 DBX 插件契约（Manifest v1 + Host API 1.x + Sidecar Protocol v1）落地。

![version](https://img.shields.io/badge/version-0.1.10-blue) ![platform](https://img.shields.io/badge/platform-windows%20%7C%20linux%20%7C%20macos-lightgrey)

## 功能

### 终端核心
- **多标签 + 分屏**：一个工作台内任意多 PTY 会话，标签页切换、水平/垂直分屏（pane 树）、活动面板高亮
- **多 Shell**：PowerShell / PowerShell Core / CMD / Bash（自动探测本机可用项，Git Bash/MSYS2 优先于 WSL 启动器）
- **输出合并流控**：8ms 时间窗 + 64KB 阈值批处理（借鉴 nyaterm `SessionOutputCoalescer`），洪泛不卡 UI
- **主题跟随**：颜色/圆角/字体实时跟随 DBX 宿主主题（明暗+自定义主题），终端配色可固定深/浅
- 会话日志导出（xterm serialize → 系统保存对话框）

### 远程连接
- **SSH**：系统 OpenSSH 客户端，密码/私钥/agent 交互认证；支持"SSH 服务器"保存连接（连接提供者），从侧边栏直连
- **Telnet**：原生 TCP 实现，自动剥离 IAC 协商字节
- **Serial**：串口连接（自动枚举 COM 口，波特率可配）
- **SFTP 文件管理**：左侧文件资源管理器面板，浏览/进入目录/新建/删除/下载/上传（≤6MB，基于系统 sftp.exe 批处理，密钥/agent 认证）

### 效率工具
- **系统监控**：右侧面板实时显示 CPU（每核占用条）/内存/磁盘/网络（sysinfo，2 秒轮询）
- **命令面板**：`Ctrl+Shift+P` 快捷动作
- **快捷命令栏**：底部常用命令芯片，点击发送到活动终端，可增删、持久化
- **终端设置中心**：字体/字号/滚回行数/光标样式+闪烁/配色，持久化到 DBX 存储
- 终端内搜索（`Ctrl+F`）、复制（`Ctrl+Shift+C`）粘贴（`Ctrl+V`）

## 安装

DBX → 插件中心 → 设置 → 本地安装 `.dbxp` 包；或从 [Releases](../../releases) 下载后本地安装。

## 构建

```bash
cd backend && cargo build --release   # Rust sidecar
npx @dbx-app/plugin-cli package .     # 打包 dist/*.dbxp
```

## 测试

```bash
# sidecar 协议端到端 harness（多会话隔离/resize/退出归属等 10 项断言）
cd backend && py ../harness.py

# UI 无头测试（Playwright + mock 宿主桥，17+ 项断言）
py ui_headless_test.py
```

## 权限

| 权限 | 用途 |
| --- | --- |
| `host.binary` | 终端输入输出二进制通道（`stdio-framed`） |
| `host.events` | 向 UI 转发会话退出事件 |
| `host.workbench` | 注册 Terminal 工作台贡献点 |
| `host.storage` | 终端设置与快捷命令持久化 |

## 目录结构

```
manifest.json          # Manifest v1：workbench + SSH 连接提供者贡献点
dbx-plugin.toml        # 打包配置
backend/               # Rust sidecar（dbx-plugin-sdk + portable-pty）
ui/index.html          # 沙箱工作台 UI（免构建，xterm.js 从包内 vendor 加载）
ui/assets/vendor/      # @xterm/xterm 5.5 + fit/search/serialize 插件
harness.py             # sidecar 协议端到端验证
ui_headless_test.py    # 无头浏览器 UI 测试
```

## License

MIT
