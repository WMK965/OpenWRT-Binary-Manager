# OpenWrt Binary Update Manager

自动从 GitHub Releases 检测并更新 OpenWrt 上的二进制程序。

## 功能特性

- **自动检测更新**：定期检查 GitHub Releases，支持 `latest` 和 `pre-release`
- **正则匹配**：通过正则表达式精确匹配目标 release asset
- **代理支持**：HTTP 镜像前缀（如 `gh-proxy.com`）和 SOCKS5 代理
- **存档解压**：自动处理 `.zip` / `.tar.gz` / `.tar.xz` 格式，支持指定提取路径
- **前后脚本**：替换前后可执行自定义 shell 命令（如停止/重启服务）
- **文件备份**：更新前自动备份旧版本，打包为 zip 防止意外执行，支持轮转保留
- **GitHub Token**：支持配置 PAT 以提高 API 速率限制
- **双运行模式**：默认单次运行（配合 cron），可选守护进程模式

## 安装

### 本地编译

```bash
cargo build --release
```

### 交叉编译（OpenWrt x86_64）

```bash
# 安装 musl 工具链
rustup target add x86_64-unknown-linux-musl

# 编译
cargo build --release --target x86_64-unknown-linux-musl

# 产物位于
# target/x86_64-unknown-linux-musl/release/openwrt-binary-manager
```

### 配置字段说明

| 字段 | 说明 |
|---|---|
| `file` | 目标二进制文件路径 |
| `interval` | 检查间隔，支持 `s`/`m`/`h`/`d` |
| `proxy` | HTTP 镜像前缀或 `socks5://` 代理地址 |
| `regex` | 匹配 release asset 文件名的正则表达式 |
| `repo` | GitHub 仓库，格式 `owner/repo` |
| `type` | `latest`（正式版）或 `pre-release`（预发布） |
| `language` | 全局语言 `en_us` / `zh_cn`，留空自动检测系统环境（可选） |
| `extract_path` | 存档内要提取的文件路径，支持 `{tag}` / `{version}` 变量（可选） |
| `pre_update` | 替换前执行的 shell 脚本，支持多行（可选） |
| `post_update` | 替换后执行的 shell 脚本，支持多行（可选） |
| `backup` | 全局备份目录配置：`dir`（备份根目录），可选 |
| `monitors.<name>.backup` | 保留历史备份份数（如 `3`），需全局 backup 已配置，不填则不备份 |
| `monitors.<name>.failsafe` | 故障保护：`true`（默认）/ `false` / `allow_post` |
| `version_check.command` | 获取本地版本号的命令（可选） |
| `version_check.regex` | 提取版本号的正则，需包含一个捕获组（可选） |
| `version_check.strip_prefix` | 比较前去除远程 tag 的前缀，如 `release-`（可选） |
| `concurrency` | 并发检查数（默认 4） |
| `timeout` | API 请求超时秒数（默认 30） |
| `download_timeout` | 下载超时秒数（默认 600） |
| `retry` | 请求失败重试次数（默认 2） |

### 多行脚本

`pre_update` 和 `post_update` 支持多行脚本，使用 YAML 的 `|`（literal block）语法：

```yaml
pre_update: |          # 注意 | 后换行，内容缩进
  /etc/init.d/qbittorrent stop
  sleep 2
  killall qbittorrent-nox || true
  echo "stopped"
```

也可以写成单行：

```yaml
pre_update: "/etc/init.d/qbittorrent stop"
```

> **注意**：`pre_update` 脚本执行失败（非零退出码）会中止本次更新；`post_update` 失败只记录警告，不影响更新结果。

### 故障保护（Failsafe）

**默认启用**，除非显式设置 `failsafe: false`。需配置全局 `backup.dir`。

更新流程：

1. 执行 `pre_update` 停止服务
2. **保存故障保护副本** → `{backup.dir}/{monitor}/failsafe/`（原二进制直接复制）
3. （可选）创建历史备份 → `{backup.dir}/{monitor}/{文件}_时间戳.zip`
4. 替换二进制文件
5. **校验新二进制**：执行 `version_check.command` 检测版本号
6. **校验失败** → 自动恢复故障保护副本
   - `failsafe: allow_post` → 恢复后仍执行 `post_update` 重启服务
   - 默认 → 直接中止（服务可能仍处于停止状态）
7. **校验通过** → 清除故障保护副本 → 执行 `post_update` 重启服务

`failsafe` 取值：
- `true`（默认）：正常故障保护
- `false`：完全关闭
- `allow_post`：恢复原文件后依然执行 post_update 脚本

### 备份目录结构

```
{backup.dir}/
├── qBittorrent-ee/
│   ├── failsafe/
│   │   └── qBittorrent-nox          # 故障保护副本（替换前的最新版本）
│   ├── qBittorrent-nox_20260531_0800.zip
│   └── qBittorrent-nox_20260530_1200.zip
├── sing-box/
│   ├── failsafe/
│   │   └── sing-box
│   └── sing-box_20260531_0801.zip
└── ...
```

## 使用

```bash
# 单次运行，检查并更新
openwrt-binary-manager upgrade /etc/updater/config.yaml

# 守护进程模式（每 60 秒检查一轮）
openwrt-binary-manager daemon /etc/updater/config.yaml

# 自定义守护进程间隔（每 300 秒）
openwrt-binary-manager daemon /etc/updater/config.yaml --interval 300

# 检测模式，仅报告可用更新，不执行任何更改
openwrt-binary-manager check /etc/updater/config.yaml
```

### 配合 cron 使用

```bash
# 每小时检查一次
0 * * * * /usr/bin/openwrt-binary-manager upgrade /etc/updater/config.yaml
```

### procd 服务（OpenWrt）

创建 `/etc/init.d/binary-updater`：

```sh
#!/bin/sh /etc/rc.common

START=99
USE_PROCD=1

start_service() {
    procd_open_instance
    procd_set_param command /usr/bin/openwrt-binary-manager daemon /etc/updater/config.yaml
    procd_set_param respawn
    procd_close_instance
}
```

```bash
chmod +x /etc/init.d/binary-updater
/etc/init.d/binary-updater enable
/etc/init.d/binary-updater start
```
