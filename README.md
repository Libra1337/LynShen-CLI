# LynShen CLI

LynShen CLI 是一个在终端里运行的编程智能体，命令是 `lynshen`，用 Rust 写成。你在项目目录里启动它，用自然语言让它读代码、改代码、跑命令和测试。

它也是 [LynShen Desktop](https://github.com/Libra1337/LynShen_Agent) 的引擎。桌面端的每个会话都运行在 `lynshen daemon` 里，这个后台服务也在本仓库。

[English](#english)

## 本仓库有什么

| 命令或目录 | 作用 |
| --- | --- |
| `lynshen` | 交互式终端界面（TUI），日常使用 |
| `lynshen --headless` | 无界面运行一个任务，输出 JSONL，用于脚本、CI 和评测 |
| `lynshen serve` | 通过标准输入输出收发 JSON 的协议，给图形界面和 IDE 用 |
| `lynshen acp` | [Agent Client Protocol](https://agentclientprotocol.com) 适配，给 Zed 等编辑器用 |
| `lynshen daemon` | 后台服务，同时托管多个会话，桌面端和手机都连接它 |
| `relay/` | 中继服务（Go），手机通过它连接电脑上的 daemon |
| `npm/` | npm 包 `@lynshen/cli` 的打包文件 |
| `evals/` | 一个小的冒烟测试集，跑 `--headless` |

## 安装

**用 LynShen Desktop。** 桌面端自带一份 CLI，放在 `~/.lynshen/bin`，随应用一起更新。要在终端里用 `lynshen` 命令，到桌面端的「设置 → 编码智能体」里把它加成终端命令。

**下载二进制。** 每个版本的可执行文件在 [GitHub Releases](https://github.com/Libra1337/LynShen-CLI/releases)，同时也发布在 LynShen 软件库 `https://software.lynshen.org/cli/<版本>/<文件名>`。

| 系统 | 文件名 |
| --- | --- |
| macOS（Apple 芯片） | `lynshen-aarch64-apple-darwin` |
| macOS（Intel） | `lynshen-x86_64-apple-darwin` |
| Linux x64 | `lynshen-x86_64-unknown-linux-gnu` |
| Windows x64 | `lynshen-x86_64-pc-windows-msvc.exe` |

例如在 Apple 芯片的 Mac 上：

```sh
curl -fLo lynshen https://github.com/Libra1337/LynShen-CLI/releases/latest/download/lynshen-aarch64-apple-darwin
chmod +x lynshen
mkdir -p ~/.local/bin && mv lynshen ~/.local/bin/
lynshen --version
```

`~/.local/bin` 要在你的 PATH 里。

发布流程也会打包 npm 包 `@lynshen/cli`，但目前 npm 上还没有这个包，请先用上面的二进制。

**从源码构建。** 需要 Rust 稳定版。`crates/llm-provider-kit` 是 git 子模块，克隆时要带上 `--recurse-submodules`：

```sh
git clone --recurse-submodules https://github.com/Libra1337/LynShen-CLI.git
cd LynShen-CLI
cargo build --release
./target/release/lynshen --version
```

`scripts/install-local.sh` 会构建 release 版本并装到 `~/.local/bin/lynshen`（用 `PREFIX` 改位置）。

**Linux 用户注意：** 智能体的 shell 命令在 bubblewrap 沙箱里运行。先安装 `bubblewrap`（例如 `sudo apt install bubblewrap`）。没有它，shell 命令会失败，直到你装上它或把沙箱切到 `full-access`。LynShen 不会自己退回到不加沙箱运行。

## 第一次使用

```sh
cd path/to/project
lynshen
```

第一次运行会创建 `~/.lynshen/config.json`。默认的模型服务是 LynShen 网关（`provider` 为 `lynshen`），默认模型是 `gpt-5.5`。LynShen 网关用 LynShen 账号的额度调用模型，账号在 [LynShen Console](https://www.lynshen.org)（LynShen 官网）注册和管理。

然后接入模型。在 TUI 里输入 `/login`，会列出可以登录的服务商，LynShen 排在第一个。

- **LynShen 账号。** 选 LynShen，或直接输入 `/login lynshen`。浏览器会打开 [LynShen 官网](https://www.lynshen.org)，登录并同意授权后回到终端。之后用账号的余额调用模型，`/usage` 查看套餐、余额和用量。
- **自己的 API Key。** 输入 `/login <服务商> <key>`，LynShen 保存 key 并切换到这个服务商，例如 `/login deepseek sk-...`。`/login list` 以文字列出全部服务商。
- **订阅账号。** 部分服务商支持浏览器登录，例如 `/login openai-codex` 用 ChatGPT 订阅登录。

`lynshen providers` 以 JSON 打印全部内置服务商和它们的模型。

登录以后直接用中文或英文描述任务，例如「修复 `cargo test` 里失败的那个测试，并说明原因」。

### 常用命令

| 命令 | 作用 |
| --- | --- |
| `/model [模型] [强度]` | 查看或切换模型和推理强度。`Ctrl+T` 切换当前模型的推理强度 |
| `/permissions [模式]` | 查看或切换审批模式。`Shift+Tab` 循环切换 |
| `/sandbox [模式]` | 查看或切换 shell 命令的沙箱 |
| `/resume [会话 ID]` | 列出或恢复以前的会话。也可以用 `lynshen --resume <id>` 启动 |
| `/rewind [回合 ID]` | 把对话和文件退回到之前的某一回合 |
| `/tree` | 查看对话分支树 |
| `/context`、`/compact` | 查看上下文用量；立即压缩上下文 |
| `/skills`、`/mcp` | 管理技能和 MCP 服务器 |
| `/goal <目标>` | 设置一个跨回合的目标 |
| `/image <路径>` | 给下一条消息附一张图片 |
| `/doctor` | 检查运行环境 |
| `/new`、`/quit` | 新会话；退出 |

输入框里的其他用法：

- `!` 开头的输入直接在本机 shell 里执行，例如 `!git log -3`，不发给模型。
- 输入 `@` 加几个字符，可以模糊选择项目里的文件。
- `~/.lynshen/commands/*.md` 里的每个 Markdown 文件都会变成一个 `/文件名` 命令。文件里的 `$ARGUMENTS` 会换成命令后面的文字。

## 审批和沙箱

审批模式决定智能体改文件、跑命令前是否先问你。在 `config.json` 的 `approval_mode` 里设置，或在会话里用 `/permissions` 切换。

| 模式 | 改文件 | 跑 shell 命令 |
| --- | --- | --- |
| `manual`（默认） | 询问 | 询问 |
| `auto-edit` | 直接改 | 询问 |
| `auto` | 直接改 | 安全模型判断为安全的直接运行，其余询问 |
| `full-access` | 直接改 | 直接运行，不询问 |

还有一个 `plan` 模式，只允许只读操作，智能体最后交一份计划。桌面端的计划模式用的就是它。

在 macOS（Seatbelt）和 Linux（bubblewrap）上，shell 命令在沙箱里运行。默认模式 `workspace-write` 只允许写工作目录、你配置的目录、临时目录和包缓存目录，其中 `.git` 只读。`~/.ssh`、`~/.aws`、`~/.lynshen/auth.json` 等路径在沙箱里不可读。需要离开沙箱的命令（例如写工作目录以外的文件）按审批模式处理。默认规则下，`git add` 和 `git commit` 可以直接离开沙箱，`git push` 总是先问你。Windows 目前没有沙箱，默认是 `full-access`。详见 [docs/sandbox.md](docs/sandbox.md)。

`full-access` 下，模型以你的用户权限运行命令和写文件，而且不受工作目录限制。只在你信任的仓库和任务上使用它。

## 无界面模式

```sh
lynshen --headless "列出仓库结构后停止"
cat task.md | lynshen --headless
lynshen --headless --approval-mode full-access "修复失败的测试并运行相关测试"
```

输出是一行一个 JSON 事件，最后一个是 `final_result`，包含状态、token 用量、工具调用次数和耗时。无界面模式默认用 `manual`：需要审批的操作会被自动拒绝，不会卡住等输入。要让它改文件或跑命令，显式传 `--approval-mode`。

## daemon、serve 和 acp

**`lynshen daemon`** 监听 `ws://127.0.0.1:7788`，客户端令牌在 `~/.lynshen/daemon/token`。它同时托管 LynShen、Claude Code、Codex 和 ACP 智能体的会话，把它们转成同一种事件流。客户端都断开后，会话继续运行。

```sh
lynshen daemon                   # 前台运行
lynshen daemon install           # 登录时自动启动（macOS 用 launchd，Linux 用 systemd 用户服务）
lynshen daemon relay on          # 打开中继连接（off 关闭，status 查看）
lynshen daemon pair              # 打印一次性配对链接，5 分钟内在手机上打开
```

手机通过中继 `wss://app.lynshen.org/relay/v1` 连接 daemon，内容端到端加密，中继看不到内容。中继默认关闭，可以在桌面端打开，也可以用上面的 `relay on`。协议见 [docs/daemon-protocol.md](docs/daemon-protocol.md) 和 [docs/relay-protocol.md](docs/relay-protocol.md)。

**`lynshen serve`** 从标准输入读命令、向标准输出写事件，都是一行一个 JSON，事件格式和 `--headless` 相同。见 [docs/serve-protocol.md](docs/serve-protocol.md)。

**`lynshen acp`** 把 LynShen 接到支持 ACP 的编辑器，例如 Zed。ACP 表达不了的功能（会话加载、对话树等）会明确拒绝。两种协议的区别见 [docs/serve-vs-acp.md](docs/serve-vs-acp.md)。

## 文件位置

所有用户数据在 `~/.lynshen/` 下：

| 路径 | 内容 |
| --- | --- |
| `config.json` | 设置：服务商、模型、审批模式、沙箱、MCP 服务器等 |
| `auth.json` | API key 和登录凭据。默认明文，`config.json` 里设 `"encrypt_secrets": true` 后加密，见 [docs/secrets.md](docs/secrets.md) |
| `prompt.txt` | 系统提示词，可以自己改。没改过的文件在升级后会换成新的默认提示词，改过的保持不变 |
| `sessions/` | 保存的会话，按项目分开 |
| `skills/` | 安装的技能 |
| `commands/` | 自定义斜杠命令 |
| `logs/lynshen.log` | 日志 |
| `daemon/` | 后台服务的状态、令牌、已配对设备和日志 |
| `agents/` | 长期 Agent 的角色、记忆和定时任务 |
| `bin/` | 桌面端管理的那份 CLI |

项目里的 `AGENTS.md` 或 `CLAUDE.md` 会作为项目说明发给模型。项目目录下的 `.lynshen/skills`、`.lynshen/commands` 等本地资源，要先用 `/trust yes` 信任这个项目才会加载。

一个简单的 `config.json` 片段：

```json
{
  "provider": "lynshen",
  "model": "gpt-5.5",
  "reasoning_effort": "medium",
  "approval_mode": "manual",
  "sandbox": "workspace-write",
  "auto_update": true
}
```

## 工具、技能和 MCP

模型能用的工具不多：`read`、`hashline_edit`（默认唯一开启的编辑工具）、`bash`、`ripgrep`、`ls`、`outline`、`checkpoint`、`web_fetch`、`generate_image`，以及子智能体相关的 `spawn_agent` 等。`str_replace`、`write`、`apply_patch` 默认关闭，用 `config.json` 的 `edit_tools` 打开。登录 LynShen 账号后还有 `web_search`。文件工具只访问工作目录，以及技能目录和 `sandbox_directories` 里配置的目录。

技能从 `~/.lynshen/skills`、`~/.agents/skills` 和已信任项目的 `.lynshen/skills`、`.agents/skills` 读取。在 `config.json` 里设 `"extra_skills_source": "anthropic"`，`/skills` 就能列出和安装 [anthropics/skills](https://github.com/anthropics/skills) 里的技能。详见 [docs/skills.md](docs/skills.md)。

MCP 服务器写在 `config.json` 的 `mcp_servers` 里，支持 stdio 和 HTTP。详见 [docs/mcp.md](docs/mcp.md)。

## 更新

`lynshen`（TUI）和 `lynshen serve` 启动时检查软件库。有新版本时，release 构建的二进制会在后台下载新版本替换自己，下次启动生效。你自己用 `cargo build --release` 编出来的二进制也会这样做；debug 构建只提示。版本清单用 Ed25519 签名，文件按 sha256 校验后才替换。`config.json` 里设 `"auto_update": false` 后只提示，不自动安装。手动更新：

```sh
lynshen update
```

桌面端管理的那份 CLI 随桌面端更新。

## 开发

```sh
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

在 Linux 上跑全部测试需要 `ripgrep` 和 `bubblewrap`，并且系统允许非特权用户命名空间（CI 里用 `sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`）。

代码结构：

- `src/main.rs`：命令行入口和子命令
- `crates/agent-core`：智能体循环、工具、会话、配置、沙箱、技能、MCP
- `crates/tui`：终端界面
- `crates/daemon`：后台服务
- `crates/llm-provider-kit`：git 子模块，各服务商的协议和模型目录
- `crates/software-release`：软件库的签名工具

## 发布

1. 改 `Cargo.toml` 里 `[workspace.package]` 的 `version`。
2. 写更新公告 `release-notes/<版本>.md`，中文，每行以「新增」「优化」或「修复」开头。
3. 推送 tag `v<版本>`。两个工作流并行运行：`Release CLI` 构建 Linux 和 Windows，`Release CLI (macOS)` 构建 Apple 芯片和 Intel 两种 Mac。它们检查二进制报告的版本和 tag 一致，然后把文件传到 GitHub Release。配置了 `NPM_TOKEN` 时还会发布 npm 包。
4. 在持有签名密钥的机器上运行 `deploy/software-library/publish.sh`，把这个版本发布到软件库。已安装的 CLI 从软件库更新。见 [docs/software-library.md](docs/software-library.md)。

桌面端固定使用某个 CLI 版本，升级 CLI 后要改桌面仓库里的 `src-tauri/lynshen-cli.ref` 和 `src-tauri/lynshen-cli.version`。

## 反馈问题

在 [GitHub Issues](https://github.com/Libra1337/LynShen-CLI/issues) 提交。请写上 `lynshen --version` 的输出、系统，以及 `~/.lynshen/logs/lynshen.log` 里相关的部分。桌面端用户也可以在应用的「设置 → 通用 → 反馈问题」里提交。

## 许可证

Apache License 2.0，见 [LICENSE](LICENSE) 和 [NOTICE](NOTICE)。Copyright 2026 LynShen Innovations INC.

`crates/llm-provider-kit` 包含来自 [oh-my-pi](https://github.com/can1357/oh-my-pi) 的代码和数据，使用 MIT 许可证，见[它的 NOTICE](crates/llm-provider-kit/NOTICE)。LynShen 名称和图标是 LynShen Innovations INC. 的商标，不授权给分支项目使用。

---

## English

LynShen CLI is a terminal coding agent written in Rust. The command is `lynshen`. It is also the engine behind [LynShen Desktop](https://github.com/Libra1337/LynShen_Agent): every Desktop session runs in `lynshen daemon`, which lives in this repository too.

**Install.** Download the binary for your platform from [GitHub Releases](https://github.com/Libra1337/LynShen-CLI/releases) (`lynshen-aarch64-apple-darwin`, `lynshen-x86_64-apple-darwin`, `lynshen-x86_64-unknown-linux-gnu`, `lynshen-x86_64-pc-windows-msvc.exe`), or build it with `cargo build --release` from a clone made with `--recurse-submodules`. LynShen Desktop ships its own copy in `~/.lynshen/bin`. The npm package `@lynshen/cli` is not on npm yet. On Linux, install `bubblewrap`: shell commands run in its sandbox and fail without it.

**First run.** Run `lynshen` in a project. Type `/login` to sign in to LynShen in the browser, or `/login <provider> <key>` to use your own key (`/login list` shows the providers). Settings are in `~/.lynshen/config.json`, credentials in `~/.lynshen/auth.json`.

**Modes.** `lynshen --headless "<task>"` runs one task and prints JSONL events. It denies approvals unless you pass `--approval-mode`. `lynshen serve` speaks newline JSON over stdio for GUIs, `lynshen acp` speaks ACP for editors such as Zed, and `lynshen daemon` hosts many sessions on `ws://127.0.0.1:7788` for Desktop and paired phones.

**Safety.** The approval modes are `manual` (default), `auto-edit`, `auto` (a safety model approves safe commands) and `full-access`. On macOS and Linux, shell commands run in an OS sandbox ([docs/sandbox.md](docs/sandbox.md)). Windows has no sandbox yet.

**Develop.** `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.

**Problems.** Open a [GitHub issue](https://github.com/Libra1337/LynShen-CLI/issues) with `lynshen --version`, your OS and the relevant part of `~/.lynshen/logs/lynshen.log`.

**License.** Apache License 2.0. The LynShen name and logo are trademarks of LynShen Innovations INC. and are not licensed for use by forks.
