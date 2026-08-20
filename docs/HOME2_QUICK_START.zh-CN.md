# FlowSplice 0.2 第二 Home macOS 快速开始

## 先明确：第二 Home 不是另一套程序

`home-1` 和 `home-2` 使用同一个 `flowsplice-homeagent` 二进制。第二 Home 的独立性来自自己的 Home ID、双证书、状态库、授权缓存、UI 端口、服务目录和签发权限，而不是改名后的另一份程序。

macOS 包名为 `flowsplice-home2-0.2.0-macos-arm64.tar.gz`，包含：

```text
flowsplice-home2-0.2.0-macos-arm64/
├── bin/flowsplice-homeagent
├── config/homeagent-serving-only.toml
├── config/homeagent-issuer.toml
├── launchd/io.zxf.flowsplice.home2.plist
├── QUICK_START.zh-CN.md
└── SHA256SUMS
```

包内没有证书、私钥、密码、Server SPKI 或生产地址。二进制使用免费的 ad-hoc codesign，标识为 `io.zxf.flowsplice.homeagent`；它不是 Developer ID 签名，也没有 notarization。

## 选择运行模式

### Serving-only（生产默认建议）

选择 `homeagent-serving-only.toml`。它可以发布 Home 2 的业务、接收符合范围的 Travel 凭据并显示本地统计，但没有批准、签发和撤销入口，也不保存任何 CA 或授权签名私钥。

Travel 必须已经持有覆盖 `home-2` 的全局凭据或 Home-2 凭据，才能访问这个 Home。

### Home-2 issuer（必须单独批准 CA 托管）

选择 `homeagent-issuer.toml`。它可以在 Home 2 本地页面批准自己的 Home 范围或自己的指定业务范围，并使用本地签发密码撤销自己签发的凭据。

这个配置故意不包含全局授权私钥，因此不会显示“全局超级授权”。它需要 Home-2 authority 私钥，以及当前设计下的 Management/Business CA 私钥。把共享 CA 私钥复制到第二台机器会扩大私钥托管和故障域，不能因为包已经生成就默认执行；必须先明确批准生产 CA 托管方案。

## 启动前必须完成的控制面配置

仅复制 macOS 包不能创建一个可信的 Home 2。管理员需要先完成以下配套变更：

1. 为 `home-2` 签发独立的 Management 和 Business 叶证书，证书身份必须绑定 `home/home-2`，不能复制 Home 1 的叶证书和私钥。
2. 在新的 deployment trust generation 中加入 `home-2` 的 Management/Business SPKI。如果启用 issuer 模式，再加入只绑定 `home-2` 的 `home-2-authority` 公钥；不要自动加入 global authority。
3. 在 Server TOML 中加入：

   ```toml
   [[homes]]
   id = "home-2"
   ```

4. 把同一份新 deployment trust 分发给 Server、Relay、Home 和 Travel，并按 0.2 的整套协议版本重启相关控制连接。
5. 在需要访问 Home 2 的 Travel TOML 中加入：

   ```toml
   [[homes]]
   id = "home-2"

   [[mappings]]
   home_id = "home-2"
   service_id = "foobar"
   protocol = "tcp"
   bind = "127.0.0.1:10082"
   ```

6. 确认 Home 2 所在的 Mac 可以主动连接 Server control 地址，以及所有已批准 Relay 的公开 data 地址。Home 不开放公网业务入站端口，Server 也没有业务数据端口。

## 解包并验证

在 Apple Silicon Mac 上：

```bash
tar -xzf flowsplice-home2-0.2.0-macos-arm64.tar.gz
cd flowsplice-home2-0.2.0-macos-arm64
shasum -a 256 -c SHA256SUMS
codesign --verify --strict --verbose=2 ./bin/flowsplice-homeagent
codesign -dvvv ./bin/flowsplice-homeagent 2>&1 | grep 'Identifier=io.zxf.flowsplice.homeagent'
```

如果文件经浏览器下载后被 Gatekeeper 隔离，请在 macOS“系统设置 → 隐私与安全性”中明确允许这次运行。免费 ad-hoc 签名只能证明解包后的二进制仍与打包时一致，不能替代 Apple Developer ID 身份和 notarization。

## 安装目录

以下命令使用当前用户自己的目录，不需要 root：

```bash
home2_root="$HOME/Library/Application Support/FlowSplice/Home2"
mkdir -p "$home2_root/bin" "$home2_root/cert" "$home2_root/state" \
  "$home2_root/issuer" "$home2_root/logs"
install -m 755 ./bin/flowsplice-homeagent "$home2_root/bin/flowsplice-homeagent"
```

把部署管理员提供的文件放到以下位置：

```text
Home2/cert/home2-management.crt
Home2/cert/home2-management.key
Home2/cert/management-ca.crt
Home2/cert/home2-business.crt
Home2/cert/home2-business.key
Home2/cert/business-ca.crt
Home2/cert/deployment-root.pub
Home2/cert/deployment-trust.json
```

