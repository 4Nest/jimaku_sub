# Jimaku 字幕订阅通知服务

定时轮询 [jimaku.cc](https://jimaku.cc) 的新字幕，通过 Telegram Bot 推送通知，支持可选的自动下载。

## 功能特性

- 🔔 **新字幕通知** — 定时检查 jimaku.cc 新上传的字幕，通过 Telegram 推送富文本卡片（语言、大小、时间、链接）
- 📄 **一键发送文件** — 通知卡片带「⬇️ 下载」按钮，点击直接把字幕文件发到聊天
- ⬇️ **自动下载** — 可选自动下载字幕文件到指定目录
- 🎯 **精准订阅** — 支持按 Jimaku Entry ID、作品名和文件关键词过滤字幕
- 🤖 **Bot 交互** — Telegram 命令 + inline 按钮管理订阅（搜索结果点选、静音/退订按钮）
- 🎚️ **订阅级策略** — 每个订阅单独设置静音、自动下载（跟随全局/开/关）、通知后发送文件
- 🔒 **命令白名单** — 只允许配置的 `TELEGRAM_CHAT_ID` 执行 Bot 命令
- 🗄️ **状态持久化** — SQLite 存储已通知记录，重启不重复推送；通知发送失败自动下轮重试
- 🛡️ **稳定运行** — 优雅停机、崩溃自动重启、Docker 健康检查、Telegram 限流退避、日志按天轮转+定期清理
- 🐳 **Docker 部署** — 一键容器化运行
- 📡 **全量频道推送** — 可选把 jimaku 全站新字幕以文件形式直发频道，Caption 含日/英/罗马音作品名

## 全量频道推送

设置 `CHANNEL_ENABLED=true` + `CHANNEL_CHAT_ID` 后，除订阅通知外，jimaku 全站新字幕会**直接以文件发到指定频道**（不走订阅匹配）：

- 文件消息 Caption（MarkdownV2 装饰面板风）：✦ 日语主标题（点击跳 jimaku）+ 罗马音/英文副标题 + 等宽文件名 + 大小/时间/AniList 链接（罗马音来自 AniList，自动缓存）
- 超过 48MB 的文件改发带下载链接的文字卡片
- 只发功能开启后新出现的字幕，不回填历史；失败自动重试，重试耗尽发私聊告警

配置步骤：

1. 创建频道，把 bot 加为**管理员**（发布消息权限）
2. 获取频道数字 id：Telegram Web 打开频道，地址栏 `/#-100xxxxxxxxxx`；或转发频道消息给 @userinfobot
3. `.env` 加 `CHANNEL_ENABLED=true` 和 `CHANNEL_CHAT_ID=-100xxxxxxxxxx`，重启容器
4. 启动时频道会收到确认消息；收不到说明 bot 权限不足（服务会启动失败并提示）

## Telegram Bot 命令

| 命令 | 说明 |
|------|------|
| `/help` | 显示帮助信息 |
| `/status` | 查看服务状态、已通知数量、上次检查时间 |
| `/checknow` | 立即触发一次检查 |
| `/download <entry_id>` | 下载指定 Jimaku Entry 的全部字幕，跳过已下载文件 |
| `/sub <entry_id>` | 按 Jimaku Entry ID 添加订阅 |
| `/sub <作品名>` | 搜索 Jimaku，多个结果时用按钮点选订阅 |
| `/sub <作品名> -r NF\|Netflix\|ATX` | 只通知文件名匹配这些关键词的字幕 |
| `/unsub <作品名\|entry_id>` | 取消订阅 |
| `/listsubs` | 列出当前动态订阅（每个订阅带管理按钮面板） |

## 按钮交互

- **通知卡片**：「⬇️ 下载字幕文件」— 把字幕文件作为文档发送到聊天（本地已有直接发，否则现下载）
- **/sub 多结果**：候选作品列表按钮，点击即订阅，附带策略面板
- **/listsubs 面板**：`🔇/🔔 静音`、`📥 自动下载（跟随全局→开→关）`、`📄 发文件`、`❌ 退订（二次确认）`

## 快速开始

### 1. 获取 API Key

- **Jimaku API Key**: 登录 [jimaku.cc/account](https://jimaku.cc/account) 生成，只填 key 本身，不要加 `Bearer `
- **Telegram Bot Token**: 在 [@BotFather](https://t.me/botfather) 创建 Bot 获取
- **Telegram Chat ID**: 通过 [@userinfobot](https://t.me/userinfobot) 获取你的用户 ID

### 2. Docker Compose 部署

```bash
# 复制环境变量模板
cp .env.example .env
# 编辑 .env 填入你的配置
nano .env

# 启动服务
docker compose up -d
```

### 3. 环境变量配置

| 变量 | 必需 | 说明 |
|------|------|------|
| `JIMAKU_API_KEY` | ✅ | Jimaku API Key |
| `TELEGRAM_BOT_TOKEN` | ✅ | Telegram Bot Token |
| `TELEGRAM_CHAT_ID` | ✅ | 目标 Chat ID |
| `DOWNLOAD_ENABLED` | ❌ | 是否自动下载 (`true`/`false`) |
| `DOWNLOAD_PATH` | ❌ | 下载路径 (默认 `/app/downloads`) |
| `SCHEDULER_INTERVAL_SECONDS` | ❌ | 检查间隔秒数 (默认 `300`) |
| `DATABASE_URL` | ❌ | SQLite 路径 (默认 `sqlite://data/jimaku_subscriber.db`，compose 中指向 `/app/data`) |
| `LOG_RETENTION_DAYS` | ❌ | 日志保留天数 (默认 `30`) |
| `CHANNEL_ENABLED` | ❌ | 全量字幕频道推送 (`true`/`false`，默认 `false`) |
| `CHANNEL_CHAT_ID` | ❌ | 频道数字 id（`-100` 开头，启用频道推送时必需） |
| `RUST_LOG` | ❌ | 日志级别 (默认 `info,teloxide=warn`) |
| `SUBSCRIPTION_ANILIST_IDS` | ❌ | 订阅的 AniList ID，逗号分隔 |
| `SUBSCRIPTION_NAME_KEYWORDS` | ❌ | 订阅关键词，逗号分隔 |

### 4. 配置文件方式

也可以使用 `config.toml`（适合更复杂的配置）：

```toml
[jimaku]
api_key = "your_api_key"

[telegram]
bot_token = "your_bot_token"
chat_id = "your_chat_id"

[subscription]
anilist_ids = [16498, 1535]
name_keywords = ["Attack on Titan"]

[download]
enabled = true
download_path = "/app/downloads"

[scheduler]
interval_seconds = 300

[database]
url = "sqlite://data/jimaku_subscriber.db"

[logging]
dir = "./logs"
retention_days = 30
```

挂载到容器：
```yaml
volumes:
  - ./config.toml:/app/config.toml:ro
```

## 订阅逻辑

- 如果 `anilist_ids` 和 `name_keywords` 都为空 → **不匹配任何条目**（全量推送请用 `CHANNEL_ENABLED` 频道功能）
- 如果配置了过滤条件 → **只通知匹配的条目**
- 配置文件订阅 + 动态订阅（通过 `/sub` 命令）会合并生效
- 动态订阅的作品名和关键词会缓存到 SQLite，`/status` 和 `/listsubs` 不会重复请求外部接口
- 日志同时输出到 `docker logs` 和 `./logs/jimaku-subscriber.log.*`

## 目录结构

```
.
├── data/              # SQLite 数据库持久化
├── downloads/         # 字幕下载目录（如启用）
├── logs/              # 应用日志（按天轮转，自动清理）
├── src/
│   ├── main.rs        # 入口：配置、日志、组件装配、任务监督
│   ├── bot/
│   │   ├── mod.rs     # AppState 与公共逻辑
│   │   ├── commands.rs   # Bot 命令 handler
│   │   ├── callbacks.rs  # inline 按钮回调
│   │   ├── keyboards.rs  # 按钮面板与 callback_data 编解码
│   │   └── text.rs       # 消息模板
│   ├── config.rs      # 配置管理
│   ├── jimaku.rs      # Jimaku API 客户端
│   ├── telegram.rs    # Telegram 通知（限流）
│   ├── database.rs    # SQLite 存储
│   ├── scheduler.rs   # 定时轮询
│   └── downloader.rs  # 字幕下载
├── Dockerfile
├── docker-compose.yml
├── config.example.toml
└── .env.example
```

## 技术栈

- **Rust** + **Tokio** 异步运行时
- **reqwest** HTTP 客户端
- **teloxide** Telegram Bot 框架
- **sqlx** SQLite 异步 ORM
- **tracing** 结构化日志

## 自行编译

```bash
cargo build --release
# 二进制在 target/release/jimaku-subscriber
```

## License

MIT