这些叶私钥必须属于 Home 2，不能复用 Home 1 的叶私钥。完成后收紧权限：

```bash
chmod 700 "$home2_root" "$home2_root/cert" "$home2_root/state" "$home2_root/issuer"
chmod 600 "$home2_root"/cert/*.key
```

issuer 模式还需要以下三把加密私钥；serving-only 模式不要复制它们：

```text
Home2/issuer/management-ca.key
Home2/issuer/business-ca.key
Home2/issuer/home2-authority.key
```

三把 issuer 私钥必须使用同一个至少 12 个字符的签发密码加密。密码只在 Home 2 本地批准或撤销时输入，不写进 TOML，也不发送给 Server、Relay 或 Travel。

## 生成 Home 2 TOML

Serving-only：

```bash
sed "s|__HOME2_ROOT__|$home2_root|g" \
  ./config/homeagent-serving-only.toml > "$home2_root/homeagent.toml"
```

或者，在已经明确批准 CA 托管后选择 issuer：

```bash
sed "s|__HOME2_ROOT__|$home2_root|g" \
  ./config/homeagent-issuer.toml > "$home2_root/homeagent.toml"
```

然后编辑 `$home2_root/homeagent.toml`，至少替换：

- `REPLACE_SERVER_HOST:7443`：Server control 地址；
- `REPLACE_SERVER_TLS_NAME`：Server 管理证书中的 TLS DNS 名；
- `REPLACE_WITH_SERVER_SPKI_SHA256_HEX`：Server Management 证书 SPKI SHA-256；
- `[[services]]`：Home 2 实际发布的业务及本机目标地址。

如果 Home 1 和 Home 2 在同一台 Mac 上，必须继续使用不同的 UI 端口、证书、授权缓存和 redb 文件。模板默认使用 `127.0.0.1:9082`。如果它们在不同机器上，Home 2 也可以改用 `127.0.0.1:9081`。

## 前台首次启动

先在终端前台运行，确认配置、证书和网络连接都正确：

```bash
RUST_LOG=flowsplice_homeagent=info \
  "$home2_root/bin/flowsplice-homeagent" --config "$home2_root/homeagent.toml"
```

成功后，本地页面位于 `http://127.0.0.1:9082/`。Serving-only 页面只有 Overview/Statistics；issuer 页面还会显示 Travel 请求、凭据签发与撤销。统计数据只有点击第二页 Statistics 后才读取。

在 Travel 端打开对应映射或运行 Foobar probe，确认日志中看到 Home 2 的 Catalog 上线和业务 Flow。相同 `service_id` 可以同时存在于两个 Home，但 Travel 的映射必须精确指定 `home_id = "home-2"`，系统不会跨 Home 回退。

## 使用 launchd 常驻

停止前台程序后生成当前用户的 launchd 文件：

```bash
mkdir -p "$HOME/Library/LaunchAgents"
sed "s|__HOME2_ROOT__|$home2_root|g" \
  ./launchd/io.zxf.flowsplice.home2.plist \
  > "$HOME/Library/LaunchAgents/io.zxf.flowsplice.home2.plist"
plutil -lint "$HOME/Library/LaunchAgents/io.zxf.flowsplice.home2.plist"
launchctl bootstrap "gui/$(id -u)" \
  "$HOME/Library/LaunchAgents/io.zxf.flowsplice.home2.plist"
launchctl kickstart -k "gui/$(id -u)/io.zxf.flowsplice.home2"
```

查看状态与日志：

```bash
launchctl print "gui/$(id -u)/io.zxf.flowsplice.home2"
tail -f "$home2_root/logs/home2.log" "$home2_root/logs/home2-error.log"
```

停止并移除自动启动：

```bash
launchctl bootout "gui/$(id -u)/io.zxf.flowsplice.home2"
```

升级时先停止 launchd，备份整个 `Home2` 目录，替换二进制后验证 codesign，再重新 bootstrap。不要删除 `state/home2-state.redb` 或 `state/authorization-cache.json`，否则会丢失本地统计、签发收件箱和授权高水位状态。

## 验收清单

- Home 2 使用自己的双证书、私钥、状态文件和 Home ID。
- Server Catalog 同时出现 `home-1` 与 `home-2`。
- Travel 访问 Home 2 的映射命中正确服务，不会回退到 Home 1。
- Home 2 停止时只影响 Home 2；Home 1 业务保持可用。
- 已建立业务流在 Server 停止时保持；Relay 故障后同一个 TCP Flow 可以换 Relay。
- Serving-only 没有批准、签发或撤销入口。
- Issuer 模式只能签 Home 2 或 Home 2 指定业务；没有全局授权入口。
- 错误签发密码不会签发或撤销；正确撤销立即关闭活动凭据且重启后不复活。
- 默认页面不读取统计；点击 Statistics 后显示 Home 2 自己的五分钟及日/周/月/年滚动报表。
